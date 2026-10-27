use crate::application::production_mutation_capability::{
    CommittedConditionalTitleUpdateFacts, ConditionalTitleUpdateEvidence,
    ConditionalTitleUpdateTarget,
};
use crate::domain::account::{
    AccountProfile, ProviderKind, RegisterAccountProfile, validate_account_id,
};
use crate::domain::calendar_alarm::notification_fires_for_occurrence;
use crate::domain::calendar_event::{
    CalendarAgendaItem, CalendarFixedRecurrence, CalendarSettingsView, CreateCalendarSubtask,
    NormalizedCalendarEvent, PendingRemoteAgendaKind, PendingRemoteAgendaState,
    PendingRemoteAgendaStatus, ProjectionStatus, UnsupportedReason, UpdateCalendarLocalMetadata,
    UpdateCalendarSubtask, normalize_vevent,
};
use crate::domain::calendar_recurrence::{
    ExpansionWindow, RecurrenceError, RecurrenceHorizon, expand_vevent_recurrence,
};
use crate::domain::calendar_sync::{
    AccountDeletionReconciliation, AccountIncrementalStage, AccountInventoryStage,
    CachedCalendarResource, IncrementalSyncRequest, ParseStatus, ResourceLifecycle,
    SyncFailureClass, SyncOutcome, SyncRunSummary, SyncTrigger,
};
use crate::domain::local_recurrence::{GeneratedOccurrence, generate_occurrences};
use crate::domain::local_todo::{
    CreateSubtask, FixedRecurrence, LocalCalendar, LocalTodo, Priority, RecurrenceEndKind,
    RecurrenceFrequency, RecurrenceOptions, SINGLETON_OCCURRENCE_KEY, SaveLocalCalendar,
    SaveLocalTodo, Subtask, Tag, UpdateLocalTodo, UpdateSubtask, fixed_occurrence_key,
    normalize_tag_name,
};
use crate::domain::remote_creation::{
    QueueRemoteEventCreate, QueueRemoteEventCreateV2, RemoteCreationAttemptEvidence,
    RemoteCreationClaim, RemoteCreationSummary,
};
use crate::domain::remote_event_update::{
    QueueRemoteEventUpdateV3, RemoteEventMutationClaimV3, RemoteEventUpdateSnapshotV3,
};
use crate::domain::remote_mutation::{
    ConflictResolutionChoice, ConflictResolutionResult, ConflictResolutionState,
    QueueRemoteEventDelete, QueueRemoteEventTextUpdateV2, QueueRemoteTitleUpdate,
    RemoteConflictSummary, RemoteDeletionClaim, RemoteDeletionReadBack, RemoteFieldsMutationClaim,
    RemoteFieldsSendSnapshot, RemoteMutationClaim, RemoteMutationExecutionPhase,
    RemoteMutationState, RemoteMutationSummary, RemoteReadBack, collection_routes_match,
    patch_single_vevent_title, resource_belongs_to_collection, single_vevent_title,
    single_vevent_uid, strong_mutation_etag, validate_title,
};
use crate::domain::settings::{AppSettings, UpdateAppSettings, WindowMode};
use crate::domain::window_placement::WindowPlacement;
use crate::ports::clock::Clock;
use rusqlite::{Connection, MAIN_DB, OptionalExtension, Transaction, params};
use rusqlite_migration::{M, Migrations};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use uuid::Uuid;
#[allow(dead_code)] // Execution-free preview; production first-route source is absent.
pub(crate) mod probe_first_route;
#[allow(dead_code)]
pub(crate) mod probe_ledger;
mod probe_seed;
#[allow(dead_code)]
pub(crate) mod probe_target;
mod remote_creation;
mod remote_crud;
mod remote_update_event;
mod remote_update_fields;
mod remote_update_successor;
#[allow(dead_code)]
pub(crate) mod scoped_event;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCalendarRecord {
    pub calendar_id: String,
    pub collection_url: String,
    pub display_name: String,
    pub server_color: Option<String>,
    pub privileges: String,
}

const MIGRATION_001: &str = r#"
CREATE TABLE local_todos (
    todo_id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL CHECK(length(trim(title)) BETWEEN 1 AND 512),
    notes TEXT,
    location TEXT,
    due_at_utc_ms INTEGER,
    start_at_utc_ms INTEGER,
    priority INTEGER NOT NULL CHECK(priority BETWEEN 0 AND 2),
    important INTEGER NOT NULL CHECK(important IN (0, 1)),
    recurrence_rule TEXT,
    active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE work_item_state (
    work_item_id TEXT PRIMARY KEY NOT NULL,
    source_kind TEXT NOT NULL CHECK(source_kind IN ('calendar_occurrence', 'local_todo_occurrence')),
    calendar_series_id TEXT,
    recurrence_key TEXT,
    local_todo_id TEXT REFERENCES local_todos(todo_id),
    local_occurrence_key TEXT,
    completed INTEGER NOT NULL DEFAULT 0 CHECK(completed IN (0, 1)),
    completed_at_ms INTEGER,
    important_override INTEGER CHECK(important_override IS NULL OR important_override IN (0, 1)),
    remote_deleted_at_ms INTEGER,
    last_seen_at_ms INTEGER NOT NULL,
    CHECK (
        (source_kind = 'calendar_occurrence' AND calendar_series_id IS NOT NULL AND recurrence_key IS NOT NULL AND local_todo_id IS NULL AND local_occurrence_key IS NULL)
        OR
        (source_kind = 'local_todo_occurrence' AND calendar_series_id IS NULL AND recurrence_key IS NULL AND local_todo_id IS NOT NULL AND local_occurrence_key IS NOT NULL)
    )
) STRICT;

CREATE UNIQUE INDEX work_item_calendar_source ON work_item_state(calendar_series_id, recurrence_key) WHERE source_kind = 'calendar_occurrence';
CREATE UNIQUE INDEX work_item_local_todo_source ON work_item_state(local_todo_id, local_occurrence_key) WHERE source_kind = 'local_todo_occurrence';
CREATE INDEX local_todos_agenda_order ON local_todos(active, important DESC, priority DESC, created_at_ms, todo_id);
"#;

const MIGRATION_002: &str = r#"
CREATE TRIGGER local_todos_valid_time_insert
BEFORE INSERT ON local_todos
WHEN NEW.start_at_utc_ms IS NOT NULL AND NEW.due_at_utc_ms IS NOT NULL AND NEW.start_at_utc_ms > NEW.due_at_utc_ms
BEGIN
    SELECT RAISE(ABORT, 'invalid local todo time range');
END;

CREATE TRIGGER local_todos_valid_time_update
BEFORE UPDATE OF start_at_utc_ms, due_at_utc_ms ON local_todos
WHEN NEW.start_at_utc_ms IS NOT NULL AND NEW.due_at_utc_ms IS NOT NULL AND NEW.start_at_utc_ms > NEW.due_at_utc_ms
BEGIN
    SELECT RAISE(ABORT, 'invalid local todo time range');
END;

CREATE TABLE tags (
    tag_id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL CHECK(length(trim(name)) BETWEEN 1 AND 64),
    normalized_name TEXT NOT NULL UNIQUE CHECK(length(normalized_name) BETWEEN 1 AND 64),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE local_todo_tags (
    local_todo_id TEXT NOT NULL REFERENCES local_todos(todo_id) ON DELETE CASCADE,
    tag_id TEXT NOT NULL REFERENCES tags(tag_id) ON DELETE RESTRICT,
    sort_order INTEGER NOT NULL CHECK(sort_order >= 0),
    PRIMARY KEY (local_todo_id, tag_id),
    UNIQUE (local_todo_id, sort_order)
) STRICT;
CREATE INDEX local_todo_tags_by_tag ON local_todo_tags(tag_id, local_todo_id);

CREATE TABLE subtasks (
    subtask_id TEXT PRIMARY KEY NOT NULL,
    work_item_id TEXT NOT NULL REFERENCES work_item_state(work_item_id) ON DELETE CASCADE,
    text TEXT NOT NULL CHECK(length(trim(text)) BETWEEN 1 AND 512),
    completed INTEGER NOT NULL DEFAULT 0 CHECK(completed IN (0, 1)),
    sort_order INTEGER NOT NULL CHECK(sort_order >= 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (work_item_id, sort_order)
) STRICT;
CREATE INDEX subtasks_by_work_item ON subtasks(work_item_id, sort_order, subtask_id);

CREATE INDEX local_todos_agenda_time_order
    ON local_todos(active, important DESC, priority DESC, due_at_utc_ms, start_at_utc_ms, created_at_ms, todo_id);
"#;

const MIGRATION_003: &str = r#"
CREATE TABLE local_todo_recurrence (
    local_todo_id TEXT PRIMARY KEY NOT NULL REFERENCES local_todos(todo_id) ON DELETE CASCADE,
    series_id TEXT NOT NULL UNIQUE,
    frequency TEXT NOT NULL CHECK(frequency IN ('daily', 'weekly')),
    interval_value INTEGER NOT NULL CHECK(interval_value BETWEEN 1 AND 365),
    anchor_due_at_utc_ms INTEGER NOT NULL,
    start_offset_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE local_todo_occurrences (
    occurrence_id TEXT PRIMARY KEY NOT NULL,
    local_todo_id TEXT NOT NULL REFERENCES local_todos(todo_id) ON DELETE CASCADE,
    series_id TEXT NOT NULL REFERENCES local_todo_recurrence(series_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK(sequence >= 0),
    occurrence_key TEXT NOT NULL,
    nominal_start_at_utc_ms INTEGER,
    nominal_due_at_utc_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    UNIQUE(local_todo_id, occurrence_key),
    UNIQUE(series_id, sequence)
) STRICT;
CREATE INDEX local_todo_occurrences_due ON local_todo_occurrences(local_todo_id, nominal_due_at_utc_ms);

"#;

const MIGRATION_004: &str = r#"
CREATE TABLE app_settings (
    singleton_id INTEGER PRIMARY KEY NOT NULL CHECK(singleton_id = 1),
    window_mode TEXT NOT NULL CHECK(window_mode IN ('desktop_companion', 'always_on_top', 'normal_window')),
    sync_interval_minutes INTEGER NOT NULL CHECK(sync_interval_minutes BETWEEN 5 AND 1440),
    sync_on_startup INTEGER NOT NULL CHECK(sync_on_startup IN (0, 1)),
    updated_at_ms INTEGER NOT NULL
) STRICT;
INSERT INTO app_settings(singleton_id, window_mode, sync_interval_minutes, sync_on_startup, updated_at_ms)
VALUES(1, 'desktop_companion', 15, 1, 0);
"#;

const MIGRATION_005: &str = r#"
CREATE TABLE accounts (
    account_id TEXT PRIMARY KEY NOT NULL,
    provider_kind TEXT NOT NULL,
    user_label TEXT NOT NULL,
    credential_ref TEXT NOT NULL UNIQUE,
    region_profile TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE calendars (
    calendar_id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    collection_url TEXT NOT NULL,
    display_name TEXT NOT NULL,
    server_color TEXT,
    privileges TEXT NOT NULL,
    available INTEGER NOT NULL DEFAULT 1 CHECK(available IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(account_id, collection_url)
) STRICT;
CREATE INDEX calendars_by_account ON calendars(account_id, available, calendar_id);

CREATE TABLE calendar_sync_state (
    calendar_id TEXT PRIMARY KEY NOT NULL REFERENCES calendars(calendar_id) ON DELETE CASCADE,
    committed_sync_token TEXT,
    last_complete_at_ms INTEGER,
    last_success_at_ms INTEGER,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE event_series (
    series_id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    uid TEXT NOT NULL,
    current_calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(account_id, uid)
) STRICT;

CREATE TABLE remote_resources (
    resource_id TEXT PRIMARY KEY NOT NULL,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE CASCADE,
    series_id TEXT NOT NULL REFERENCES event_series(series_id) ON DELETE RESTRICT,
    exact_href TEXT NOT NULL,
    etag TEXT NOT NULL,
    content_type TEXT NOT NULL,
    raw_ics BLOB NOT NULL CHECK(length(raw_ics) > 0),
    parse_status TEXT NOT NULL CHECK(parse_status IN ('parsed', 'read_only')),
    lifecycle TEXT NOT NULL CHECK(lifecycle IN ('active', 'missing_candidate')),
    last_seen_at_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(calendar_id, exact_href)
) STRICT;
CREATE INDEX remote_resources_by_series ON remote_resources(series_id, lifecycle, resource_id);
CREATE INDEX remote_resources_by_calendar ON remote_resources(calendar_id, lifecycle, exact_href);

CREATE TABLE identity_aliases (
    series_id TEXT NOT NULL REFERENCES event_series(series_id) ON DELETE CASCADE,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE CASCADE,
    exact_href TEXT NOT NULL,
    first_observed_at_ms INTEGER NOT NULL,
    last_observed_at_ms INTEGER NOT NULL,
    PRIMARY KEY(series_id, calendar_id, exact_href)
) STRICT;
CREATE INDEX identity_aliases_by_href ON identity_aliases(calendar_id, exact_href, series_id);
"#;

const MIGRATION_006: &str = r#"
CREATE TABLE sync_runs (
    run_id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL,
    trigger_kind TEXT NOT NULL CHECK(trigger_kind IN ('startup', 'manual', 'periodic', 'retry')),
    outcome TEXT NOT NULL CHECK(outcome IN ('success', 'failure')),
    failure_class TEXT NOT NULL CHECK(failure_class IN ('none', 'transient', 'unauthorized', 'incomplete', 'unsafe_data', 'validation', 'persistence')),
    calendar_count INTEGER NOT NULL CHECK(calendar_count >= 0),
    resource_count INTEGER NOT NULL CHECK(resource_count >= 0),
    started_at_ms INTEGER NOT NULL,
    finished_at_ms INTEGER NOT NULL CHECK(finished_at_ms >= started_at_ms)
) STRICT;
CREATE INDEX sync_runs_by_account ON sync_runs(account_id, started_at_ms DESC, run_id);
"#;

const MIGRATION_007: &str = r#"
DROP INDEX remote_resources_by_series;
DROP INDEX remote_resources_by_calendar;
ALTER TABLE remote_resources RENAME TO remote_resources_v6;

CREATE TABLE remote_resources (
    resource_id TEXT PRIMARY KEY NOT NULL,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE CASCADE,
    series_id TEXT NOT NULL REFERENCES event_series(series_id) ON DELETE RESTRICT,
    exact_href TEXT NOT NULL,
    etag TEXT NOT NULL,
    content_type TEXT NOT NULL,
    raw_ics BLOB NOT NULL CHECK(length(raw_ics) > 0),
    parse_status TEXT NOT NULL CHECK(parse_status IN ('parsed', 'read_only')),
    lifecycle TEXT NOT NULL CHECK(lifecycle IN ('active', 'missing_candidate', 'remote_deleted')),
    last_seen_at_ms INTEGER NOT NULL,
    remote_deleted_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK((lifecycle = 'remote_deleted' AND remote_deleted_at_ms IS NOT NULL) OR
          (lifecycle != 'remote_deleted' AND remote_deleted_at_ms IS NULL)),
    UNIQUE(calendar_id, exact_href)
) STRICT;
INSERT INTO remote_resources(
    resource_id,calendar_id,series_id,exact_href,etag,content_type,raw_ics,parse_status,
    lifecycle,last_seen_at_ms,remote_deleted_at_ms,created_at_ms,updated_at_ms
)
SELECT resource_id,calendar_id,series_id,exact_href,etag,content_type,raw_ics,parse_status,
       lifecycle,last_seen_at_ms,NULL,created_at_ms,updated_at_ms
FROM remote_resources_v6;
DROP TABLE remote_resources_v6;
CREATE INDEX remote_resources_by_series ON remote_resources(series_id, lifecycle, resource_id);
CREATE INDEX remote_resources_by_calendar ON remote_resources(calendar_id, lifecycle, exact_href);

CREATE TABLE deletion_evidence (
    evidence_id TEXT PRIMARY KEY NOT NULL,
    resource_id TEXT NOT NULL REFERENCES remote_resources(resource_id) ON DELETE CASCADE,
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE CASCADE,
    probe_status TEXT NOT NULL CHECK(probe_status IN ('found', 'not_found', 'gone', 'unauthorized', 'transient_failure', 'server_failure')),
    replacement_search_complete INTEGER NOT NULL CHECK(replacement_search_complete = 1),
    replacement_found INTEGER NOT NULL CHECK(replacement_found IN (0, 1)),
    account_barrier_complete INTEGER NOT NULL CHECK(account_barrier_complete = 1),
    decision TEXT NOT NULL CHECK(decision IN ('confirmed_deleted', 'retained')),
    observed_at_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    UNIQUE(resource_id, observed_at_ms)
) STRICT;
CREATE INDEX deletion_evidence_by_resource ON deletion_evidence(resource_id, observed_at_ms DESC);
CREATE INDEX deletion_evidence_by_account ON deletion_evidence(account_id, observed_at_ms DESC);
"#;

const MIGRATION_008: &str = r#"
CREATE TABLE calendar_event_projections (
    series_id TEXT PRIMARY KEY NOT NULL REFERENCES event_series(series_id) ON DELETE CASCADE,
    resource_id TEXT NOT NULL UNIQUE REFERENCES remote_resources(resource_id) ON DELETE CASCADE,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE CASCADE,
    title TEXT NOT NULL CHECK(length(trim(title)) BETWEEN 1 AND 512),
    notes TEXT,
    location TEXT,
    start_at_utc_ms INTEGER,
    end_at_utc_ms INTEGER,
    all_day INTEGER NOT NULL CHECK(all_day IN (0, 1)),
    cancelled INTEGER NOT NULL CHECK(cancelled IN (0, 1)),
    projection_status TEXT NOT NULL CHECK(projection_status IN ('ready', 'unsupported')),
    unsupported_reason TEXT CHECK(unsupported_reason IS NULL OR unsupported_reason IN ('invalid_structure', 'unsupported_recurrence', 'unsupported_time', 'invalid_data')),
    source_etag TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK((projection_status = 'ready' AND unsupported_reason IS NULL AND start_at_utc_ms IS NOT NULL AND end_at_utc_ms IS NOT NULL) OR
          (projection_status = 'unsupported' AND unsupported_reason IS NOT NULL AND start_at_utc_ms IS NULL AND end_at_utc_ms IS NULL)),
    CHECK(start_at_utc_ms IS NULL OR end_at_utc_ms >= start_at_utc_ms)
) STRICT;
CREATE INDEX calendar_event_projection_agenda
    ON calendar_event_projections(cancelled, projection_status, start_at_utc_ms, end_at_utc_ms, series_id);
CREATE INDEX calendar_event_projection_calendar ON calendar_event_projections(calendar_id, series_id);
"#;

const MIGRATION_009: &str = r#"
CREATE TRIGGER work_item_calendar_series_insert
BEFORE INSERT ON work_item_state
WHEN NEW.source_kind='calendar_occurrence'
 AND NOT EXISTS(SELECT 1 FROM event_series WHERE series_id=NEW.calendar_series_id)
BEGIN
    SELECT RAISE(ABORT, 'invalid calendar work item series');
END;

CREATE TRIGGER work_item_calendar_series_update
BEFORE UPDATE OF source_kind, calendar_series_id ON work_item_state
WHEN NEW.source_kind='calendar_occurrence'
 AND NOT EXISTS(SELECT 1 FROM event_series WHERE series_id=NEW.calendar_series_id)
BEGIN
    SELECT RAISE(ABORT, 'invalid calendar work item series');
END;

CREATE TRIGGER event_series_preserve_local_state
BEFORE DELETE ON event_series
WHEN EXISTS(
    SELECT 1 FROM work_item_state
    WHERE source_kind='calendar_occurrence' AND calendar_series_id=OLD.series_id
)
BEGIN
    SELECT RAISE(ABORT, 'calendar series has local state');
END;
"#;

const MIGRATION_010: &str = r#"
ALTER TABLE calendar_event_projections RENAME TO calendar_event_projections_v9;
DROP INDEX calendar_event_projection_agenda;
DROP INDEX calendar_event_projection_calendar;

CREATE TABLE calendar_event_projections (
    series_id TEXT NOT NULL REFERENCES event_series(series_id) ON DELETE CASCADE,
    recurrence_key TEXT NOT NULL CHECK(length(recurrence_key) BETWEEN 1 AND 255),
    resource_id TEXT NOT NULL REFERENCES remote_resources(resource_id) ON DELETE CASCADE,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE CASCADE,
    title TEXT NOT NULL CHECK(length(trim(title)) BETWEEN 1 AND 512),
    notes TEXT,
    location TEXT,
    start_at_utc_ms INTEGER,
    end_at_utc_ms INTEGER,
    all_day INTEGER NOT NULL CHECK(all_day IN (0, 1)),
    cancelled INTEGER NOT NULL CHECK(cancelled IN (0, 1)),
    projection_status TEXT NOT NULL CHECK(projection_status IN ('ready', 'unsupported')),
    unsupported_reason TEXT CHECK(unsupported_reason IS NULL OR unsupported_reason IN ('invalid_structure', 'unsupported_recurrence', 'unsupported_time', 'invalid_data')),
    source_etag TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY(series_id, recurrence_key),
    CHECK((projection_status = 'ready' AND unsupported_reason IS NULL AND start_at_utc_ms IS NOT NULL AND end_at_utc_ms IS NOT NULL) OR
          (projection_status = 'unsupported' AND unsupported_reason IS NOT NULL AND start_at_utc_ms IS NULL AND end_at_utc_ms IS NULL)),
    CHECK(start_at_utc_ms IS NULL OR end_at_utc_ms >= start_at_utc_ms)
) STRICT;
INSERT INTO calendar_event_projections(
    series_id,recurrence_key,resource_id,calendar_id,title,notes,location,start_at_utc_ms,end_at_utc_ms,
    all_day,cancelled,projection_status,unsupported_reason,source_etag,updated_at_ms
)
SELECT series_id,'SINGLETON',resource_id,calendar_id,title,notes,location,start_at_utc_ms,end_at_utc_ms,
       all_day,cancelled,projection_status,unsupported_reason,source_etag,updated_at_ms
FROM calendar_event_projections_v9;
DROP TABLE calendar_event_projections_v9;
CREATE INDEX calendar_event_projection_agenda
    ON calendar_event_projections(cancelled, projection_status, start_at_utc_ms, end_at_utc_ms, series_id, recurrence_key);
CREATE INDEX calendar_event_projection_calendar ON calendar_event_projections(calendar_id, series_id, recurrence_key);
"#;

const MIGRATION_011: &str = r#"
ALTER TABLE accounts ADD COLUMN credential_configured INTEGER NOT NULL DEFAULT 0 CHECK(credential_configured IN (0, 1));
"#;

const MIGRATION_012: &str = r#"
CREATE TABLE notification_jobs (
    job_id TEXT PRIMARY KEY NOT NULL,
    source_kind TEXT NOT NULL CHECK(source_kind IN ('local_todo', 'calendar_occurrence')),
    source_id TEXT NOT NULL,
    occurrence_key TEXT NOT NULL,
    revision TEXT NOT NULL,
    fire_at_utc_ms INTEGER NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('planned', 'claimed', 'delivered', 'cancelled', 'missed', 'failed')),
    claimed_at_utc_ms INTEGER,
    finished_at_utc_ms INTEGER,
    UNIQUE(source_kind, source_id, occurrence_key, revision, fire_at_utc_ms)
) STRICT;
CREATE INDEX notification_jobs_due ON notification_jobs(state, fire_at_utc_ms, job_id);
"#;

const MIGRATION_013: &str = r#"
ALTER TABLE app_settings ADD COLUMN desktop_list_mode INTEGER NOT NULL DEFAULT 0 CHECK(desktop_list_mode IN (0, 1));
ALTER TABLE app_settings ADD COLUMN desktop_opacity_percent INTEGER NOT NULL DEFAULT 82 CHECK(desktop_opacity_percent BETWEEN 20 AND 100);
"#;

const MIGRATION_014: &str = r#"
ALTER TABLE accounts ADD COLUMN removed_at_ms INTEGER;
CREATE INDEX accounts_active ON accounts(removed_at_ms, account_id);
"#;

const MIGRATION_015: &str = r#"
CREATE TABLE desktop_item_order (
    item_key TEXT PRIMARY KEY NOT NULL CHECK(length(item_key) BETWEEN 1 AND 512),
    sort_order INTEGER NOT NULL UNIQUE CHECK(sort_order >= 0)
) STRICT;
"#;

const MIGRATION_016: &str = r#"
ALTER TABLE app_settings ADD COLUMN start_hidden_in_tray INTEGER NOT NULL DEFAULT 0 CHECK(start_hidden_in_tray IN (0, 1));

CREATE TABLE window_placements (
    window_label TEXT PRIMARY KEY NOT NULL CHECK(window_label IN ('main', 'desktop')),
    monitor_name TEXT,
    x_dip REAL NOT NULL,
    y_dip REAL NOT NULL,
    width_dip REAL NOT NULL CHECK(width_dip > 0.0),
    height_dip REAL NOT NULL CHECK(height_dip > 0.0),
    maximized INTEGER NOT NULL DEFAULT 0 CHECK(maximized IN (0, 1)),
    updated_at_ms INTEGER NOT NULL
) STRICT;
"#;

/// Removes the `normal_window` desktop-list mode. Existing rows move to the
/// default Companion mode, and the rebuilt table rejects the removed value so a
/// stale constraint cannot keep it alive.
const MIGRATION_017: &str = r#"
CREATE TABLE app_settings_v17 (
    singleton_id INTEGER PRIMARY KEY NOT NULL CHECK(singleton_id = 1),
    window_mode TEXT NOT NULL CHECK(window_mode IN ('desktop_companion', 'always_on_top')),
    sync_interval_minutes INTEGER NOT NULL CHECK(sync_interval_minutes BETWEEN 5 AND 1440),
    sync_on_startup INTEGER NOT NULL CHECK(sync_on_startup IN (0, 1)),
    updated_at_ms INTEGER NOT NULL,
    desktop_list_mode INTEGER NOT NULL DEFAULT 0 CHECK(desktop_list_mode IN (0, 1)),
    desktop_opacity_percent INTEGER NOT NULL DEFAULT 82 CHECK(desktop_opacity_percent BETWEEN 20 AND 100),
    start_hidden_in_tray INTEGER NOT NULL DEFAULT 0 CHECK(start_hidden_in_tray IN (0, 1))
) STRICT;

INSERT INTO app_settings_v17(
    singleton_id,window_mode,sync_interval_minutes,sync_on_startup,updated_at_ms,
    desktop_list_mode,desktop_opacity_percent,start_hidden_in_tray
)
SELECT singleton_id,
       CASE window_mode WHEN 'normal_window' THEN 'desktop_companion' ELSE window_mode END,
       sync_interval_minutes,sync_on_startup,updated_at_ms,
       desktop_list_mode,desktop_opacity_percent,start_hidden_in_tray
FROM app_settings;

DROP TABLE app_settings;
ALTER TABLE app_settings_v17 RENAME TO app_settings;
"#;

const MIGRATION_018: &str = r#"
ALTER TABLE app_settings ADD COLUMN launch_at_login INTEGER NOT NULL DEFAULT 0 CHECK(launch_at_login IN (0, 1));
"#;

const MIGRATION_019: &str = r#"
ALTER TABLE calendars ADD COLUMN color_override TEXT CHECK(color_override IS NULL OR (length(color_override)=7 AND color_override GLOB '#[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]'));
"#;

const MIGRATION_020: &str = r#"
ALTER TABLE app_settings ADD COLUMN local_todo_color TEXT NOT NULL DEFAULT '#2F7D68' CHECK(length(local_todo_color)=7 AND local_todo_color GLOB '#[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]');
"#;

const DEFAULT_LOCAL_ACCOUNT_ID: &str = "00000000-0000-4000-8000-0000000000a1";
const MIGRATION_021: &str = r#"
CREATE TABLE local_accounts (
    account_id TEXT PRIMARY KEY NOT NULL,
    display_name TEXT NOT NULL CHECK(length(trim(display_name)) BETWEEN 1 AND 128)
);
CREATE TABLE local_calendars (
    calendar_id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL REFERENCES local_accounts(account_id),
    name TEXT NOT NULL CHECK(length(trim(name)) BETWEEN 1 AND 128),
    color TEXT NOT NULL CHECK(length(color)=7 AND color GLOB '#[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]'),
    is_default INTEGER NOT NULL DEFAULT 0 CHECK(is_default IN (0,1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE UNIQUE INDEX one_default_local_calendar ON local_calendars(account_id) WHERE is_default=1;
INSERT INTO local_accounts(account_id,display_name) VALUES('00000000-0000-4000-8000-0000000000a1','本地');
INSERT INTO local_calendars(calendar_id,account_id,name,color,is_default,created_at_ms,updated_at_ms)
SELECT '00000000-0000-4000-8000-0000000000c1','00000000-0000-4000-8000-0000000000a1','默认日历',local_todo_color,1,updated_at_ms,updated_at_ms FROM app_settings WHERE singleton_id=1;
ALTER TABLE local_todos ADD COLUMN local_calendar_id TEXT REFERENCES local_calendars(calendar_id);
UPDATE local_todos SET local_calendar_id='00000000-0000-4000-8000-0000000000c1';
CREATE TRIGGER local_todos_calendar_required_insert BEFORE INSERT ON local_todos WHEN NEW.local_calendar_id IS NULL OR NOT EXISTS(SELECT 1 FROM local_calendars WHERE calendar_id=NEW.local_calendar_id) BEGIN SELECT RAISE(ABORT,'local calendar required'); END;
CREATE TRIGGER local_todos_calendar_required_update BEFORE UPDATE OF local_calendar_id ON local_todos WHEN NEW.local_calendar_id IS NULL OR NOT EXISTS(SELECT 1 FROM local_calendars WHERE calendar_id=NEW.local_calendar_id) BEGIN SELECT RAISE(ABORT,'local calendar required'); END;
"#;
const MIGRATION_022: &str = r#"
ALTER TABLE calendars ADD COLUMN selected INTEGER NOT NULL DEFAULT 1 CHECK(selected IN (0,1));
"#;

/// Supports the bounded retention purge of confirmed-deleted raw cache: the
/// eligible set is selected by lifecycle and deletion instant, never by a scan
/// of every cached resource.
const MIGRATION_023: &str = r#"
CREATE INDEX remote_resources_by_deletion ON remote_resources(lifecycle, remote_deleted_at_ms, resource_id);
"#;

/// Lets a local calendar be hidden without touching any task it owns. Hidden
/// calendars keep their rows, membership, completion, and subtasks; they stop
/// contributing Agenda rows and reminders only.
const MIGRATION_024: &str = r#"
ALTER TABLE local_calendars ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0 CHECK(hidden IN (0,1));
"#;

/// Stores the desktop list's own presentation choices: whether its position is
/// locked and whether the user left it folded. These are deliberately separate
/// from the Settings form so it can never overwrite them, and the folded
/// rectangle itself is still never a restore target.
const MIGRATION_025: &str = r#"
ALTER TABLE app_settings ADD COLUMN desktop_pinned INTEGER NOT NULL DEFAULT 0 CHECK(desktop_pinned IN (0,1));
ALTER TABLE app_settings ADD COLUMN desktop_collapsed INTEGER NOT NULL DEFAULT 0 CHECK(desktop_collapsed IN (0,1));
"#;

/// Durable Phase 3 intent and conflict evidence. Nothing consumes these rows
/// over the network yet: a future writer must add a reviewed claim/execute/read-
/// back path before an operation can become `applied`.
const MIGRATION_026: &str = r#"
CREATE TABLE outbox_operations (
    operation_id TEXT PRIMARY KEY NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE RESTRICT,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE RESTRICT,
    resource_id TEXT NOT NULL REFERENCES remote_resources(resource_id) ON DELETE RESTRICT,
    series_id TEXT NOT NULL REFERENCES event_series(series_id) ON DELETE RESTRICT,
    operation_kind TEXT NOT NULL CHECK(operation_kind = 'update_title'),
    base_etag TEXT NOT NULL CHECK(length(base_etag) BETWEEN 1 AND 4096),
    base_raw_ics BLOB NOT NULL CHECK(length(base_raw_ics) BETWEEN 1 AND 4194304),
    intent_json TEXT NOT NULL CHECK(length(intent_json) BETWEEN 2 AND 4096),
    state TEXT NOT NULL CHECK(state IN ('pending','conflict','applied','cancelled')),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX outbox_one_pending_resource
    ON outbox_operations(resource_id) WHERE state='pending';
CREATE INDEX outbox_by_account_state
    ON outbox_operations(account_id,state,created_at_ms,operation_id);

CREATE TABLE conflicts (
    conflict_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL UNIQUE REFERENCES outbox_operations(operation_id) ON DELETE CASCADE,
    reason TEXT NOT NULL CHECK(reason IN ('concurrent_remote_change')),
    base_etag TEXT NOT NULL,
    base_raw_ics BLOB NOT NULL CHECK(length(base_raw_ics) BETWEEN 1 AND 4194304),
    local_intent_json TEXT NOT NULL CHECK(length(local_intent_json) BETWEEN 2 AND 4096),
    remote_etag TEXT NOT NULL,
    remote_raw_ics BLOB NOT NULL CHECK(length(remote_raw_ics) BETWEEN 1 AND 4194304),
    resolution_state TEXT NOT NULL CHECK(resolution_state IN ('unresolved','keep_remote','retry_local')),
    created_at_ms INTEGER NOT NULL,
    resolved_at_ms INTEGER
) STRICT;
CREATE INDEX conflicts_unresolved
    ON conflicts(resolution_state,created_at_ms,conflict_id);
"#;

/// Internal execution metadata. The public mutation state remains pending
/// while an opaque lease owns work or an ambiguous outcome requires GET-first
/// verification; no production writer is registered by this migration.
const MIGRATION_027: &str = r#"
ALTER TABLE outbox_operations ADD COLUMN execution_phase TEXT NOT NULL DEFAULT 'ready'
    CHECK(execution_phase IN ('ready','verify_first'));
ALTER TABLE outbox_operations ADD COLUMN claim_token TEXT;
ALTER TABLE outbox_operations ADD COLUMN claimed_at_ms INTEGER;
ALTER TABLE outbox_operations ADD COLUMN last_outcome TEXT
    CHECK(last_outcome IS NULL OR last_outcome IN ('transient','ambiguous','unauthorized','unsafe'));
CREATE UNIQUE INDEX outbox_claim_token ON outbox_operations(claim_token) WHERE claim_token IS NOT NULL;
"#;

/// Links an idempotent retry-local resolution to the fresh operation created
/// from the current committed remote revision.
const MIGRATION_028: &str = r#"
ALTER TABLE conflicts ADD COLUMN retry_operation_id TEXT
    REFERENCES outbox_operations(operation_id) ON DELETE RESTRICT;
CREATE UNIQUE INDEX conflicts_retry_operation
    ON conflicts(retry_operation_id) WHERE retry_operation_id IS NOT NULL;
"#;

/// Account-scoped authorization for future remote mutation execution. This is
/// non-secret workflow state, defaults off, and does not register a transport.
const MIGRATION_029: &str = r#"
ALTER TABLE accounts ADD COLUMN remote_editing_enabled INTEGER NOT NULL DEFAULT 0
    CHECK(remote_editing_enabled IN (0,1));
"#;

/// A read stage must not resurrect a resource revision captured as the base
/// of a title update that has already reached the actor's applied state.
const MIGRATION_030: &str = r#"
CREATE INDEX outbox_applied_base_by_resource
    ON outbox_operations(resource_id,base_etag) WHERE state='applied';
"#;

/// A direct conflict also leaves the committed cache at its captured base.
/// A later inventory carrying that base cannot prove it observed the conflict.
const MIGRATION_031: &str = r#"
DROP INDEX outbox_applied_base_by_resource;
CREATE INDEX outbox_terminal_base_by_resource
    ON outbox_operations(resource_id,base_etag);
"#;

/// Existing candidates have no durable proof that their omission survived a
/// calendar availability cycle, so migration leaves them invalid.
const MIGRATION_032: &str = r#"
ALTER TABLE remote_resources ADD COLUMN candidate_evidence_valid INTEGER NOT NULL DEFAULT 0
    CHECK(candidate_evidence_valid IN (0,1));
"#;

/// Earlier discovery route repairs could retain another route's token.
/// Legacy tokens have no evidence of ownership; rebuild them by full inventory.
const MIGRATION_033: &str = r#"
UPDATE calendar_sync_state SET committed_sync_token=NULL;
"#;

/// Product deletion intent is persisted separately until its own claim and
/// terminal state machine is reviewed. This avoids widening the title writer's
/// executable outbox by schema accident.
const MIGRATION_034: &str = r#"
CREATE TABLE remote_delete_operations (
    operation_id TEXT PRIMARY KEY NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE RESTRICT,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE RESTRICT,
    resource_id TEXT NOT NULL REFERENCES remote_resources(resource_id) ON DELETE RESTRICT,
    series_id TEXT NOT NULL REFERENCES event_series(series_id) ON DELETE RESTRICT,
    base_etag TEXT NOT NULL CHECK(length(base_etag) BETWEEN 1 AND 256),
    base_raw_ics BLOB NOT NULL CHECK(length(base_raw_ics) BETWEEN 1 AND 4194304),
    intent_json TEXT NOT NULL CHECK(intent_json = '{"delete":true}'),
    state TEXT NOT NULL CHECK(state = 'pending'),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX remote_delete_one_pending_resource
    ON remote_delete_operations(resource_id) WHERE state='pending';
CREATE INDEX remote_delete_by_account_state
    ON remote_delete_operations(account_id,state,created_at_ms,operation_id);
"#;

/// Deletion execution remains a separate state machine. Existing pending-only
/// rows are copied losslessly while adding an actor-owned lease, GET-first
/// recovery phase, and bounded terminal evidence.
const MIGRATION_035: &str = r#"
DROP INDEX remote_delete_one_pending_resource;
DROP INDEX remote_delete_by_account_state;
ALTER TABLE remote_delete_operations RENAME TO remote_delete_operations_v34;
CREATE TABLE remote_delete_operations (
    operation_id TEXT PRIMARY KEY NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE RESTRICT,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE RESTRICT,
    resource_id TEXT NOT NULL REFERENCES remote_resources(resource_id) ON DELETE RESTRICT,
    series_id TEXT NOT NULL REFERENCES event_series(series_id) ON DELETE RESTRICT,
    base_etag TEXT NOT NULL CHECK(length(base_etag) BETWEEN 1 AND 256),
    base_raw_ics BLOB NOT NULL CHECK(length(base_raw_ics) BETWEEN 1 AND 4194304),
    intent_json TEXT NOT NULL CHECK(intent_json = '{"delete":true}'),
    state TEXT NOT NULL CHECK(state IN ('pending','conflict','applied','cancelled')),
    execution_phase TEXT NOT NULL DEFAULT 'ready' CHECK(execution_phase IN ('ready','verify_first')),
    claim_token TEXT UNIQUE,
    claimed_at_ms INTEGER,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
    last_outcome TEXT,
    remote_etag TEXT CHECK(remote_etag IS NULL OR length(remote_etag) BETWEEN 1 AND 256),
    remote_raw_ics BLOB CHECK(remote_raw_ics IS NULL OR length(remote_raw_ics) BETWEEN 1 AND 4194304),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK((claim_token IS NULL AND claimed_at_ms IS NULL) OR
          (claim_token IS NOT NULL AND claimed_at_ms IS NOT NULL)),
    CHECK(state='pending' OR claim_token IS NULL)
) STRICT;
INSERT INTO remote_delete_operations(
    operation_id,idempotency_key,account_id,calendar_id,resource_id,series_id,
    base_etag,base_raw_ics,intent_json,state,created_at_ms,updated_at_ms)
SELECT operation_id,idempotency_key,account_id,calendar_id,resource_id,series_id,
       base_etag,base_raw_ics,intent_json,state,created_at_ms,updated_at_ms
FROM remote_delete_operations_v34;
DROP TABLE remote_delete_operations_v34;
CREATE UNIQUE INDEX remote_delete_one_pending_resource
    ON remote_delete_operations(resource_id) WHERE state='pending';
CREATE INDEX remote_delete_by_account_state
    ON remote_delete_operations(account_id,state,created_at_ms,operation_id);
"#;

/// Deletion conflicts keep their resolution lineage in the deletion store.
/// This avoids making the title conflict table polymorphic or granting its
/// retry path authority to create deletion work.
const MIGRATION_036: &str = r#"
ALTER TABLE remote_delete_operations ADD COLUMN resolution_state TEXT
    CHECK(resolution_state IS NULL OR resolution_state IN ('unresolved','keep_remote','retry_local'));
ALTER TABLE remote_delete_operations ADD COLUMN retry_operation_id TEXT
    REFERENCES remote_delete_operations(operation_id) ON DELETE RESTRICT;
ALTER TABLE remote_delete_operations ADD COLUMN conflict_at_ms INTEGER;
ALTER TABLE remote_delete_operations ADD COLUMN resolved_at_ms INTEGER;
UPDATE remote_delete_operations SET resolution_state='unresolved',conflict_at_ms=updated_at_ms
    WHERE state='conflict';
CREATE INDEX remote_delete_conflict_resolution
    ON remote_delete_operations(account_id,resolution_state,updated_at_ms,operation_id)
    WHERE state='conflict';
"#;

/// Presentation-only metadata for CalDAV events plus the user's preferred
/// destination for newly opened create forms. These values never enter ICS.
const MIGRATION_046: &str = r#"
ALTER TABLE app_settings ADD COLUMN default_calendar_id TEXT;

CREATE TABLE calendar_resource_tags (
    resource_id TEXT NOT NULL,
    tag_id TEXT NOT NULL REFERENCES tags(tag_id) ON DELETE RESTRICT,
    sort_order INTEGER NOT NULL CHECK(sort_order >= 0),
    PRIMARY KEY(resource_id, tag_id),
    UNIQUE(resource_id, sort_order)
) STRICT;
CREATE INDEX calendar_resource_tags_by_tag ON calendar_resource_tags(tag_id, resource_id);

CREATE TABLE calendar_resource_recurrence (
    resource_id TEXT PRIMARY KEY NOT NULL,
    frequency TEXT NOT NULL CHECK(frequency IN ('daily', 'weekly')),
    interval_value INTEGER NOT NULL CHECK(interval_value BETWEEN 1 AND 365),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;
"#;

/// Per-calendar local Agenda presentation bounds. They never narrow remote
/// discovery, synchronization, cache retention, deletion evidence or reminders.
const MIGRATION_047: &str = r#"
ALTER TABLE calendars ADD COLUMN display_past_days INTEGER NOT NULL DEFAULT 7
    CHECK(display_past_days BETWEEN 0 AND 3650);
ALTER TABLE calendars ADD COLUMN display_future_days INTEGER NOT NULL DEFAULT 30
    CHECK(display_future_days BETWEEN 0 AND 3650);
"#;

const MIGRATION_048: &str = r#"
ALTER TABLE app_settings ADD COLUMN language TEXT NOT NULL DEFAULT 'system'
    CHECK(language IN ('system', 'zh-CN', 'en'));
"#;

/// Desktop-list density controls. Both default on to preserve existing installs;
/// turning them off changes presentation only.
const MIGRATION_049: &str = r#"
ALTER TABLE app_settings ADD COLUMN desktop_show_calendar_name INTEGER NOT NULL DEFAULT 1
    CHECK(desktop_show_calendar_name IN (0, 1));
ALTER TABLE app_settings ADD COLUMN desktop_show_time INTEGER NOT NULL DEFAULT 1
    CHECK(desktop_show_time IN (0, 1));
"#;

/// Main-window Agenda density controls. These are presentation-only and
/// default on so an upgrade preserves the existing information density.
const MIGRATION_050: &str = r#"
ALTER TABLE app_settings ADD COLUMN main_show_calendar_name INTEGER NOT NULL DEFAULT 1
    CHECK(main_show_calendar_name IN (0, 1));
ALTER TABLE app_settings ADD COLUMN main_show_time INTEGER NOT NULL DEFAULT 1
    CHECK(main_show_time IN (0, 1));
"#;

/// Optional tag metadata for the main and desktop lists. Both default on so an
/// upgrade preserves already-visible Desktop List tags and exposes one shared
/// presentation rule without affecting calendar data or synchronization.
const MIGRATION_051: &str = r#"
ALTER TABLE app_settings ADD COLUMN desktop_show_tags INTEGER NOT NULL DEFAULT 1
    CHECK(desktop_show_tags IN (0, 1));
ALTER TABLE app_settings ADD COLUMN main_show_tags INTEGER NOT NULL DEFAULT 1
    CHECK(main_show_tags IN (0, 1));
"#;

/// Optional presentation bounds. Existing remote calendars retain their 7/30
/// bounds; new unbounded flags let either field be blank without overloading a
/// numeric sentinel. Local TODO bounds default to unbounded.
const MIGRATION_052: &str = r#"
ALTER TABLE app_settings ADD COLUMN local_display_past_days INTEGER
    CHECK(local_display_past_days BETWEEN 0 AND 3650);
ALTER TABLE app_settings ADD COLUMN local_display_future_days INTEGER
    CHECK(local_display_future_days BETWEEN 0 AND 3650);
ALTER TABLE calendars ADD COLUMN display_past_unbounded INTEGER NOT NULL DEFAULT 0
    CHECK(display_past_unbounded IN (0, 1));
ALTER TABLE calendars ADD COLUMN display_future_unbounded INTEGER NOT NULL DEFAULT 0
    CHECK(display_future_unbounded IN (0, 1));
"#;

/// Local calendar presentation bounds replace the former account-wide bounds.
/// Existing calendars inherit the current account values once; new calendars
/// remain unbounded until the user configures them.
const MIGRATION_053: &str = r#"
ALTER TABLE local_calendars ADD COLUMN display_past_days INTEGER
    CHECK(display_past_days BETWEEN 0 AND 3650);
ALTER TABLE local_calendars ADD COLUMN display_future_days INTEGER
    CHECK(display_future_days BETWEEN 0 AND 3650);
UPDATE local_calendars
SET display_past_days=(SELECT local_display_past_days FROM app_settings WHERE singleton_id=1),
    display_future_days=(SELECT local_display_future_days FROM app_settings WHERE singleton_id=1);
"#;

/// Bounded local recurrence rules retain durable occurrence history while
/// adding calendar-based schedules and explicit end conditions.
const MIGRATION_054: &str = r#"
CREATE TABLE IF NOT EXISTS calendar_resource_recurrence (
    resource_id TEXT PRIMARY KEY NOT NULL,
    frequency TEXT NOT NULL CHECK(frequency IN ('daily', 'weekly')),
    interval_value INTEGER NOT NULL CHECK(interval_value BETWEEN 1 AND 365),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;
ALTER TABLE calendar_resource_recurrence RENAME TO calendar_resource_recurrence_v53;
CREATE TABLE calendar_resource_recurrence (
    resource_id TEXT PRIMARY KEY NOT NULL,
    frequency TEXT NOT NULL CHECK(frequency IN ('daily','weekly','monthly','yearly','custom_weekly','custom_monthly','custom_yearly')),
    interval_value INTEGER NOT NULL CHECK(interval_value BETWEEN 1 AND 365),
    timezone TEXT NOT NULL DEFAULT 'UTC' CHECK(length(timezone) BETWEEN 1 AND 128),
    weekdays_mask INTEGER NOT NULL DEFAULT 0 CHECK(weekdays_mask BETWEEN 0 AND 127),
    month_days_mask INTEGER NOT NULL DEFAULT 0 CHECK(month_days_mask BETWEEN 0 AND 2147483647),
    year_months_mask INTEGER NOT NULL DEFAULT 0 CHECK(year_months_mask BETWEEN 0 AND 4095),
    end_kind TEXT NOT NULL DEFAULT 'count' CHECK(end_kind IN ('count','until')),
    end_count INTEGER DEFAULT 100 CHECK(end_count BETWEEN 1 AND 100),
    end_until_at_utc_ms INTEGER,
    account_id TEXT,
    calendar_id TEXT,
    series_id TEXT,
    uid TEXT,
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
    enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)),
    anchor_start_at_utc_ms INTEGER,
    anchor_end_at_utc_ms INTEGER,
    cursor_start_at_utc_ms INTEGER,
    cursor_end_at_utc_ms INTEGER,
    pending_operation_id TEXT,
    pending_idempotency_key TEXT,
    pending_expected_start_at_utc_ms INTEGER,
    pending_expected_end_at_utc_ms INTEGER,
    pending_next_start_at_utc_ms INTEGER,
    pending_next_end_at_utc_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK((end_kind='count' AND end_count IS NOT NULL AND end_until_at_utc_ms IS NULL) OR (end_kind='until' AND end_count IS NULL AND end_until_at_utc_ms IS NOT NULL)),
    CHECK(enabled=0 OR (
        account_id IS NOT NULL AND calendar_id IS NOT NULL AND series_id IS NOT NULL AND uid IS NOT NULL
        AND anchor_start_at_utc_ms IS NOT NULL AND anchor_end_at_utc_ms IS NOT NULL
        AND cursor_start_at_utc_ms IS NOT NULL AND cursor_end_at_utc_ms IS NOT NULL
        AND anchor_start_at_utc_ms < anchor_end_at_utc_ms
        AND cursor_start_at_utc_ms < cursor_end_at_utc_ms
    )),
    CHECK((pending_operation_id IS NULL AND pending_idempotency_key IS NULL
           AND pending_expected_start_at_utc_ms IS NULL AND pending_expected_end_at_utc_ms IS NULL
           AND pending_next_start_at_utc_ms IS NULL AND pending_next_end_at_utc_ms IS NULL)
       OR (pending_operation_id IS NOT NULL AND pending_idempotency_key IS NOT NULL
           AND pending_expected_start_at_utc_ms IS NOT NULL AND pending_expected_end_at_utc_ms IS NOT NULL
           AND pending_next_start_at_utc_ms IS NOT NULL AND pending_next_end_at_utc_ms IS NOT NULL))
) STRICT;
INSERT INTO calendar_resource_recurrence(resource_id,frequency,interval_value,timezone,weekdays_mask,month_days_mask,year_months_mask,end_kind,end_count,end_until_at_utc_ms,generation,enabled,created_at_ms,updated_at_ms)
SELECT resource_id,frequency,interval_value,'UTC',0,0,0,'count',100,NULL,0,0,created_at_ms,updated_at_ms FROM calendar_resource_recurrence_v53;
DROP TABLE calendar_resource_recurrence_v53;

CREATE UNIQUE INDEX calendar_resource_recurrence_pending_operation
    ON calendar_resource_recurrence(pending_operation_id)
    WHERE pending_operation_id IS NOT NULL;
CREATE UNIQUE INDEX calendar_resource_recurrence_pending_idempotency
    ON calendar_resource_recurrence(pending_idempotency_key)
    WHERE pending_idempotency_key IS NOT NULL;

CREATE TABLE calendar_local_occurrence_tombstones (
    resource_id TEXT NOT NULL,
    nominal_end_at_utc_ms INTEGER NOT NULL,
    cancelled_at_ms INTEGER NOT NULL,
    PRIMARY KEY(resource_id, nominal_end_at_utc_ms)
) STRICT;

ALTER TABLE local_todo_occurrences RENAME TO local_todo_occurrences_v53;
ALTER TABLE local_todo_recurrence RENAME TO local_todo_recurrence_v53;

CREATE TABLE local_todo_recurrence (
    local_todo_id TEXT PRIMARY KEY NOT NULL REFERENCES local_todos(todo_id) ON DELETE CASCADE,
    series_id TEXT NOT NULL UNIQUE,
    schedule_state TEXT NOT NULL DEFAULT 'active' CHECK(schedule_state IN ('active','stopped')),
    retained_occurrence_key TEXT,
    frequency TEXT NOT NULL CHECK(frequency IN ('daily','weekly','monthly','yearly','custom_weekly','custom_monthly','custom_yearly')),
    interval_value INTEGER NOT NULL CHECK(interval_value BETWEEN 1 AND 365),
    timezone TEXT NOT NULL DEFAULT 'UTC' CHECK(length(timezone) BETWEEN 1 AND 128),
    weekdays_mask INTEGER NOT NULL DEFAULT 0 CHECK(weekdays_mask BETWEEN 0 AND 127),
    month_days_mask INTEGER NOT NULL DEFAULT 0 CHECK(month_days_mask BETWEEN 0 AND 2147483647),
    year_months_mask INTEGER NOT NULL DEFAULT 0 CHECK(year_months_mask BETWEEN 0 AND 4095),
    end_kind TEXT NOT NULL DEFAULT 'count' CHECK(end_kind IN ('count','until')),
    end_count INTEGER DEFAULT 100 CHECK(end_count BETWEEN 1 AND 100),
    end_until_at_utc_ms INTEGER,
    anchor_due_at_utc_ms INTEGER NOT NULL,
    start_offset_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK((end_kind='count' AND end_count IS NOT NULL AND end_until_at_utc_ms IS NULL) OR (end_kind='until' AND end_count IS NULL AND end_until_at_utc_ms IS NOT NULL))
) STRICT;
INSERT INTO local_todo_recurrence(local_todo_id,series_id,schedule_state,retained_occurrence_key,frequency,interval_value,timezone,weekdays_mask,month_days_mask,year_months_mask,end_kind,end_count,end_until_at_utc_ms,anchor_due_at_utc_ms,start_offset_ms,created_at_ms,updated_at_ms)
SELECT local_todo_id,series_id,'active',NULL,frequency,interval_value,'UTC',0,0,0,'count',100,NULL,anchor_due_at_utc_ms,start_offset_ms,created_at_ms,updated_at_ms FROM local_todo_recurrence_v53;

CREATE TABLE local_todo_occurrences (
    occurrence_id TEXT PRIMARY KEY NOT NULL,
    local_todo_id TEXT NOT NULL REFERENCES local_todos(todo_id) ON DELETE CASCADE,
    series_id TEXT NOT NULL REFERENCES local_todo_recurrence(series_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK(sequence >= 0),
    occurrence_key TEXT NOT NULL,
    nominal_identity TEXT NOT NULL,
    nominal_start_at_utc_ms INTEGER,
    nominal_due_at_utc_ms INTEGER NOT NULL,
    occurrence_state TEXT NOT NULL DEFAULT 'active' CHECK(occurrence_state IN ('active','retired','cancelled')),
    cancelled_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    UNIQUE(local_todo_id, occurrence_key),
    UNIQUE(series_id, sequence),
    UNIQUE(series_id, nominal_identity)
) STRICT;
INSERT INTO local_todo_occurrences(occurrence_id,local_todo_id,series_id,sequence,occurrence_key,nominal_identity,nominal_start_at_utc_ms,nominal_due_at_utc_ms,occurrence_state,created_at_ms)
SELECT occurrence_id,local_todo_id,series_id,sequence,occurrence_key,'UTC:'||nominal_due_at_utc_ms,nominal_start_at_utc_ms,nominal_due_at_utc_ms,'active',created_at_ms FROM local_todo_occurrences_v53;
DROP TABLE local_todo_occurrences_v53;
DROP TABLE local_todo_recurrence_v53;
CREATE INDEX local_todo_occurrences_due ON local_todo_occurrences(local_todo_id, nominal_due_at_utc_ms);
"#;

/// Presentation-only palette choice, kept as migration history for the later
/// palette/mode split. It never influences calendar data, synchronization,
/// cache retention, reminders or mutation admission.
const MIGRATION_055: &str = r#"
ALTER TABLE app_settings ADD COLUMN theme TEXT NOT NULL DEFAULT 'system'
    CHECK(theme IN ('system', 'quietDaybook', 'dawnWheat'));
"#;

/// Palette and color mode are orthogonal presentation preferences. The old
/// Migration 055 value is retained as upgrade evidence and mapped exactly once.
const MIGRATION_056: &str = r#"
ALTER TABLE app_settings ADD COLUMN theme_palette TEXT NOT NULL DEFAULT 'quietDaybook'
    CHECK(theme_palette IN ('quietDaybook', 'dawnWheat'));
ALTER TABLE app_settings ADD COLUMN color_mode TEXT NOT NULL DEFAULT 'system'
    CHECK(color_mode IN ('system', 'light', 'dark'));
UPDATE app_settings
SET theme_palette=CASE theme
        WHEN 'dawnWheat' THEN 'dawnWheat'
        ELSE 'quietDaybook'
    END,
    color_mode=CASE theme
        WHEN 'quietDaybook' THEN 'light'
        ELSE 'system'
    END;
"#;

/// First-run default is Quiet Daybook in explicit Light.
///
/// Migration 056 already maps a *stored* legacy choice into `theme_palette` and
/// `color_mode`, and that mapping stays authoritative for every profile the user
/// configured. This migration only covers the untouched seed row that migration
/// 004 created (`updated_at_ms = 0`): nothing was ever saved against it, so the
/// values it carries are product defaults rather than a user choice. Profiles
/// with a saved settings write keep their exact pair.
const MIGRATION_057: &str = r#"
UPDATE app_settings
SET color_mode='light'
WHERE singleton_id=1
  AND updated_at_ms=0
  AND theme='system'
  AND theme_palette='quietDaybook'
  AND color_mode='system';
"#;

/// Two durable reminder decisions belong to the user, not to the planner:
///
/// 1. A fire instant that the application never scheduled was never an
///    opportunity to notify. That covers a past item created locally, an iCloud
///    event created in the past, an external client moving an item into the past,
///    and the first import of historical events. Such an instant is recorded
///    `suppressed` instead of entering the catch-up window, which stays reserved
///    for fire instants that were already `planned` before this rebuild.
/// 2. One switch turns Windows reminder delivery off without stopping the
///    durable projection, so a re-enabled profile resumes only future fires.
///
/// `suppressed` is a terminal state distinct from `missed` (late startup work)
/// and `failed` (a platform refusal that is never retried).
const MIGRATION_058: &str = r#"
DROP INDEX notification_jobs_due;
ALTER TABLE notification_jobs RENAME TO notification_jobs_v57;
CREATE TABLE notification_jobs (
    job_id TEXT PRIMARY KEY NOT NULL,
    source_kind TEXT NOT NULL CHECK(source_kind IN ('local_todo', 'calendar_occurrence')),
    source_id TEXT NOT NULL,
    occurrence_key TEXT NOT NULL,
    revision TEXT NOT NULL,
    fire_at_utc_ms INTEGER NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('planned', 'claimed', 'delivered', 'cancelled', 'missed', 'failed', 'suppressed')),
    claimed_at_utc_ms INTEGER,
    finished_at_utc_ms INTEGER,
    UNIQUE(source_kind, source_id, occurrence_key, revision, fire_at_utc_ms)
) STRICT;
INSERT INTO notification_jobs(
    job_id,source_kind,source_id,occurrence_key,revision,fire_at_utc_ms,state,
    claimed_at_utc_ms,finished_at_utc_ms
)
SELECT job_id,source_kind,source_id,occurrence_key,revision,fire_at_utc_ms,state,
       claimed_at_utc_ms,finished_at_utc_ms
FROM notification_jobs_v57;
DROP TABLE notification_jobs_v57;
CREATE INDEX notification_jobs_due ON notification_jobs(state, fire_at_utc_ms, job_id);
ALTER TABLE app_settings ADD COLUMN reminder_notifications_enabled INTEGER NOT NULL DEFAULT 1
    CHECK(reminder_notifications_enabled IN (0, 1));
"#;

const UNSUPPORTED_RECURRENCE_KEY: &str = "UNSUPPORTED";

/// Raw remote-cache retention after a confirmed remote deletion.
///
/// Confirmation itself is only a logical transition: raw ICS, evidence,
/// identity, aliases, and local extension state stay durable, so a mistaken
/// deletion decision can still be corrected from local data. Physical
/// reclamation waits for this window.
const DELETED_RAW_CACHE_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;

/// Upper bound on resources reclaimed by one maintenance tick. A larger backlog
/// drains over several ticks instead of one unbounded transaction.
const DELETED_RAW_CACHE_PURGE_LIMIT: u32 = 256;
/// 2200-01-01T00:00:00Z. A projection window is derived from wall-clock time, so
/// an instant outside this range is rejected rather than used to pick a window.
const MAX_SUPPORTED_HORIZON_NOW_MS: i64 = 7_258_118_400_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationJob {
    pub job_id: String,
    pub source_kind: String,
    pub source_id: String,
    pub occurrence_key: String,
    pub fire_at_utc_ms: i64,
    pub catch_up: bool,
}

/// Rebuild only pending jobs. Claimed and terminal identities survive edits and
/// restart, so the same occurrence/revision cannot be dispatched twice.
fn reconcile_notification_jobs(
    connection: &mut Connection,
    now_ms: i64,
) -> Result<Option<i64>, PersistenceError> {
    admitted_now_ms(now_ms)?;
    let transaction = connection.transaction()?;
    transaction.execute(
        "UPDATE notification_jobs SET state='missed',finished_at_utc_ms=?1 WHERE state='claimed'",
        [now_ms],
    )?;
    // Eligibility changes (startup, resume, visibility, completion, undo) may
    // revive a cancelled fire: cancellation here records that the source was not
    // currently eligible, not that the reminder opportunity was spent.
    let mut desired = BTreeMap::<String, Vec<(String, String, i64)>>::new();
    for (id, key, revision, fire) in calendar_notification_fires(&transaction, None)? {
        desired.entry(id).or_default().push((key, revision, fire));
    }
    for (series_id, fires) in &desired {
        reconcile_notification_source(
            &transaction,
            "calendar_occurrence",
            series_id,
            fires,
            now_ms,
            NotificationReconcileContext::EligibilityRefresh,
        )?;
    }
    // A series that stopped being eligible (hidden or deselected calendar,
    // disabled account, missing resource, completed item) contributed no desired
    // fires above; cancel whatever it still had planned.
    let vanished = {
        let mut statement = transaction.prepare(
            "SELECT DISTINCT source_id FROM notification_jobs
             WHERE source_kind='calendar_occurrence' AND state='planned'",
        )?;
        statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for series_id in vanished {
        if desired.contains_key(&series_id) {
            continue;
        }
        reconcile_notification_source(
            &transaction,
            "calendar_occurrence",
            &series_id,
            &[],
            now_ms,
            NotificationReconcileContext::EligibilityRefresh,
        )?;
    }
    reconcile_local_notification_jobs(&transaction, None, now_ms)?;
    let next = transaction.query_row(
        "SELECT MIN(fire_at_utc_ms) FROM notification_jobs WHERE state='planned'",
        [],
        |r| r.get(0),
    )?;
    transaction.commit()?;
    if !reminder_notifications_enabled(connection)? {
        // The durable projection still tracks edits; only delivery authority is off.
        return Ok(None);
    }
    Ok(next)
}

/// Why a rebuild is asking for a set of fire instants.
///
/// `EligibilityRefresh` re-derives reminders from unchanged schedule data, so a
/// cancelled instant may become eligible again (hidden calendar revealed, item
/// restored). `ScheduleMutation` reports that the schedule itself changed, so a
/// fire instant appearing for the first time was never scheduled and must not be
/// delivered late.
#[derive(Clone, Copy)]
enum NotificationReconcileContext {
    EligibilityRefresh,
    ScheduleMutation,
}

/// Terminal fire identities never become deliverable again. Only `cancelled`
/// returns to `planned`, because a cancellation is the consequence of current
/// eligibility rather than a completed delivery decision.
const TERMINAL_NOTIFICATION_STATES: &str = "'claimed','delivered','missed','failed','suppressed'";

/// Logical fire identity. The durable `revision` is deliberately excluded: it
/// changes with every remote ETag or alarm index, while the reminder a user
/// experiences is one instant of one occurrence of one source.
fn terminal_notification_exists(
    transaction: &Transaction<'_>,
    source_kind: &str,
    source_id: &str,
    occurrence_key: &str,
    fire_at_utc_ms: i64,
) -> Result<bool, PersistenceError> {
    transaction
        .query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM notification_jobs
                 WHERE source_kind=?1 AND source_id=?2 AND occurrence_key=?3
                   AND fire_at_utc_ms=?4 AND state IN({TERMINAL_NOTIFICATION_STATES}))"
            ),
            params![source_kind, source_id, occurrence_key, fire_at_utc_ms],
            |r| r.get(0),
        )
        .map_err(PersistenceError::Sqlite)
}

/// Admit exactly one job for a logical fire instant.
///
/// A previously `planned` job is the durable proof that the application had
/// scheduled this reminder before the current rebuild, so an overdue instant
/// keeps its bounded catch-up opportunity. A fire instant with no such proof was
/// first observed now; when it is already due it is recorded as `suppressed`
/// instead of notifying, which is the same rule applied to newly created local
/// items and to in-app iCloud time edits.
fn admit_notification_job(
    transaction: &Transaction<'_>,
    source_kind: &str,
    source_id: &str,
    occurrence_key: &str,
    revision: &str,
    fire_at_utc_ms: i64,
    now_ms: i64,
    context: NotificationReconcileContext,
) -> Result<(), PersistenceError> {
    if terminal_notification_exists(
        transaction,
        source_kind,
        source_id,
        occurrence_key,
        fire_at_utc_ms,
    )? {
        // A terminal decision is final for this logical instant. Cancel any
        // still-planned row that persisted under another revision, so a legacy
        // duplicate can never be claimed alongside the decided identity.
        transaction.execute(
            "UPDATE notification_jobs SET state='cancelled',finished_at_utc_ms=?5
             WHERE source_kind=?1 AND source_id=?2 AND occurrence_key=?3
               AND fire_at_utc_ms=?4 AND state='planned'",
            params![
                source_kind,
                source_id,
                occurrence_key,
                fire_at_utc_ms,
                now_ms
            ],
        )?;
        return Ok(());
    }
    let planned = transaction
        .query_row(
            &format!(
                "SELECT job_id FROM notification_jobs
                 WHERE source_kind=?1 AND source_id=?2 AND occurrence_key=?3
                   AND fire_at_utc_ms=?4 AND state='planned' ORDER BY job_id LIMIT 1"
            ),
            params![source_kind, source_id, occurrence_key, fire_at_utc_ms],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    if let Some(job_id) = planned {
        // Drop only duplicate rows for the same logical instant; the surviving
        // claim has already proven this reminder was scheduled earlier.
        transaction.execute(
            "UPDATE notification_jobs SET state='cancelled',finished_at_utc_ms=?2
             WHERE source_kind=?1 AND source_id=?3 AND occurrence_key=?4
               AND fire_at_utc_ms=?5 AND state='planned' AND job_id<>?6",
            params![
                source_kind,
                now_ms,
                source_id,
                occurrence_key,
                fire_at_utc_ms,
                &job_id
            ],
        )?;
        return Ok(());
    }
    let cancelled = transaction
        .query_row(
            &format!(
                "SELECT job_id FROM notification_jobs
                 WHERE source_kind=?1 AND source_id=?2 AND occurrence_key=?3
                   AND fire_at_utc_ms=?4 AND state='cancelled' ORDER BY job_id DESC LIMIT 1"
            ),
            params![source_kind, source_id, occurrence_key, fire_at_utc_ms],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let (state, finished_at_utc_ms) = match (cancelled.is_some(), context) {
        // An eligibility rebuild restores an instant the user had already
        // scheduled: hiding and revealing a calendar, or undoing a deletion, is
        // not a new schedule. The claim-time 15-minute/24-hour bounds still
        // decide whether that restored instant may still notify.
        (true, NotificationReconcileContext::EligibilityRefresh) => ("planned", None),
        _ if fire_at_utc_ms > now_ms => ("planned", None),
        _ => ("suppressed", Some(now_ms)),
    };
    transaction.execute(
        "INSERT INTO notification_jobs(job_id,source_kind,source_id,occurrence_key,revision,fire_at_utc_ms,state,finished_at_utc_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(source_kind,source_id,occurrence_key,revision,fire_at_utc_ms)
         DO UPDATE SET state=?7,finished_at_utc_ms=?8 WHERE state='cancelled'",
        params![
            Uuid::new_v4().to_string(),
            source_kind,
            source_id,
            occurrence_key,
            revision,
            fire_at_utc_ms,
            state,
            finished_at_utc_ms
        ],
    )?;
    Ok(())
}

/// Reconcile one source against the fire instants its current projection implies.
/// Instants that disappeared are cancelled; nothing else about that source is
/// touched. `desired` carries `(occurrence_key, revision, fire_at_utc_ms)`.
fn reconcile_notification_source(
    transaction: &Transaction<'_>,
    source_kind: &str,
    source_id: &str,
    desired: &[(String, String, i64)],
    now_ms: i64,
    context: NotificationReconcileContext,
) -> Result<(), PersistenceError> {
    for (occurrence_key, revision, fire_at_utc_ms) in desired {
        admit_notification_job(
            transaction,
            source_kind,
            source_id,
            occurrence_key,
            revision,
            *fire_at_utc_ms,
            now_ms,
            context,
        )?;
    }
    let mut cancelled = Vec::new();
    {
        let mut statement = transaction.prepare(
            "SELECT job_id,occurrence_key,fire_at_utc_ms FROM notification_jobs
             WHERE source_kind=?1 AND source_id=?2 AND state='planned'",
        )?;
        let planned = statement.query_map(params![source_kind, source_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in planned {
            let (job_id, occurrence_key, fire_at_utc_ms) = row?;
            if !desired
                .iter()
                .any(|(key, _, fire)| *key == occurrence_key && *fire == fire_at_utc_ms)
            {
                cancelled.push(job_id);
            }
        }
    }
    for job_id in cancelled {
        transaction.execute(
            "UPDATE notification_jobs SET state='cancelled',finished_at_utc_ms=?2 WHERE job_id=?1 AND state='planned'",
            params![job_id, now_ms],
        )?;
    }
    Ok(())
}

/// Reference instants the local planner derives from current TODO state.
fn local_notification_fires(
    transaction: &Transaction<'_>,
    todo: Option<&str>,
) -> Result<Vec<(String, String, i64, i64)>, PersistenceError> {
    // A hidden calendar contributes no reminders; revealing it plans
    // the same jobs again because the task itself never changed.
    let mut stmt = transaction.prepare("SELECT t.todo_id, COALESCE(o.occurrence_key,'SINGLETON'), CASE WHEN o.occurrence_id IS NOT NULL THEN COALESCE(o.nominal_start_at_utc_ms,o.nominal_due_at_utc_ms) ELSE COALESCE(t.start_at_utc_ms,t.due_at_utc_ms) END, t.updated_at_ms FROM local_todos t JOIN local_calendars c ON c.calendar_id=t.local_calendar_id AND c.hidden=0 LEFT JOIN local_todo_occurrences o ON o.local_todo_id=t.todo_id AND o.occurrence_state='active' LEFT JOIN work_item_state w ON w.local_todo_id=t.todo_id AND w.local_occurrence_key=COALESCE(o.occurrence_key,'SINGLETON') WHERE (?1 IS NULL OR t.todo_id=?1) AND t.active=1 AND COALESCE(w.completed,0)=0 AND (CASE WHEN o.occurrence_id IS NOT NULL THEN COALESCE(o.nominal_start_at_utc_ms,o.nominal_due_at_utc_ms) ELSE COALESCE(t.start_at_utc_ms,t.due_at_utc_ms) END) IS NOT NULL AND (o.occurrence_id IS NOT NULL OR NOT EXISTS(SELECT 1 FROM local_todo_recurrence r WHERE r.local_todo_id=t.todo_id AND r.schedule_state='active'))")?;
    Ok(stmt
        .query_map([todo], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

/// Plan every currently eligible local reminder against its own source.
/// Lives at `EligibilityRefresh` because current TODO state is the whole input.
fn reconcile_local_notification_jobs(
    transaction: &Transaction<'_>,
    todo: Option<&str>,
    now_ms: i64,
) -> Result<(), PersistenceError> {
    let mut desired = BTreeMap::<String, Vec<(String, String, i64)>>::new();
    for (id, key, fire, revision) in local_notification_fires(transaction, todo)? {
        desired
            .entry(id)
            .or_default()
            .push((key, revision.to_string(), fire));
    }
    for (id, fires) in &desired {
        reconcile_notification_source(
            transaction,
            "local_todo",
            id,
            fires,
            now_ms,
            NotificationReconcileContext::EligibilityRefresh,
        )?;
    }
    // A source that vanished from the eligible set (deleted, completed, hidden)
    // must not keep a claimable job behind.
    let mut vanished = Vec::new();
    {
        let mut statement = transaction.prepare(
            "SELECT DISTINCT source_id FROM notification_jobs
             WHERE source_kind='local_todo' AND state='planned'",
        )?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let id = row?;
            if !desired.contains_key(&id) && todo.is_none_or(|scope| scope == id) {
                vanished.push(id);
            }
        }
    }
    for id in vanished {
        reconcile_notification_source(
            transaction,
            "local_todo",
            &id,
            &[],
            now_ms,
            NotificationReconcileContext::EligibilityRefresh,
        )?;
    }
    Ok(())
}

/// A schedule edit rebuilds only its own reminders, and only when the effective
/// instants really moved. Text-only edits leave the durable jobs untouched.
fn replan_local_notification_jobs_after_edit(
    transaction: &Transaction<'_>,
    todo: &str,
    now_ms: i64,
) -> Result<(), PersistenceError> {
    let desired = local_notification_fires(transaction, Some(todo))?;
    let mut statement = transaction.prepare(
        "SELECT occurrence_key,fire_at_utc_ms FROM notification_jobs WHERE source_kind='local_todo' AND source_id=?1 AND state='planned' ORDER BY occurrence_key,fire_at_utc_ms",
    )?;
    let planned = statement
        .query_map([todo], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut desired_instants = desired
        .iter()
        .map(|(_, key, fire, _)| (key.clone(), *fire))
        .collect::<Vec<_>>();
    desired_instants.sort();
    if desired_instants == planned {
        return Ok(());
    }
    let fires = desired
        .into_iter()
        .map(|(_, key, fire, revision)| (key, revision.to_string(), fire))
        .collect::<Vec<_>>();
    reconcile_notification_source(
        transaction,
        "local_todo",
        todo,
        &fires,
        now_ms,
        NotificationReconcileContext::ScheduleMutation,
    )
}

fn reminder_notifications_enabled(connection: &Connection) -> Result<bool, PersistenceError> {
    connection
        .query_row(
            "SELECT reminder_notifications_enabled FROM app_settings WHERE singleton_id=1",
            [],
            |row| row.get(0),
        )
        .map_err(PersistenceError::Sqlite)
}

/// Fire instants the current projections imply for one series or every series.
/// Each entry carries the occurrence identity, the alarm revision and the instant.
fn calendar_notification_fires(
    transaction: &Transaction<'_>,
    series: Option<&str>,
) -> Result<Vec<(String, String, String, i64)>, PersistenceError> {
    let calendar = {
        let mut stmt = transaction.prepare("SELECT p.series_id,p.recurrence_key,p.start_at_utc_ms,p.end_at_utc_ms,p.source_etag,r.raw_ics FROM calendar_event_projections p JOIN remote_resources r ON r.resource_id=p.resource_id JOIN event_series s ON s.series_id=p.series_id JOIN calendars c ON c.calendar_id=p.calendar_id JOIN accounts a ON a.account_id=s.account_id LEFT JOIN work_item_state w ON w.calendar_series_id=p.series_id AND w.recurrence_key=p.recurrence_key WHERE (?1 IS NULL OR p.series_id=?1) AND p.projection_status='ready' AND p.cancelled=0 AND p.start_at_utc_ms IS NOT NULL AND p.end_at_utc_ms IS NOT NULL AND r.lifecycle='active' AND a.enabled=1 AND a.removed_at_ms IS NULL AND c.selected=1 AND c.available=1 AND COALESCE(w.completed,0)=0")?;
        stmt.query_map([series], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Vec<u8>>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut fires = Vec::new();
    for (id, key, start, end, revision, raw) in calendar {
        if let Ok(alarms) = notification_fires_for_occurrence(&raw, &key, start, end) {
            let mut instants = std::collections::BTreeSet::new();
            for alarm in alarms {
                if !instants.insert(alarm.fire_at_utc_ms) {
                    continue;
                }
                let alarm_revision =
                    format!("{revision}:{}:{}", alarm.alarm_index, alarm.repeat_index);
                fires.push((
                    id.clone(),
                    key.clone(),
                    alarm_revision,
                    alarm.fire_at_utc_ms,
                ));
            }
        }
    }
    Ok(fires)
}

/// Reconcile one calendar series from its current projection. Sibling series are
/// untouched, so an unrelated sync cannot rewrite another series' reminders.
fn reconcile_calendar_notification_jobs(
    transaction: &Transaction<'_>,
    series: &str,
    now_ms: i64,
    context: NotificationReconcileContext,
) -> Result<(), PersistenceError> {
    let mut desired = Vec::new();
    for (id, key, alarm_revision, fire) in calendar_notification_fires(transaction, Some(series))? {
        debug_assert_eq!(id, series);
        desired.push((key, alarm_revision, fire));
    }
    reconcile_notification_source(
        transaction,
        "calendar_occurrence",
        series,
        &desired,
        now_ms,
        context,
    )
}

/// Reconcile many series in one pass. A full-sync import or a projection-window
/// rebuild touches most of the account, and one shared query avoids re-running
/// the projection join per series.
fn reconcile_calendar_notification_series<'a>(
    transaction: &Transaction<'_>,
    series_ids: impl IntoIterator<Item = &'a String>,
    now_ms: i64,
    context: NotificationReconcileContext,
) -> Result<(), PersistenceError> {
    let mut desired = BTreeMap::<String, Vec<(String, String, i64)>>::new();
    for (id, key, revision, fire) in calendar_notification_fires(transaction, None)? {
        desired.entry(id).or_default().push((key, revision, fire));
    }
    for series_id in series_ids {
        let fires = desired.remove(series_id).unwrap_or_default();
        reconcile_notification_source(
            transaction,
            "calendar_occurrence",
            series_id,
            &fires,
            now_ms,
            context,
        )?;
    }
    Ok(())
}

fn claim_due_notification_jobs(
    connection: &mut Connection,
    now_ms: i64,
) -> Result<Vec<NotificationJob>, PersistenceError> {
    admitted_now_ms(now_ms)?;
    if !reminder_notifications_enabled(connection)? {
        return Ok(Vec::new());
    }
    let transaction = connection.transaction()?;
    let due = {
        let mut stmt = transaction.prepare(
            "SELECT j.job_id,j.source_kind,j.source_id,j.occurrence_key,j.fire_at_utc_ms
             FROM notification_jobs j
             WHERE j.state='planned' AND j.fire_at_utc_ms<=?1
               AND (
                 (j.source_kind='local_todo' AND EXISTS(
                   SELECT 1 FROM local_todos t
                   JOIN local_calendars c ON c.calendar_id=t.local_calendar_id
                   LEFT JOIN work_item_state w ON w.local_todo_id=t.todo_id AND w.local_occurrence_key=j.occurrence_key
                   WHERE t.todo_id=j.source_id AND t.active=1 AND c.hidden=0 AND COALESCE(w.completed,0)=0
                     AND (
                       EXISTS(SELECT 1 FROM local_todo_occurrences o WHERE o.local_todo_id=t.todo_id AND o.occurrence_key=j.occurrence_key AND o.occurrence_state='active')
                       OR (j.occurrence_key='SINGLETON' AND NOT EXISTS(SELECT 1 FROM local_todo_recurrence r WHERE r.local_todo_id=t.todo_id))
                     )
                 ))
                 OR
                 (j.source_kind='calendar_occurrence' AND EXISTS(
                   SELECT 1 FROM calendar_event_projections p
                   JOIN remote_resources r ON r.resource_id=p.resource_id
                   JOIN calendars c ON c.calendar_id=p.calendar_id
                   JOIN event_series s ON s.series_id=p.series_id
                   JOIN accounts a ON a.account_id=s.account_id
                   LEFT JOIN work_item_state w ON w.calendar_series_id=p.series_id AND w.recurrence_key=p.recurrence_key
                   WHERE p.series_id=j.source_id AND p.recurrence_key=j.occurrence_key
                     AND p.projection_status='ready' AND p.cancelled=0
                     AND p.start_at_utc_ms IS NOT NULL AND p.end_at_utc_ms IS NOT NULL
                     AND r.lifecycle='active' AND c.selected=1 AND c.available=1
                     AND a.enabled=1 AND a.removed_at_ms IS NULL AND COALESCE(w.completed,0)=0
                 ))
               )
             ORDER BY j.fire_at_utc_ms,j.job_id LIMIT 128"
        )?;
        stmt.query_map([now_ms], |r| {
            Ok(NotificationJob {
                job_id: r.get(0)?,
                source_kind: r.get(1)?,
                source_id: r.get(2)?,
                occurrence_key: r.get(3)?,
                fire_at_utc_ms: r.get(4)?,
                catch_up: false,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut deliver = Vec::new();
    let mut catch_up_keys = std::collections::HashSet::new();
    for mut job in due {
        let late = now_ms.saturating_sub(job.fire_at_utc_ms);
        if late > 86_400_000
            || (late > 900_000
                && !catch_up_keys.insert((
                    job.source_kind.clone(),
                    job.source_id.clone(),
                    job.occurrence_key.clone(),
                )))
        {
            transaction.execute(
                "UPDATE notification_jobs SET state='missed',finished_at_utc_ms=?2 WHERE job_id=?1",
                params![job.job_id, now_ms],
            )?;
        } else {
            job.catch_up = late > 900_000;
            transaction.execute(
                "UPDATE notification_jobs SET state='claimed',claimed_at_utc_ms=?2 WHERE job_id=?1",
                params![job.job_id, now_ms],
            )?;
            deliver.push(job);
        }
    }
    transaction.commit()?;
    Ok(deliver)
}

fn finish_notification_job(
    connection: &mut Connection,
    job_id: &str,
    delivered: bool,
    now_ms: i64,
) -> Result<(), PersistenceError> {
    admitted_now_ms(now_ms)?;
    let state = if delivered { "delivered" } else { "failed" };
    connection.execute("UPDATE notification_jobs SET state=?2,finished_at_utc_ms=?3 WHERE job_id=?1 AND state='claimed'",params![job_id,state,now_ms])?;
    Ok(())
}

#[derive(Clone)]
pub struct Database {
    clock_port: std::sync::Weak<dyn Clock>,
    sender: Sender<Request>,
    owner: Arc<()>,
    // Last field: the final sender drops before joining the actor. Provenance
    // owner semantics remain independent from this thread-lifetime holder.
    _actor_lifetime: Arc<ActorLifetime>,
}

mod db_owner;
pub use db_owner::OwnerFailure as DatabaseOwnerFailure;
pub use db_owner::{
    DirectoryAccessRequestObservation, LockFallbackObservation, OwnerDetail, ParentAccessRequests,
};
pub use db_owner::{OwnerError as DatabaseOwnerErrorKind, OwnerStep};
mod product_read_only;
pub use product_read_only::ConfigurationAvailability;
pub(in crate::adapters) use product_read_only::ConfigurationCheck;
pub(in crate::adapters) use product_read_only::check_configured_probe_profile;
pub(in crate::adapters) use product_read_only::{
    ConfiguredProfileSelection, SelectionFailure, select_configured_probe_profile,
};
pub use product_read_only::{ProductReadOnlyFailure, read_legacy_diagnostic_profiles};
struct ActorLifetime(Option<std::thread::JoinHandle<()>>);
impl Drop for ActorLifetime {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take()
            && handle.thread().id() != std::thread::current().id()
        {
            let _ = handle.join();
        }
    }
}

/// Actor-issued evidence that a read stage began before any later cache write.
/// Fields stay private so callers cannot manufacture an ordering claim.
#[derive(Clone)]
pub struct SyncCapture {
    account_id: String,
    epoch: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteRecurrenceTarget {
    pub resource_id: String,
    pub account_id: String,
    pub collection_route: String,
    pub deadline_at_utc_ms: i64,
    pub pending: bool,
}

/// Bounded result of one recurrence-horizon projection rebuild.
///
/// Counts and window bounds only: no href, ETag, UID, title, or raw ICS leaves
/// the persistence boundary here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalendarProjectionRefreshSummary {
    pub window_start_at_utc_ms: i64,
    pub window_end_at_utc_ms: i64,
    pub series_count: u32,
    pub occurrence_count: u32,
    pub unsupported_series_count: u32,
}

/// One persisted window rectangle keyed by a stable window label.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredWindowPlacement {
    pub window_label: String,
    pub placement: WindowPlacement,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        Self::open_with_clock(path, Arc::new(crate::ports::clock::SystemClock))
    }

    /// Opens the actor-owned connection with an explicit clock.
    ///
    /// The clock is read once while the actor starts so the startup projection
    /// rebuild is derived from an injectable instant instead of the platform
    /// clock. Later horizon refreshes carry the instant supplied by the caller.
    pub fn open_with_clock(
        path: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, PersistenceError> {
        let owner_guard = db_owner::DatabaseOwner::acquire(path.as_ref())
            .map_err(PersistenceError::DatabaseOwnership)?;
        let clock_port = Arc::downgrade(&clock);
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("calendar-todo-sqlite".into())
            .spawn(move || run_actor(owner_guard, receiver, ready_sender, clock))
            .map_err(|_| PersistenceError::ActorUnavailable)?;
        let actor_lifetime = Arc::new(ActorLifetime(Some(thread)));
        ready_receiver
            .recv()
            .map_err(|_| PersistenceError::ActorUnavailable)??;
        Ok(Self {
            sender,
            clock_port,
            owner: Arc::new(()),
            _actor_lifetime: actor_lifetime,
        })
    }

    pub(crate) fn matches_clock(&self, clock: &Arc<dyn Clock>) -> bool {
        self.clock_port.upgrade().is_some()
            && std::sync::Weak::ptr_eq(&self.clock_port, &Arc::downgrade(clock))
    }
    pub(crate) fn actor_provenance(&self) -> std::sync::Weak<()> {
        Arc::downgrade(&self.owner)
    }
    pub(crate) fn exact_probe_collections(
        &self,
    ) -> Result<probe_target::CommittedExactProbeCollections, PersistenceError> {
        let actor = self.actor_provenance();
        self.request(|respond_to| Request::ExactProbeCollections { actor, respond_to })
    }
    pub(crate) fn revalidate_exact_probe_collections(
        &self,
        snapshot: probe_target::CommittedExactProbeCollections,
    ) -> Result<probe_target::CommittedExactProbeCollections, PersistenceError> {
        if !snapshot.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::RevalidateExactProbeCollections {
            snapshot,
            respond_to,
        })
    }
    #[allow(dead_code)] // Execution-free candidate; no Probe composition installed.
    pub(crate) fn probe_target_snapshot(
        &self,
        selection: probe_target::ProbeTargetSelection,
    ) -> Result<probe_target::CommittedProbeTarget, PersistenceError> {
        let actor = self.actor_provenance();
        self.request(|respond_to| Request::ProbeTargetSnapshot {
            selection,
            actor,
            respond_to,
        })
    }

    #[allow(dead_code)] // Execution-free candidate; no Probe composition installed.
    pub(crate) fn revalidate_probe_target(
        &self,
        snapshot: probe_target::CommittedProbeTarget,
    ) -> Result<probe_target::CommittedProbeTarget, PersistenceError> {
        if !snapshot.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::RevalidateProbeTarget {
            snapshot,
            respond_to,
        })
    }
    #[allow(dead_code)] // Internal consuming witness import; no product entry.
    pub(crate) fn import_probe_seed_witness(
        &self,
        envelope: crate::adapters::production_mutation::probe_preflight::seed::ProbeSeedEnvelope,
    ) -> Result<(), PersistenceError> {
        let (candidate, revision, owner) = envelope
            .into_parts()
            .map_err(|_| PersistenceError::StaleSyncCapture)?;
        if !candidate.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::ImportProbeSeedWitness {
            candidate,
            revision,
            owner,
            respond_to,
        })
    }
    #[allow(dead_code)]
    pub(crate) fn preview_probe_first_route(
        &self,
        candidate: crate::adapters::production_mutation::probe_preflight::first_route::FirstRouteCandidate,
    ) -> Result<probe_first_route::FirstRoutePreview, PersistenceError> {
        let parts = candidate
            .into_preview()
            .map_err(|_| PersistenceError::StaleSyncCapture)?;
        if parts.actor.upgrade().is_none()
            || !std::sync::Weak::ptr_eq(&parts.actor, &self.actor_provenance())
        {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::PreviewProbeFirstRoute { parts, respond_to })
    }
    pub(crate) fn begin_probe_source(
        &self,
        candidate: probe_target::CommittedProbeTarget,
    ) -> Result<
        (
            probe_target::CommittedProbeTarget,
            probe_ledger::ProbeLedgerToken,
        ),
        PersistenceError,
    > {
        let actor = self.actor_provenance();
        if !candidate.matches_actor(&actor) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::BeginProbeSource {
            candidate,
            actor,
            respond_to,
        })
    }
    pub(crate) fn finish_probe_source(
        &self,
        token: &probe_ledger::ProbeLedgerToken,
        retired: bool,
    ) -> Result<(), PersistenceError> {
        if !token.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        // Retain the caller's token across failures for exact local shutdown retry.
        self.request(|respond_to| Request::FinishProbeSource {
            scope: token.scope.clone(),
            retired,
            respond_to,
        })
    }
    pub(crate) fn probe_source_scope(
        &self,
        token: &probe_ledger::ProbeLedgerToken,
    ) -> Result<probe_ledger::ProbeLedgerScope, PersistenceError> {
        let scope = token.scope.clone();
        if !self.probe_source_active(&scope)? {
            return Err(PersistenceError::StaleSyncCapture);
        }
        Ok(scope)
    }
    pub(crate) fn probe_source_active(
        &self,
        scope: &probe_ledger::ProbeLedgerScope,
    ) -> Result<bool, PersistenceError> {
        if !scope.matches_actor(&self.actor_provenance()) {
            return Ok(false);
        }
        self.request(|respond_to| Request::ProbeSourceActive {
            scope: scope.clone(),
            respond_to,
        })
    }
    pub(crate) fn record_probe_source_closure(
        &self,
        proof: crate::application::attachment_runtime::ProbeLocalClosure<'_>,
    ) -> Result<probe_ledger::ProbeReconciliationEvidence, PersistenceError> {
        let token = proof.token();
        if !token.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::RecordProbeSourceClosure {
            scope: token.scope.clone(),
            respond_to,
        })
    }
    pub(crate) fn reconcile_probe_source(
        &self,
        evidence: probe_ledger::ProbeReconciliationEvidence,
    ) -> Result<(), PersistenceError> {
        if !evidence.scope.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::ReconcileProbeSource {
            evidence,
            respond_to,
        })
    }
    pub(crate) fn creation_reservation_snapshot(
        &self,
        account_id: String,
        operation_id: String,
    ) -> Result<
        crate::application::production_creation_owner::CreationReservationSnapshot,
        PersistenceError,
    > {
        self.request(|respond_to| Request::CreationReservationSnapshot {
            account_id,
            operation_id,
            respond_to,
        })
    }

    pub fn create_local_todo(
        &self,
        title: String,
        priority: Priority,
        important: bool,
    ) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::Create {
            title,
            priority,
            important,
            respond_to,
        })
    }
    pub fn list_local_todos(&self) -> Result<Vec<LocalTodo>, PersistenceError> {
        self.request(|respond_to| Request::List { respond_to })
    }
    pub fn list_local_calendars(&self) -> Result<Vec<LocalCalendar>, PersistenceError> {
        self.request(|respond_to| Request::ListLocalCalendars { respond_to })
    }
    pub fn save_local_calendar(
        &self,
        input: SaveLocalCalendar,
    ) -> Result<LocalCalendar, PersistenceError> {
        self.request(|respond_to| Request::SaveLocalCalendar { input, respond_to })
    }
    pub fn set_local_calendar_hidden(
        &self,
        calendar_id: String,
        hidden: bool,
    ) -> Result<LocalCalendar, PersistenceError> {
        self.request(|respond_to| Request::SetLocalCalendarHidden {
            calendar_id,
            hidden,
            respond_to,
        })
    }
    pub fn delete_local_calendar(
        &self,
        calendar_id: String,
    ) -> Result<crate::domain::local_todo::DeletedLocalCalendar, PersistenceError> {
        self.request(|respond_to| Request::DeleteLocalCalendar {
            calendar_id,
            respond_to,
        })
    }
    pub fn move_local_todo(
        &self,
        todo_id: String,
        calendar_id: String,
    ) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::MoveLocalTodo {
            todo_id,
            calendar_id,
            respond_to,
        })
    }
    pub fn desktop_item_order(&self) -> Result<Vec<String>, PersistenceError> {
        self.request(|respond_to| Request::DesktopOrder { respond_to })
    }
    pub fn set_desktop_item_order(&self, keys: Vec<String>) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::SetDesktopOrder { keys, respond_to })
    }
    pub fn reset_desktop_item_order(&self) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::ResetDesktopOrder { respond_to })
    }
    pub fn update_local_todo(&self, input: UpdateLocalTodo) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::Update { input, respond_to })
    }

    pub fn save_local_todo(&self, input: SaveLocalTodo) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::SaveLocalTodo { input, respond_to })
    }
    pub fn set_completed(
        &self,
        todo_id: String,
        occurrence_key: String,
        completed: bool,
    ) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::SetCompleted {
            todo_id,
            occurrence_key,
            completed,
            respond_to,
        })
    }
    pub fn create_subtask(&self, input: CreateSubtask) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::CreateSubtask { input, respond_to })
    }
    pub fn update_subtask(&self, input: UpdateSubtask) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::UpdateSubtask { input, respond_to })
    }
    pub fn move_local_subtask(
        &self,
        input: crate::domain::local_todo::MoveLocalSubtask,
    ) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::MoveLocalSubtask { input, respond_to })
    }
    pub fn set_subtask_completed(
        &self,
        todo_id: String,
        subtask_id: String,
        completed: bool,
    ) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::SetSubtaskCompleted {
            todo_id,
            subtask_id,
            completed,
            respond_to,
        })
    }
    pub fn delete_subtask(
        &self,
        todo_id: String,
        subtask_id: String,
    ) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::DeleteSubtask {
            todo_id,
            subtask_id,
            respond_to,
        })
    }
    pub fn soft_delete(&self, todo_id: String) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::SoftDelete {
            todo_id,
            respond_to,
        })
    }
    pub fn soft_delete_occurrence(
        &self,
        todo_id: String,
        occurrence_key: String,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::SoftDeleteOccurrence {
            todo_id,
            occurrence_key,
            respond_to,
        })
    }
    pub fn restore_recently_deleted_occurrence(
        &self,
        todo_id: String,
        occurrence_key: String,
    ) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::RestoreRecentlyDeletedOccurrence {
            todo_id,
            occurrence_key,
            respond_to,
        })
    }
    pub fn restore_recently_deleted(&self, todo_id: String) -> Result<LocalTodo, PersistenceError> {
        self.request(|respond_to| Request::RestoreRecentlyDeleted {
            todo_id,
            respond_to,
        })
    }
    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::Backup {
            destination: destination.as_ref().to_owned(),
            respond_to,
        })
    }
    pub fn schema_version(&self) -> Result<i64, PersistenceError> {
        self.request(|respond_to| Request::SchemaVersion { respond_to })
    }
    pub fn get_app_settings(&self) -> Result<AppSettings, PersistenceError> {
        self.request(|respond_to| Request::GetAppSettings { respond_to })
    }
    pub fn update_app_settings(
        &self,
        input: UpdateAppSettings,
    ) -> Result<AppSettings, PersistenceError> {
        self.request(|respond_to| Request::UpdateAppSettings { input, respond_to })
    }
    /// Store the desktop list's position lock.
    pub fn set_desktop_pinned(&self, pinned: bool) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::SetDesktopPinned { pinned, respond_to })
    }
    /// Store the desktop list's folded presentation choice.
    pub fn set_desktop_collapsed(&self, collapsed: bool) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::SetDesktopCollapsed {
            collapsed,
            respond_to,
        })
    }
    pub fn list_window_placements(&self) -> Result<Vec<StoredWindowPlacement>, PersistenceError> {
        self.request(|respond_to| Request::ListWindowPlacements { respond_to })
    }
    pub fn save_window_placement(
        &self,
        window_label: impl Into<String>,
        placement: WindowPlacement,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::SaveWindowPlacement {
            window_label: window_label.into(),
            placement,
            respond_to,
        })
    }
    pub fn upsert_account_profile(
        &self,
        input: RegisterAccountProfile,
    ) -> Result<AccountProfile, PersistenceError> {
        self.request(|respond_to| Request::UpsertAccountProfile { input, respond_to })
    }
    pub fn account_profile(&self, account_id: String) -> Result<AccountProfile, PersistenceError> {
        self.request(|respond_to| Request::AccountProfile {
            account_id,
            respond_to,
        })
    }
    pub fn list_account_profiles(&self) -> Result<Vec<AccountProfile>, PersistenceError> {
        self.request(|respond_to| Request::ListAccountProfiles { respond_to })
    }
    pub fn archive_account_profile(&self, account_id: String) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::ArchiveAccountProfile {
            account_id,
            respond_to,
        })
    }
    pub fn set_account_enabled(
        &self,
        account_id: String,
        enabled: bool,
    ) -> Result<AccountProfile, PersistenceError> {
        self.request(|respond_to| Request::SetAccountEnabled {
            account_id,
            enabled,
            respond_to,
        })
    }
    pub fn set_account_credential_configured(
        &self,
        account_id: String,
    ) -> Result<AccountProfile, PersistenceError> {
        self.request(|respond_to| Request::SetAccountCredentialConfigured {
            account_id,
            respond_to,
        })
    }
    pub fn set_account_remote_editing_enabled(
        &self,
        account_id: String,
        enabled: bool,
    ) -> Result<AccountProfile, PersistenceError> {
        self.request(|respond_to| Request::SetAccountRemoteEditingEnabled {
            account_id,
            enabled,
            respond_to,
        })
    }
    pub fn begin_sync_capture(&self, account_id: String) -> Result<SyncCapture, PersistenceError> {
        self.request(|respond_to| Request::BeginSyncCapture {
            account_id,
            include_requests: false,
            respond_to,
        })
        .map(|(capture, _)| capture)
    }

    pub fn begin_incremental_sync_capture(
        &self,
        account_id: String,
    ) -> Result<(SyncCapture, Vec<IncrementalSyncRequest>), PersistenceError> {
        self.request(|respond_to| Request::BeginSyncCapture {
            account_id,
            include_requests: true,
            respond_to,
        })
    }

    pub fn promote_captured_account_inventory(
        &self,
        capture: SyncCapture,
        stage: AccountInventoryStage,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::PromoteAccountInventory {
            capture,
            stage,
            respond_to,
        })
    }
    pub fn promote_captured_account_incremental(
        &self,
        capture: SyncCapture,
        stage: AccountIncrementalStage,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::PromoteAccountIncremental {
            capture,
            stage,
            respond_to,
        })
    }

    pub fn account_sync_requests(
        &self,
        account_id: String,
    ) -> Result<Vec<IncrementalSyncRequest>, PersistenceError> {
        self.request(|respond_to| Request::AccountSyncRequests {
            account_id,
            respond_to,
        })
    }
    pub fn list_cached_calendar_resources(
        &self,
    ) -> Result<Vec<CachedCalendarResource>, PersistenceError> {
        self.request(|respond_to| Request::ListCachedCalendarResources { respond_to })
    }

    pub fn conditional_title_update_evidence(
        &self,
        target: ConditionalTitleUpdateTarget,
    ) -> Result<ConditionalTitleUpdateEvidence, PersistenceError> {
        self.request(|respond_to| Request::ConditionalTitleUpdateEvidence { target, respond_to })
    }

    pub fn queue_remote_title_update(
        &self,
        input: QueueRemoteTitleUpdate,
    ) -> Result<RemoteMutationSummary, PersistenceError> {
        self.request(|respond_to| Request::QueueRemoteTitleUpdate { input, respond_to })
    }
    /// Actor-only event-v3 admission; no execution or product IPC is composed here.
    pub fn queue_remote_event_update_v3(
        &self,
        input: QueueRemoteEventUpdateV3,
    ) -> Result<RemoteMutationSummary, PersistenceError> {
        self.request(|respond_to| Request::QueueRemoteEventUpdateV3 { input, respond_to })
    }
    pub(crate) fn remote_recurrence_targets(
        &self,
    ) -> Result<Vec<RemoteRecurrenceTarget>, PersistenceError> {
        self.request(|respond_to| Request::RemoteRecurrenceTargets { respond_to })
    }
    pub(crate) fn admit_remote_recurrence_advance(
        &self,
        resource_id: String,
        connected: scoped_event::ConnectedOwnerCheck,
    ) -> Result<scoped_event::ScopedEventSnapshot, PersistenceError> {
        let actor = self.actor_provenance();
        self.request(|respond_to| Request::AdmitRemoteRecurrenceAdvance {
            resource_id,
            actor,
            connected,
            respond_to,
        })
    }
    pub(crate) fn select_confirmed_target(
        &self,
        input: scoped_event::ConfirmedTargetInput,
    ) -> Result<scoped_event::ConfirmedTarget, PersistenceError> {
        let actor = self.actor_provenance();
        self.request(|respond_to| Request::SelectConfirmedTarget {
            input,
            actor,
            respond_to,
        })
    }
    pub(crate) fn admit_confirmed_target(
        &self,
        input: scoped_event::ConfirmedTargetInput,
        target: scoped_event::ConfirmedTarget,
        connected: scoped_event::ConnectedOwnerCheck,
    ) -> Result<scoped_event::ScopedEventSnapshot, PersistenceError> {
        let actor = self.actor_provenance();
        self.request(|respond_to| Request::AdmitConfirmedTarget {
            input,
            target,
            actor,
            connected,
            respond_to,
        })
    }
    pub(crate) fn restore_scoped_event(
        &self,
        kind: crate::domain::remote_crud::RemoteCrudKind,
        operation_id: String,
        account_id: String,
        calendar_id: String,
        resource_id: String,
        connected: Option<scoped_event::ConnectedOwnerCheck>,
    ) -> Result<scoped_event::ScopedEventSnapshot, PersistenceError> {
        let actor = self.actor_provenance();
        self.request(|respond_to| Request::RehydrateScopedEvent {
            kind,
            operation_id,
            account_id,
            calendar_id,
            resource_id,
            actor,
            connected,
            respond_to,
        })
    }
    pub(crate) fn admit_connected_scoped_event(
        &self,
        input: QueueRemoteEventUpdateV3,
        confirmation: scoped_event::EventConfirmation,
        connected: Option<scoped_event::ConnectedOwnerCheck>,
    ) -> Result<scoped_event::ScopedEventSnapshot, PersistenceError> {
        let actor = self.actor_provenance();
        self.request(|respond_to| Request::AdmitScopedEvent {
            input,
            confirmation,
            actor,
            connected,
            respond_to,
        })
    }
    #[allow(dead_code)]
    pub(crate) fn claim_scoped_event(
        &self,
        scope: &scoped_event::ScopedEventSnapshot,
    ) -> Result<Option<RemoteEventMutationClaimV3>, PersistenceError> {
        if !scope.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::ClaimScopedEvent {
            scope: scope.clone(),
            request: None,
            respond_to,
        })
    }
    pub(crate) fn admit_connected_scoped_crud(
        &self,
        input: scoped_event::ScopedCrudInput,
        connected: Option<scoped_event::ConnectedOwnerCheck>,
    ) -> Result<scoped_event::ScopedEventSnapshot, PersistenceError> {
        let actor = self.actor_provenance();
        self.request(|respond_to| Request::AdmitScopedCrud {
            input,
            actor,
            connected,
            respond_to,
        })
    }
    pub(crate) fn claim_scoped_create(
        &self,
        scope: &scoped_event::ScopedEventSnapshot,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<Option<RemoteCreationClaim>, PersistenceError> {
        if !scope.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::ClaimScopedCreate {
            scope: scope.clone(),
            request: request.clone(),
            respond_to,
        })
    }
    pub(crate) fn claim_scoped_delete(
        &self,
        scope: &scoped_event::ScopedEventSnapshot,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<Option<RemoteDeletionClaim>, PersistenceError> {
        if !scope.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::ClaimScopedDelete {
            scope: scope.clone(),
            request: request.clone(),
            respond_to,
        })
    }
    pub(crate) fn claim_scoped_event_for_request(
        &self,
        scope: &scoped_event::ScopedEventSnapshot,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<Option<RemoteEventMutationClaimV3>, PersistenceError> {
        if !scope.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::ClaimScopedEvent {
            scope: scope.clone(),
            request: Some(request.clone()),
            respond_to,
        })
    }
    #[allow(dead_code)]
    pub(crate) fn scoped_event_send_snapshot(
        &self,
        scope: &scoped_event::ScopedEventSnapshot,
        claim: &RemoteEventMutationClaimV3,
    ) -> Result<RemoteEventUpdateSnapshotV3, PersistenceError> {
        if !scope.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::ScopedEventSendSnapshot {
            scope: scope.clone(),
            claim: claim.clone(),
            respond_to,
        })
    }
    #[allow(dead_code)]
    pub(crate) fn scoped_event_read_allowed(
        &self,
        scope: &scoped_event::ScopedEventSnapshot,
        claim: &RemoteEventMutationClaimV3,
    ) -> Result<bool, PersistenceError> {
        if !scope.matches_actor(&self.actor_provenance()) {
            return Ok(false);
        }
        self.request(|respond_to| Request::ScopedEventReadAllowed {
            scope: scope.clone(),
            claim: claim.clone(),
            respond_to,
        })
    }
    #[allow(dead_code)]
    pub(crate) fn scoped_event_status(
        &self,
        scope: &scoped_event::ScopedEventSnapshot,
    ) -> Result<crate::domain::remote_crud::RemoteCrudOperation, PersistenceError> {
        if !scope.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::ScopedEventStatus {
            scope: scope.clone(),
            respond_to,
        })
    }
    #[allow(dead_code)]
    pub(crate) fn resolve_scoped_event(
        &self,
        scope: &scoped_event::ScopedEventSnapshot,
        choice: ConflictResolutionChoice,
    ) -> Result<ConflictResolutionResult, PersistenceError> {
        self.resolve_connected_scoped_event(scope, choice, None)
    }
    pub(crate) fn resolve_connected_scoped_event(
        &self,
        scope: &scoped_event::ScopedEventSnapshot,
        choice: ConflictResolutionChoice,
        connected: Option<scoped_event::ConnectedOwnerCheck>,
    ) -> Result<ConflictResolutionResult, PersistenceError> {
        if !scope.matches_actor(&self.actor_provenance()) {
            return Err(PersistenceError::StaleSyncCapture);
        }
        self.request(|respond_to| Request::ResolveScopedEvent {
            scope: scope.clone(),
            choice,
            connected,
            respond_to,
        })
    }

    pub fn claim_account_remote_event_mutation(
        &self,
        account_id: String,
    ) -> Result<Option<RemoteEventMutationClaimV3>, PersistenceError> {
        self.request(|respond_to| Request::ClaimRemoteEvent {
            account_id,
            respond_to,
        })
    }
    // Actor-only event-v3 foundation; no worker or sender registration.
    #[allow(dead_code)]
    pub(crate) fn remote_event_read_allowed(
        &self,
        claim: &RemoteEventMutationClaimV3,
    ) -> Result<bool, PersistenceError> {
        self.request(|respond_to| Request::RemoteEventReadAllowed {
            claim: claim.clone(),
            respond_to,
        })
    }
    pub fn remote_event_send_snapshot(
        &self,
        claim: &RemoteEventMutationClaimV3,
    ) -> Result<RemoteEventUpdateSnapshotV3, PersistenceError> {
        self.request(|respond_to| Request::RemoteEventSnapshot {
            claim: claim.clone(),
            respond_to,
        })
    }
    pub fn defer_remote_event_mutation(
        &self,
        claim: RemoteEventMutationClaimV3,
        uncertain: bool,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::DeferRemoteEvent {
            claim,
            uncertain,
            respond_to,
        })
    }
    pub fn restore_remote_event_base(
        &self,
        claim: &RemoteEventMutationClaimV3,
        remote: RemoteReadBack,
    ) -> Result<RemoteEventMutationClaimV3, PersistenceError> {
        self.request(|respond_to| Request::RestoreRemoteEvent {
            claim: claim.clone(),
            remote,
            respond_to,
        })
    }
    /// Actor-only v2 admission; no execution or product IPC is composed here.
    pub fn queue_remote_event_text_update_v2(
        &self,
        input: QueueRemoteEventTextUpdateV2,
    ) -> Result<RemoteMutationSummary, PersistenceError> {
        self.request(|respond_to| Request::QueueRemoteEventTextUpdateV2 { input, respond_to })
    }

    pub fn claim_account_remote_fields_mutation(
        &self,
        account_id: String,
    ) -> Result<Option<RemoteFieldsMutationClaim>, PersistenceError> {
        self.request(|respond_to| Request::ClaimRemoteFields {
            account_id,
            respond_to,
        })
    }
    // Internal v2 path stays outside worker/product composition until B3c.
    #[allow(dead_code)]
    pub(crate) fn remote_fields_read_allowed(
        &self,
        claim: &RemoteFieldsMutationClaim,
    ) -> Result<bool, PersistenceError> {
        self.request(|respond_to| Request::RemoteFieldsReadAllowed {
            claim: claim.clone(),
            respond_to,
        })
    }
    pub fn remote_fields_send_snapshot(
        &self,
        claim: &RemoteFieldsMutationClaim,
    ) -> Result<RemoteFieldsSendSnapshot, PersistenceError> {
        self.request(|respond_to| Request::RemoteFieldsSnapshot {
            claim: claim.clone(),
            respond_to,
        })
    }
    pub fn defer_remote_fields_mutation(
        &self,
        claim: RemoteFieldsMutationClaim,
        uncertain: bool,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::DeferRemoteFields {
            claim,
            uncertain,
            respond_to,
        })
    }
    pub fn restore_remote_fields_base(
        &self,
        claim: &RemoteFieldsMutationClaim,
        remote: RemoteReadBack,
    ) -> Result<RemoteFieldsMutationClaim, PersistenceError> {
        self.request(|respond_to| Request::RestoreRemoteFields {
            claim: claim.clone(),
            remote,
            respond_to,
        })
    }
    pub fn apply_remote_fields_mutation(
        &self,
        claim: RemoteFieldsMutationClaim,
        remote: RemoteReadBack,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::FinishRemoteFields {
            claim,
            remote,
            applied: true,
            respond_to,
        })
    }
    pub fn conflict_remote_fields_mutation(
        &self,
        claim: RemoteFieldsMutationClaim,
        remote: RemoteReadBack,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::FinishRemoteFields {
            claim,
            remote,
            applied: false,
            respond_to,
        })
    }
    pub fn resolve_remote_fields_conflict(
        &self,
        operation_id: String,
        choice: ConflictResolutionChoice,
    ) -> Result<ConflictResolutionResult, PersistenceError> {
        self.request(|respond_to| Request::ResolveRemoteFields {
            operation_id,
            choice,
            respond_to,
        })
    }
    pub fn apply_remote_event_mutation(
        &self,
        claim: RemoteEventMutationClaimV3,
        remote: RemoteReadBack,
    ) -> Result<(), PersistenceError> {
        self.apply_remote_event_mutation_with_schedule(claim, remote)
            .map(|_| ())
    }
    pub(crate) fn apply_remote_event_mutation_with_schedule(
        &self,
        claim: RemoteEventMutationClaimV3,
        remote: RemoteReadBack,
    ) -> Result<CommittedEventScheduleChange, PersistenceError> {
        self.apply_remote_event_mutation_scoped_schedule(claim, remote, std::sync::Weak::new())
    }
    pub(crate) fn apply_remote_event_mutation_for_request(
        &self,
        claim: RemoteEventMutationClaimV3,
        remote: RemoteReadBack,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<CommittedEventScheduleChange, PersistenceError> {
        self.request(|respond_to| Request::FinishRemoteEvent {
            actor: self.actor_provenance(),
            request: request.event_schedule_provenance(),
            scope_request: request.event_scope().map(|_| request.clone()),
            claim,
            remote,
            applied: true,
            respond_to,
        })
    }
    pub(crate) fn restore_remote_event_base_for_request(
        &self,
        claim: &RemoteEventMutationClaimV3,
        remote: RemoteReadBack,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<RemoteEventMutationClaimV3, PersistenceError> {
        if request.event_scope().is_none() {
            return self.restore_remote_event_base(claim, remote);
        }
        self.request(|respond_to| Request::RestoreScopedEvent {
            claim: claim.clone(),
            remote,
            request: request.clone(),
            respond_to,
        })
    }
    fn apply_remote_event_mutation_scoped_schedule(
        &self,
        claim: RemoteEventMutationClaimV3,
        remote: RemoteReadBack,
        request: std::sync::Weak<()>,
    ) -> Result<CommittedEventScheduleChange, PersistenceError> {
        self.request(|respond_to| Request::FinishRemoteEvent {
            actor: self.actor_provenance(),
            request,
            scope_request: None,
            claim,
            remote,
            applied: true,
            respond_to,
        })
    }
    pub fn conflict_remote_event_mutation(
        &self,
        claim: RemoteEventMutationClaimV3,
        remote: RemoteReadBack,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::FinishRemoteEvent {
            actor: self.actor_provenance(),
            request: std::sync::Weak::new(),
            scope_request: None,
            claim,
            remote,
            applied: false,
            respond_to,
        })
        .map(|_| ())
    }
    pub(crate) fn conflict_remote_event_mutation_for_request(
        &self,
        claim: RemoteEventMutationClaimV3,
        remote: RemoteReadBack,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::FinishRemoteEvent {
            actor: self.actor_provenance(),
            request: std::sync::Weak::new(),
            scope_request: request.event_scope().map(|_| request.clone()),
            claim,
            remote,
            applied: false,
            respond_to,
        })
        .map(|_| ())
    }
    pub fn resolve_remote_event_conflict(
        &self,
        operation_id: String,
        choice: ConflictResolutionChoice,
    ) -> Result<ConflictResolutionResult, PersistenceError> {
        self.request(|respond_to| Request::ResolveRemoteEvent {
            operation_id,
            choice,
            respond_to,
        })
    }
    pub fn queue_remote_event_delete(
        &self,
        input: QueueRemoteEventDelete,
    ) -> Result<RemoteMutationSummary, PersistenceError> {
        self.request(|respond_to| Request::QueueRemoteEventDelete { input, respond_to })
    }
    pub(crate) fn queue_remote_crud_delete(
        &self,
        input: QueueRemoteEventDelete,
    ) -> Result<RemoteMutationSummary, PersistenceError> {
        self.request(|respond_to| Request::QueueRemoteCrudDelete { input, respond_to })
    }
    /// Actor-only utc-v2 admission; no product dispatch composition.
    pub fn queue_remote_event_create_v2(
        &self,
        input: QueueRemoteEventCreateV2,
    ) -> Result<RemoteCreationSummary, PersistenceError> {
        self.request(|respond_to| Request::QueueRemoteEventCreateV2 { input, respond_to })
    }
    pub fn queue_remote_event_create(
        &self,
        input: QueueRemoteEventCreate,
    ) -> Result<RemoteCreationSummary, PersistenceError> {
        self.request(|respond_to| Request::QueueRemoteEventCreate { input, respond_to })
    }
    pub(crate) fn remote_crud_status(
        &self,
        account: String,
        kind: crate::domain::remote_crud::RemoteCrudKind,
    ) -> Result<crate::domain::remote_crud::RemoteCrudStatus, PersistenceError> {
        self.request(|respond_to| Request::RemoteCrudStatus {
            account,
            kind,
            respond_to,
        })
    }
    pub(crate) fn resolve_remote_crud_conflict(
        &self,
        kind: crate::domain::remote_crud::RemoteCrudKind,
        operation: String,
        choice: ConflictResolutionChoice,
    ) -> Result<ConflictResolutionResult, PersistenceError> {
        self.request(|respond_to| Request::ResolveRemoteCrudConflict {
            kind,
            operation,
            choice,
            respond_to,
        })
    }

    pub fn remote_creation_account_view(
        &self,
        account_id: String,
    ) -> Result<crate::domain::remote_creation::RemoteCreationAccountView, PersistenceError> {
        self.request(|respond_to| Request::RemoteCreationAccountView {
            account_id,
            respond_to,
        })
    }
    pub(crate) fn select_account_remote_creation(
        &self,
        account_id: String,
    ) -> Result<Option<String>, PersistenceError> {
        self.request(|respond_to| Request::SelectRemoteCreation {
            account_id,
            version: "utc-v1",
            respond_to,
        })
    }
    pub(crate) fn select_account_remote_creation_v2(
        &self,
        account_id: String,
    ) -> Result<Option<String>, PersistenceError> {
        self.request(|respond_to| Request::SelectRemoteCreation {
            account_id,
            version: "utc-v2",
            respond_to,
        })
    }
    pub fn claim_account_remote_creation(
        &self,
        account_id: String,
    ) -> Result<Option<RemoteCreationClaim>, PersistenceError> {
        self.request(|respond_to| Request::ClaimRemoteCreation {
            account_id,
            legacy_only: false,
            respond_to,
        })
    }
    /// Reviewed production dispatcher consumes utc-v1 only.
    pub(crate) fn claim_account_remote_creation_v1(
        &self,
        account_id: String,
    ) -> Result<Option<RemoteCreationClaim>, PersistenceError> {
        self.request(|respond_to| Request::ClaimRemoteCreation {
            account_id,
            legacy_only: true,
            respond_to,
        })
    }
    #[allow(dead_code)]
    pub(crate) fn claim_account_remote_creation_v2(
        &self,
        account_id: String,
    ) -> Result<Option<RemoteCreationClaim>, PersistenceError> {
        self.request(|respond_to| Request::ClaimRemoteCreationV2 {
            account_id,
            respond_to,
        })
    }
    pub fn remote_creation_read_allowed(
        &self,
        claim: RemoteCreationClaim,
    ) -> Result<bool, PersistenceError> {
        self.request(|respond_to| Request::RemoteCreationReadAllowed { claim, respond_to })
    }
    /// Internal reservation only. No IPC, sender, or production dispatch authority.
    pub fn record_remote_creation_first_send(
        &self,
        claim: RemoteCreationClaim,
    ) -> Result<(RemoteCreationClaim, RemoteCreationAttemptEvidence), PersistenceError> {
        self.request(|respond_to| Request::RecordRemoteCreationFirstSend { claim, respond_to })
    }
    /// Read-only final snapshot; only consuming transport may spend the evidence.
    #[allow(dead_code)]
    pub(crate) fn remote_creation_send_allowed(
        &self,
        claim: RemoteCreationClaim,
        evidence: &RemoteCreationAttemptEvidence,
    ) -> Result<bool, PersistenceError> {
        if !evidence.matches_claim(&claim) {
            return Ok(false);
        }
        self.request(|respond_to| Request::RemoteCreationSendAllowed {
            claim,
            attempt_id: evidence.attempt_id,
            attempted_at_ms: evidence.attempted_at_ms,
            respond_to,
        })
    }
    pub fn observe_remote_creation(
        &self,
        claim: RemoteCreationClaim,
        evidence: crate::domain::remote_creation::RemoteCreationReadBack,
    ) -> Result<crate::domain::remote_creation::RemoteCreationObservation, PersistenceError> {
        self.request(|respond_to| Request::ObserveRemoteCreation {
            claim,
            evidence,
            respond_to,
        })
    }
    /// Internal recovery only; not registered as IPC or production execution.
    pub fn complete_remote_creation(
        &self,
        claim: RemoteCreationClaim,
        remote: RemoteReadBack,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::CompleteRemoteCreation {
            actor: std::sync::Weak::new(),
            request: std::sync::Weak::new(),
            claim,
            remote,
            respond_to,
        })
        .map(|_| ())
    }
    /// Request-scoped create completion that also publishes committed schedule
    /// evidence. Only the actor can mint it, and only for the transaction that
    /// actually reconciled this series' reminders.
    pub(crate) fn complete_remote_creation_for_request(
        &self,
        claim: RemoteCreationClaim,
        remote: RemoteReadBack,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<CommittedEventScheduleChange, PersistenceError> {
        self.request(|respond_to| Request::CompleteRemoteCreation {
            actor: self.actor_provenance(),
            request: request.event_schedule_provenance(),
            claim,
            remote,
            respond_to,
        })
    }
    pub fn defer_remote_creation(
        &self,
        claim: RemoteCreationClaim,
        uncertain_delivery: bool,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::DeferRemoteCreation {
            claim,
            uncertain_delivery,
            respond_to,
        })
    }
    pub fn claim_account_remote_deletion(
        &self,
        account_id: String,
        now_ms: i64,
    ) -> Result<Option<RemoteDeletionClaim>, PersistenceError> {
        self.request(|respond_to| Request::ClaimRemoteDeletion {
            account_id,
            now_ms,
            respond_to,
        })
    }
    pub fn remote_deletion_send_allowed(
        &self,
        claim_token: String,
        now_ms: i64,
    ) -> Result<bool, PersistenceError> {
        self.request(|respond_to| Request::RemoteDeletionSendAllowed {
            claim_token,
            now_ms,
            respond_to,
        })
    }
    pub(crate) fn remote_deletion_read_allowed(
        &self,
        claim_token: String,
        now_ms: i64,
    ) -> Result<bool, PersistenceError> {
        self.request(|respond_to| Request::RemoteDeletionReadAllowed {
            claim_token,
            now_ms,
            respond_to,
        })
    }
    pub fn defer_remote_deletion(
        &self,
        claim_token: String,
        ambiguous: bool,
        outcome: &'static str,
        now_ms: i64,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::DeferRemoteDeletion {
            claim_token,
            ambiguous,
            outcome,
            now_ms,
            respond_to,
        })
    }
    pub fn finish_remote_deletion(
        &self,
        claim_token: String,
        evidence: RemoteDeletionReadBack,
        now_ms: i64,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::FinishRemoteDeletion {
            claim_token,
            evidence,
            now_ms,
            actor: self.actor_provenance(),
            request: None,
            respond_to,
        })
        .map(|_| ())
    }
    pub(crate) fn finish_remote_deletion_for_request(
        &self,
        claim_token: String,
        evidence: RemoteDeletionReadBack,
        now_ms: i64,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<CommittedEventScheduleChange, PersistenceError> {
        self.request(|respond_to| Request::FinishRemoteDeletion {
            claim_token,
            evidence,
            now_ms,
            actor: self.actor_provenance(),
            request: Some(request.clone()),
            respond_to,
        })
    }
    pub fn list_remote_mutations(&self) -> Result<Vec<RemoteMutationSummary>, PersistenceError> {
        self.request(|respond_to| Request::ListRemoteMutations { respond_to })
    }
    pub fn list_remote_conflicts(&self) -> Result<Vec<RemoteConflictSummary>, PersistenceError> {
        self.request(|respond_to| Request::ListRemoteConflicts { respond_to })
    }
    pub fn claim_remote_mutation(
        &self,
        now_ms: i64,
    ) -> Result<Option<RemoteMutationClaim>, PersistenceError> {
        self.claim_remote_mutation_for_account(None, now_ms)
    }
    pub fn claim_account_remote_mutation(
        &self,
        account_id: String,
        now_ms: i64,
    ) -> Result<Option<RemoteMutationClaim>, PersistenceError> {
        self.claim_remote_mutation_for_account(Some(account_id), now_ms)
    }
    fn claim_remote_mutation_for_account(
        &self,
        account_id: Option<String>,
        now_ms: i64,
    ) -> Result<Option<RemoteMutationClaim>, PersistenceError> {
        self.request(|respond_to| Request::ClaimRemoteMutation {
            account_id,
            now_ms,
            respond_to,
        })
    }
    pub(crate) fn remote_mutation_send_allowed(
        &self,
        claim_token: String,
        now_ms: i64,
    ) -> Result<bool, PersistenceError> {
        self.request(|respond_to| Request::RemoteMutationSendAllowed {
            claim_token,
            now_ms,
            respond_to,
        })
    }
    pub fn defer_remote_mutation(
        &self,
        claim_token: String,
        ambiguous: bool,
        outcome: &'static str,
        now_ms: i64,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::DeferRemoteMutation {
            claim_token,
            ambiguous,
            outcome,
            now_ms,
            respond_to,
        })
    }
    pub fn conflict_remote_mutation(
        &self,
        claim_token: String,
        remote: RemoteReadBack,
        now_ms: i64,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::ConflictRemoteMutation {
            claim_token,
            remote,
            now_ms,
            respond_to,
        })
    }
    pub fn apply_remote_mutation(
        &self,
        claim_token: String,
        remote: RemoteReadBack,
        now_ms: i64,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::ApplyRemoteMutation {
            claim_token,
            remote,
            now_ms,
            respond_to,
        })
    }
    pub fn resolve_remote_conflict(
        &self,
        operation_id: String,
        choice: ConflictResolutionChoice,
        now_ms: i64,
    ) -> Result<ConflictResolutionResult, PersistenceError> {
        self.request(|respond_to| Request::ResolveRemoteConflict {
            operation_id,
            choice,
            now_ms,
            respond_to,
        })
    }
    pub fn list_calendar_agenda_items(&self) -> Result<Vec<CalendarAgendaItem>, PersistenceError> {
        self.request(|respond_to| Request::ListCalendarAgendaItems { respond_to })
    }
    pub fn list_calendar_agenda_items_in_display_window(
        &self,
        now_utc_ms: i64,
    ) -> Result<Vec<CalendarAgendaItem>, PersistenceError> {
        self.request(
            |respond_to| Request::ListCalendarAgendaItemsInDisplayWindow {
                now_utc_ms,
                respond_to,
            },
        )
    }
    pub fn list_calendar_settings(&self) -> Result<Vec<CalendarSettingsView>, PersistenceError> {
        self.request(|respond_to| Request::ListCalendarSettings { respond_to })
    }
    pub fn set_calendar_color_override(
        &self,
        calendar_id: String,
        color: Option<String>,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::SetCalendarColorOverride {
            calendar_id,
            color,
            respond_to,
        })
    }
    pub fn set_calendar_visible(
        &self,
        account_id: String,
        calendar_id: String,
        visible: bool,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::SetCalendarVisible {
            account_id,
            calendar_id,
            visible,
            respond_to,
        })
    }
    pub fn set_calendar_display_range(
        &self,
        account_id: String,
        calendar_id: String,
        display_past_days: Option<i64>,
        display_future_days: Option<i64>,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::SetCalendarDisplayRange {
            account_id,
            calendar_id,
            display_past_days,
            display_future_days,
            respond_to,
        })
    }
    pub fn replace_discovered_calendars(
        &self,
        account_id: String,
        calendars: Vec<DiscoveredCalendarRecord>,
        observed_at_ms: i64,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::ReplaceDiscoveredCalendars {
            account_id,
            calendars,
            binding: None,
            observed_at_ms,
            respond_to,
        })
    }
    pub fn available_calendar_ids_by_url(
        &self,
        account_id: String,
    ) -> Result<BTreeMap<String, String>, PersistenceError> {
        self.request(|respond_to| Request::AvailableCalendarIdsByUrl {
            account_id,
            respond_to,
        })
    }
    pub fn refresh_calendar_projections(
        &self,
        now_ms: i64,
        horizon: RecurrenceHorizon,
    ) -> Result<CalendarProjectionRefreshSummary, PersistenceError> {
        self.request(|respond_to| Request::RefreshCalendarProjections {
            now_ms,
            horizon,
            respond_to,
        })
    }
    pub fn set_calendar_completed(
        &self,
        series_id: String,
        recurrence_key: String,
        completed: bool,
    ) -> Result<CalendarAgendaItem, PersistenceError> {
        self.request(|respond_to| Request::SetCalendarCompleted {
            series_id,
            recurrence_key,
            completed,
            respond_to,
        })
    }
    pub fn set_calendar_important(
        &self,
        series_id: String,
        recurrence_key: String,
        important: bool,
    ) -> Result<CalendarAgendaItem, PersistenceError> {
        self.request(|respond_to| Request::SetCalendarImportant {
            series_id,
            recurrence_key,
            important,
            respond_to,
        })
    }
    pub fn update_calendar_local_metadata(
        &self,
        input: UpdateCalendarLocalMetadata,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::UpdateCalendarLocalMetadata { input, respond_to })
    }
    pub fn create_calendar_subtask(
        &self,
        input: CreateCalendarSubtask,
    ) -> Result<CalendarAgendaItem, PersistenceError> {
        self.request(|respond_to| Request::CreateCalendarSubtask { input, respond_to })
    }
    pub fn update_calendar_subtask(
        &self,
        input: UpdateCalendarSubtask,
    ) -> Result<CalendarAgendaItem, PersistenceError> {
        self.request(|respond_to| Request::UpdateCalendarSubtask { input, respond_to })
    }
    pub fn set_calendar_subtask_completed(
        &self,
        series_id: String,
        recurrence_key: String,
        subtask_id: String,
        completed: bool,
    ) -> Result<CalendarAgendaItem, PersistenceError> {
        self.request(|respond_to| Request::SetCalendarSubtaskCompleted {
            series_id,
            recurrence_key,
            subtask_id,
            completed,
            respond_to,
        })
    }
    pub fn delete_calendar_subtask(
        &self,
        series_id: String,
        recurrence_key: String,
        subtask_id: String,
    ) -> Result<CalendarAgendaItem, PersistenceError> {
        self.request(|respond_to| Request::DeleteCalendarSubtask {
            series_id,
            recurrence_key,
            subtask_id,
            respond_to,
        })
    }
    pub fn reconcile_deletion_evidence(
        &self,
        capture: SyncCapture,
        reconciliation: AccountDeletionReconciliation,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::ReconcileDeletionEvidence {
            capture,
            reconciliation,
            respond_to,
        })
    }
    /// Reclaim the raw cache of resources whose remote deletion was confirmed
    /// at least the documented retention window before `now_ms`. Returns the
    /// number of reclaimed resources; at most one bounded batch per call.
    pub fn purge_deleted_raw_cache(&self, now_ms: i64) -> Result<u32, PersistenceError> {
        self.request(|respond_to| Request::PurgeDeletedRawCache { now_ms, respond_to })
    }
    pub fn calendar_sync_token(
        &self,
        calendar_id: String,
    ) -> Result<Option<String>, PersistenceError> {
        self.request(|respond_to| Request::CalendarSyncToken {
            calendar_id,
            respond_to,
        })
    }
    pub fn record_sync_run(
        &self,
        account_id: String,
        trigger: SyncTrigger,
        outcome: SyncOutcome,
        failure_class: SyncFailureClass,
        calendar_count: u32,
        resource_count: u32,
    ) -> Result<SyncRunSummary, PersistenceError> {
        self.request(|respond_to| Request::RecordSyncRun {
            account_id,
            trigger,
            outcome,
            failure_class,
            calendar_count,
            resource_count,
            respond_to,
        })
    }
    pub fn list_sync_runs(&self) -> Result<Vec<SyncRunSummary>, PersistenceError> {
        self.request(|respond_to| Request::ListSyncRuns { respond_to })
    }

    pub(crate) fn next_notification_deadline(&self) -> Result<Option<i64>, PersistenceError> {
        self.request(|respond_to| Request::NextNotificationDeadline { respond_to })
    }

    /// Read-only, actor-clock deadline evidence; no job content, claim or replan.
    pub(crate) fn next_account_notification_deadline(
        &self,
        account: String,
    ) -> Result<Option<i64>, PersistenceError> {
        self.request(|respond_to| Request::NextAccountNotificationDeadline {
            account,
            respond_to,
        })
    }

    pub fn reconcile_notification_jobs(
        &self,
        now_ms: i64,
    ) -> Result<Option<i64>, PersistenceError> {
        self.request(|respond_to| Request::ReconcileNotificationJobs { now_ms, respond_to })
    }

    pub fn claim_due_notification_jobs(
        &self,
        now_ms: i64,
    ) -> Result<Vec<NotificationJob>, PersistenceError> {
        self.request(|respond_to| Request::ClaimDueNotificationJobs { now_ms, respond_to })
    }

    pub fn finish_notification_job(
        &self,
        job_id: String,
        delivered: bool,
        now_ms: i64,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::FinishNotificationJob {
            job_id,
            delivered,
            now_ms,
            respond_to,
        })
    }

    fn request<T>(
        &self,
        build: impl FnOnce(Sender<Result<T, PersistenceError>>) -> Request,
    ) -> Result<T, PersistenceError> {
        let (respond_to, response) = mpsc::channel();
        self.sender
            .send(build(respond_to))
            .map_err(|_| PersistenceError::ActorUnavailable)?;
        response
            .recv()
            .map_err(|_| PersistenceError::ActorUnavailable)?
    }
}

enum Request {
    ProbeSourceActive {
        scope: probe_ledger::ProbeLedgerScope,
        respond_to: Sender<Result<bool, PersistenceError>>,
    },
    RecordProbeSourceClosure {
        scope: probe_ledger::ProbeLedgerScope,
        respond_to: Sender<Result<probe_ledger::ProbeReconciliationEvidence, PersistenceError>>,
    },
    ReconcileProbeSource {
        evidence: probe_ledger::ProbeReconciliationEvidence,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    BeginProbeSource {
        candidate: probe_target::CommittedProbeTarget,
        actor: std::sync::Weak<()>,
        respond_to: Sender<
            Result<
                (
                    probe_target::CommittedProbeTarget,
                    probe_ledger::ProbeLedgerToken,
                ),
                PersistenceError,
            >,
        >,
    },
    FinishProbeSource {
        scope: probe_ledger::ProbeLedgerScope,
        retired: bool,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    ProbeTargetSnapshot {
        selection: probe_target::ProbeTargetSelection,
        actor: std::sync::Weak<()>,
        respond_to: Sender<Result<probe_target::CommittedProbeTarget, PersistenceError>>,
    },
    ExactProbeCollections {
        actor: std::sync::Weak<()>,
        respond_to: Sender<Result<probe_target::CommittedExactProbeCollections, PersistenceError>>,
    },
    RevalidateExactProbeCollections {
        snapshot: probe_target::CommittedExactProbeCollections,
        respond_to: Sender<Result<probe_target::CommittedExactProbeCollections, PersistenceError>>,
    },
    ImportProbeSeedWitness {
        candidate: probe_target::CommittedProbeTarget,
        revision: String,
        owner: crate::adapters::production_mutation::probe_preflight::seed::ProbeSeedOwner,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    PreviewProbeFirstRoute {
        parts: crate::adapters::production_mutation::probe_preflight::first_route::FirstRoutePreviewParts,
        respond_to: Sender<Result<probe_first_route::FirstRoutePreview, PersistenceError>>,
    },
    RevalidateProbeTarget {
        snapshot: probe_target::CommittedProbeTarget,
        respond_to: Sender<Result<probe_target::CommittedProbeTarget, PersistenceError>>,
    },
    QueueRemoteCrudDelete {
        input: QueueRemoteEventDelete,
        respond_to: Sender<Result<RemoteMutationSummary, PersistenceError>>,
    },
    RemoteCrudStatus {
        account: String,
        kind: crate::domain::remote_crud::RemoteCrudKind,
        respond_to: Sender<Result<crate::domain::remote_crud::RemoteCrudStatus, PersistenceError>>,
    },
    ResolveRemoteCrudConflict {
        kind: crate::domain::remote_crud::RemoteCrudKind,
        operation: String,
        choice: ConflictResolutionChoice,
        respond_to: Sender<Result<ConflictResolutionResult, PersistenceError>>,
    },
    NextNotificationDeadline {
        respond_to: Sender<Result<Option<i64>, PersistenceError>>,
    },
    NextAccountNotificationDeadline {
        account: String,
        respond_to: Sender<Result<Option<i64>, PersistenceError>>,
    },
    ReconcileNotificationJobs {
        now_ms: i64,
        respond_to: Sender<Result<Option<i64>, PersistenceError>>,
    },
    ClaimDueNotificationJobs {
        now_ms: i64,
        respond_to: Sender<Result<Vec<NotificationJob>, PersistenceError>>,
    },
    FinishNotificationJob {
        job_id: String,
        delivered: bool,
        now_ms: i64,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    Create {
        title: String,
        priority: Priority,
        important: bool,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    List {
        respond_to: Sender<Result<Vec<LocalTodo>, PersistenceError>>,
    },
    ListLocalCalendars {
        respond_to: Sender<Result<Vec<LocalCalendar>, PersistenceError>>,
    },
    SaveLocalCalendar {
        input: SaveLocalCalendar,
        respond_to: Sender<Result<LocalCalendar, PersistenceError>>,
    },
    SetLocalCalendarHidden {
        calendar_id: String,
        hidden: bool,
        respond_to: Sender<Result<LocalCalendar, PersistenceError>>,
    },
    DeleteLocalCalendar {
        calendar_id: String,
        respond_to:
            Sender<Result<crate::domain::local_todo::DeletedLocalCalendar, PersistenceError>>,
    },
    MoveLocalTodo {
        todo_id: String,
        calendar_id: String,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    DesktopOrder {
        respond_to: Sender<Result<Vec<String>, PersistenceError>>,
    },
    SetDesktopOrder {
        keys: Vec<String>,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    ResetDesktopOrder {
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    Update {
        input: UpdateLocalTodo,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    SaveLocalTodo {
        input: SaveLocalTodo,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    SetCompleted {
        todo_id: String,
        occurrence_key: String,
        completed: bool,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    CreateSubtask {
        input: CreateSubtask,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    UpdateSubtask {
        input: UpdateSubtask,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    MoveLocalSubtask {
        input: crate::domain::local_todo::MoveLocalSubtask,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    SetSubtaskCompleted {
        todo_id: String,
        subtask_id: String,
        completed: bool,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    DeleteSubtask {
        todo_id: String,
        subtask_id: String,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    SoftDelete {
        todo_id: String,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    SoftDeleteOccurrence { todo_id: String, occurrence_key: String, respond_to: Sender<Result<(), PersistenceError>> },
    RestoreRecentlyDeletedOccurrence { todo_id: String, occurrence_key: String, respond_to: Sender<Result<LocalTodo, PersistenceError>> },
    RestoreRecentlyDeleted {
        todo_id: String,
        respond_to: Sender<Result<LocalTodo, PersistenceError>>,
    },
    Backup {
        destination: PathBuf,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    SchemaVersion {
        respond_to: Sender<Result<i64, PersistenceError>>,
    },
    GetAppSettings {
        respond_to: Sender<Result<AppSettings, PersistenceError>>,
    },
    UpdateAppSettings {
        input: UpdateAppSettings,
        respond_to: Sender<Result<AppSettings, PersistenceError>>,
    },
    SetDesktopPinned {
        pinned: bool,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    SetDesktopCollapsed {
        collapsed: bool,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    ListWindowPlacements {
        respond_to: Sender<Result<Vec<StoredWindowPlacement>, PersistenceError>>,
    },
    SaveWindowPlacement {
        window_label: String,
        placement: WindowPlacement,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    UpsertAccountProfile {
        input: RegisterAccountProfile,
        respond_to: Sender<Result<AccountProfile, PersistenceError>>,
    },
    AccountProfile {
        account_id: String,
        respond_to: Sender<Result<AccountProfile, PersistenceError>>,
    },
    ListAccountProfiles {
        respond_to: Sender<Result<Vec<AccountProfile>, PersistenceError>>,
    },
    ArchiveAccountProfile {
        account_id: String,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    SetAccountEnabled {
        account_id: String,
        enabled: bool,
        respond_to: Sender<Result<AccountProfile, PersistenceError>>,
    },
    SetAccountCredentialConfigured {
        account_id: String,
        respond_to: Sender<Result<AccountProfile, PersistenceError>>,
    },
    SetAccountRemoteEditingEnabled {
        account_id: String,
        enabled: bool,
        respond_to: Sender<Result<AccountProfile, PersistenceError>>,
    },
    BeginSyncCapture {
        account_id: String,
        include_requests: bool,
        respond_to: Sender<Result<(SyncCapture, Vec<IncrementalSyncRequest>), PersistenceError>>,
    },
    PromoteAccountInventory {
        capture: SyncCapture,
        stage: AccountInventoryStage,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    PromoteAccountIncremental {
        capture: SyncCapture,
        stage: AccountIncrementalStage,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    AccountSyncRequests {
        account_id: String,
        respond_to: Sender<Result<Vec<IncrementalSyncRequest>, PersistenceError>>,
    },
    ListCachedCalendarResources {
        respond_to: Sender<Result<Vec<CachedCalendarResource>, PersistenceError>>,
    },
    ConditionalTitleUpdateEvidence {
        target: ConditionalTitleUpdateTarget,
        respond_to: Sender<Result<ConditionalTitleUpdateEvidence, PersistenceError>>,
    },
    QueueRemoteTitleUpdate {
        input: QueueRemoteTitleUpdate,
        respond_to: Sender<Result<RemoteMutationSummary, PersistenceError>>,
    },
    QueueRemoteEventUpdateV3 {
        input: QueueRemoteEventUpdateV3,
        respond_to: Sender<Result<RemoteMutationSummary, PersistenceError>>,
    },
    RemoteRecurrenceTargets {
        respond_to: Sender<Result<Vec<RemoteRecurrenceTarget>, PersistenceError>>,
    },
    AdmitRemoteRecurrenceAdvance {
        resource_id: String,
        actor: std::sync::Weak<()>,
        connected: scoped_event::ConnectedOwnerCheck,
        respond_to: Sender<Result<scoped_event::ScopedEventSnapshot, PersistenceError>>,
    },
    SelectConfirmedTarget { input: scoped_event::ConfirmedTargetInput, actor: std::sync::Weak<()>, respond_to: Sender<Result<scoped_event::ConfirmedTarget, PersistenceError>> },
    AdmitConfirmedTarget { input: scoped_event::ConfirmedTargetInput, target: scoped_event::ConfirmedTarget, actor: std::sync::Weak<()>, connected: scoped_event::ConnectedOwnerCheck, respond_to: Sender<Result<scoped_event::ScopedEventSnapshot, PersistenceError>> },
    RehydrateScopedEvent { kind:crate::domain::remote_crud::RemoteCrudKind,operation_id:String,account_id:String,calendar_id:String,resource_id:String,actor:std::sync::Weak<()>,connected:Option<scoped_event::ConnectedOwnerCheck>,respond_to:Sender<Result<scoped_event::ScopedEventSnapshot,PersistenceError>> },
    AdmitScopedCrud { input:scoped_event::ScopedCrudInput,actor:std::sync::Weak<()>,connected:Option<scoped_event::ConnectedOwnerCheck>,respond_to:Sender<Result<scoped_event::ScopedEventSnapshot,PersistenceError>> },
    ClaimScopedCreate {scope:scoped_event::ScopedEventSnapshot,request:crate::application::remote_mutation_runtime::MutationRequestGate,respond_to:Sender<Result<Option<RemoteCreationClaim>,PersistenceError>>},
    ClaimScopedDelete {scope:scoped_event::ScopedEventSnapshot,request:crate::application::remote_mutation_runtime::MutationRequestGate,respond_to:Sender<Result<Option<RemoteDeletionClaim>,PersistenceError>>},
    AdmitScopedEvent { input: QueueRemoteEventUpdateV3, confirmation: scoped_event::EventConfirmation, actor: std::sync::Weak<()>, connected:Option<scoped_event::ConnectedOwnerCheck>, respond_to: Sender<Result<scoped_event::ScopedEventSnapshot, PersistenceError>> },
    ClaimScopedEvent { scope: scoped_event::ScopedEventSnapshot, request:Option<crate::application::remote_mutation_runtime::MutationRequestGate>, respond_to: Sender<Result<Option<RemoteEventMutationClaimV3>, PersistenceError>> },
    ScopedEventSendSnapshot { scope: scoped_event::ScopedEventSnapshot, claim: RemoteEventMutationClaimV3, respond_to: Sender<Result<RemoteEventUpdateSnapshotV3, PersistenceError>> },
    ScopedEventReadAllowed { scope: scoped_event::ScopedEventSnapshot, claim: RemoteEventMutationClaimV3, respond_to: Sender<Result<bool, PersistenceError>> },
    ScopedEventStatus { scope: scoped_event::ScopedEventSnapshot, respond_to: Sender<Result<crate::domain::remote_crud::RemoteCrudOperation, PersistenceError>> },
    ResolveScopedEvent { scope: scoped_event::ScopedEventSnapshot, choice: ConflictResolutionChoice, connected:Option<scoped_event::ConnectedOwnerCheck>, respond_to: Sender<Result<ConflictResolutionResult, PersistenceError>> },

    ClaimRemoteEvent {
        account_id: String,
        respond_to: Sender<Result<Option<RemoteEventMutationClaimV3>, PersistenceError>>,
    },
    // Actor-only event-v3; no sender.
    #[allow(dead_code)]
    RemoteEventReadAllowed {
        claim: RemoteEventMutationClaimV3,
        respond_to: Sender<Result<bool, PersistenceError>>,
    },
    RemoteEventSnapshot {
        claim: RemoteEventMutationClaimV3,
        respond_to: Sender<Result<RemoteEventUpdateSnapshotV3, PersistenceError>>,
    },
    DeferRemoteEvent {
        claim: RemoteEventMutationClaimV3,
        uncertain: bool,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    RestoreRemoteEvent {
        claim: RemoteEventMutationClaimV3,
        remote: RemoteReadBack,
        respond_to: Sender<Result<RemoteEventMutationClaimV3, PersistenceError>>,
    },
    QueueRemoteEventTextUpdateV2 {
        input: QueueRemoteEventTextUpdateV2,
        respond_to: Sender<Result<RemoteMutationSummary, PersistenceError>>,
    },

    ClaimRemoteFields {
        account_id: String,
        respond_to: Sender<Result<Option<RemoteFieldsMutationClaim>, PersistenceError>>,
    },
    // Internal v2 path stays outside worker/product composition until B3c.
    #[allow(dead_code)]
    RemoteFieldsReadAllowed {
        claim: RemoteFieldsMutationClaim,
        respond_to: Sender<Result<bool, PersistenceError>>,
    },
    RemoteFieldsSnapshot {
        claim: RemoteFieldsMutationClaim,
        respond_to: Sender<Result<RemoteFieldsSendSnapshot, PersistenceError>>,
    },
    DeferRemoteFields {
        claim: RemoteFieldsMutationClaim,
        uncertain: bool,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    RestoreRemoteFields {
        claim: RemoteFieldsMutationClaim,
        remote: RemoteReadBack,
        respond_to: Sender<Result<RemoteFieldsMutationClaim, PersistenceError>>,
    },
    FinishRemoteEvent {
        actor: std::sync::Weak<()>,
        request: std::sync::Weak<()>,
        scope_request: Option<crate::application::remote_mutation_runtime::MutationRequestGate>,
        claim: RemoteEventMutationClaimV3,
        remote: RemoteReadBack,
        applied: bool,
        respond_to: Sender<Result<CommittedEventScheduleChange, PersistenceError>>,
    },
    RestoreScopedEvent {
        claim:RemoteEventMutationClaimV3,remote:RemoteReadBack,request:crate::application::remote_mutation_runtime::MutationRequestGate,
        respond_to:Sender<Result<RemoteEventMutationClaimV3,PersistenceError>>,
    },
    FinishRemoteFields {
        claim: RemoteFieldsMutationClaim,
        remote: RemoteReadBack,
        applied: bool,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    ResolveRemoteFields {
        operation_id: String,
        choice: ConflictResolutionChoice,
        respond_to: Sender<Result<ConflictResolutionResult, PersistenceError>>,
    },
    ResolveRemoteEvent {
        operation_id: String,
        choice: ConflictResolutionChoice,
        respond_to: Sender<Result<ConflictResolutionResult, PersistenceError>>,
    },
    QueueRemoteEventDelete {
        input: QueueRemoteEventDelete,
        respond_to: Sender<Result<RemoteMutationSummary, PersistenceError>>,
    },
    QueueRemoteEventCreateV2 {
        input: QueueRemoteEventCreateV2,
        respond_to: Sender<Result<RemoteCreationSummary, PersistenceError>>,
    },
    QueueRemoteEventCreate {
        input: QueueRemoteEventCreate,
        respond_to: Sender<Result<RemoteCreationSummary, PersistenceError>>,
    },
    RemoteCreationAccountView {
        account_id: String,
        respond_to: Sender<
            Result<crate::domain::remote_creation::RemoteCreationAccountView, PersistenceError>,
        >,
    },
    CreationReservationSnapshot {
        account_id: String,
        operation_id: String,
        respond_to: Sender<
            Result<
                crate::application::production_creation_owner::CreationReservationSnapshot,
                PersistenceError,
            >,
        >,
    },
    SelectRemoteCreation {
        account_id: String,
        version: &'static str,
        respond_to: Sender<Result<Option<String>, PersistenceError>>,
    },
    ClaimRemoteCreation {
        account_id: String,
        legacy_only: bool,
        respond_to: Sender<Result<Option<RemoteCreationClaim>, PersistenceError>>,
    },
    #[allow(dead_code)]
    ClaimRemoteCreationV2 {
        account_id: String,
        respond_to: Sender<Result<Option<RemoteCreationClaim>, PersistenceError>>,
    },
    RemoteCreationReadAllowed {
        claim: RemoteCreationClaim,
        respond_to: Sender<Result<bool, PersistenceError>>,
    },
    RecordRemoteCreationFirstSend {
        claim: RemoteCreationClaim,
        respond_to:
            Sender<Result<(RemoteCreationClaim, RemoteCreationAttemptEvidence), PersistenceError>>,
    },
    RemoteCreationSendAllowed {
        claim: RemoteCreationClaim,
        attempt_id: Uuid,
        attempted_at_ms: i64,
        respond_to: Sender<Result<bool, PersistenceError>>,
    },
    ObserveRemoteCreation {
        claim: RemoteCreationClaim,
        evidence: crate::domain::remote_creation::RemoteCreationReadBack,
        respond_to: Sender<
            Result<crate::domain::remote_creation::RemoteCreationObservation, PersistenceError>,
        >,
    },
    CompleteRemoteCreation {
        actor: std::sync::Weak<()>,
        request: std::sync::Weak<()>,
        claim: RemoteCreationClaim,
        remote: RemoteReadBack,
        respond_to: Sender<Result<CommittedEventScheduleChange, PersistenceError>>,
    },
    DeferRemoteCreation {
        claim: RemoteCreationClaim,
        uncertain_delivery: bool,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    ClaimRemoteDeletion {
        account_id: String,
        now_ms: i64,
        respond_to: Sender<Result<Option<RemoteDeletionClaim>, PersistenceError>>,
    },
    RemoteDeletionSendAllowed {
        claim_token: String,
        now_ms: i64,
        respond_to: Sender<Result<bool, PersistenceError>>,
    },
    RemoteDeletionReadAllowed {
        claim_token: String,
        now_ms: i64,
        respond_to: Sender<Result<bool, PersistenceError>>,
    },
    DeferRemoteDeletion {
        claim_token: String,
        ambiguous: bool,
        outcome: &'static str,
        now_ms: i64,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    FinishRemoteDeletion {
        claim_token: String,
        evidence: RemoteDeletionReadBack,
        now_ms: i64,
        actor: std::sync::Weak<()>,
        request: Option<crate::application::remote_mutation_runtime::MutationRequestGate>,
        respond_to: Sender<Result<CommittedEventScheduleChange, PersistenceError>>,
    },
    ListRemoteMutations {
        respond_to: Sender<Result<Vec<RemoteMutationSummary>, PersistenceError>>,
    },
    ListRemoteConflicts {
        respond_to: Sender<Result<Vec<RemoteConflictSummary>, PersistenceError>>,
    },
    ClaimRemoteMutation {
        account_id: Option<String>,
        now_ms: i64,
        respond_to: Sender<Result<Option<RemoteMutationClaim>, PersistenceError>>,
    },
    RemoteMutationSendAllowed {
        claim_token: String,
        now_ms: i64,
        respond_to: Sender<Result<bool, PersistenceError>>,
    },
    DeferRemoteMutation {
        claim_token: String,
        ambiguous: bool,
        outcome: &'static str,
        now_ms: i64,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    ConflictRemoteMutation {
        claim_token: String,
        remote: RemoteReadBack,
        now_ms: i64,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    ApplyRemoteMutation {
        claim_token: String,
        remote: RemoteReadBack,
        now_ms: i64,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    ResolveRemoteConflict {
        operation_id: String,
        choice: ConflictResolutionChoice,
        now_ms: i64,
        respond_to: Sender<Result<ConflictResolutionResult, PersistenceError>>,
    },
    ListCalendarAgendaItems {
        respond_to: Sender<Result<Vec<CalendarAgendaItem>, PersistenceError>>,
    },
    ListCalendarAgendaItemsInDisplayWindow {
        now_utc_ms: i64,
        respond_to: Sender<Result<Vec<CalendarAgendaItem>, PersistenceError>>,
    },
    ListCalendarSettings {
        respond_to: Sender<Result<Vec<CalendarSettingsView>, PersistenceError>>,
    },
    SetCalendarColorOverride {
        calendar_id: String,
        color: Option<String>,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    SetCalendarVisible {
        account_id: String,
        calendar_id: String,
        visible: bool,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    SetCalendarDisplayRange {
        account_id: String,
        calendar_id: String,
        display_past_days: Option<i64>,
        display_future_days: Option<i64>,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    ICloudBindingLease {
        account_id: String,
        respond_to: Sender<Result<Option<(crate::adapters::icloud_caldav::ICloudBinding, Arc<std::sync::atomic::AtomicBool>)>, PersistenceError>>,
    },
    ICloudBinding {
        account_id: String,
        respond_to: Sender<Result<Option<crate::adapters::icloud_caldav::ICloudBinding>, PersistenceError>>,
    },
    ReplaceDiscoveredCalendars {
        account_id: String,
        calendars: Vec<DiscoveredCalendarRecord>,
        binding: Option<crate::adapters::icloud_caldav::ICloudBinding>,
        observed_at_ms: i64,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    AvailableCalendarIdsByUrl {
        account_id: String,
        respond_to: Sender<Result<BTreeMap<String, String>, PersistenceError>>,
    },
    RefreshCalendarProjections {
        now_ms: i64,
        horizon: RecurrenceHorizon,
        respond_to: Sender<Result<CalendarProjectionRefreshSummary, PersistenceError>>,
    },
    SetCalendarCompleted {
        series_id: String,
        recurrence_key: String,
        completed: bool,
        respond_to: Sender<Result<CalendarAgendaItem, PersistenceError>>,
    },
    SetCalendarImportant {
        series_id: String,
        recurrence_key: String,
        important: bool,
        respond_to: Sender<Result<CalendarAgendaItem, PersistenceError>>,
    },
    UpdateCalendarLocalMetadata {
        input: UpdateCalendarLocalMetadata,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    CreateCalendarSubtask {
        input: CreateCalendarSubtask,
        respond_to: Sender<Result<CalendarAgendaItem, PersistenceError>>,
    },
    UpdateCalendarSubtask {
        input: UpdateCalendarSubtask,
        respond_to: Sender<Result<CalendarAgendaItem, PersistenceError>>,
    },
    SetCalendarSubtaskCompleted {
        series_id: String,
        recurrence_key: String,
        subtask_id: String,
        completed: bool,
        respond_to: Sender<Result<CalendarAgendaItem, PersistenceError>>,
    },
    DeleteCalendarSubtask {
        series_id: String,
        recurrence_key: String,
        subtask_id: String,
        respond_to: Sender<Result<CalendarAgendaItem, PersistenceError>>,
    },
    ReconcileDeletionEvidence {
        capture: SyncCapture,
        reconciliation: AccountDeletionReconciliation,
        respond_to: Sender<Result<(), PersistenceError>>,
    },
    PurgeDeletedRawCache {
        now_ms: i64,
        respond_to: Sender<Result<u32, PersistenceError>>,
    },
    CalendarSyncToken {
        calendar_id: String,
        respond_to: Sender<Result<Option<String>, PersistenceError>>,
    },
    RecordSyncRun {
        account_id: String,
        trigger: SyncTrigger,
        outcome: SyncOutcome,
        failure_class: SyncFailureClass,
        calendar_count: u32,
        resource_count: u32,
        respond_to: Sender<Result<SyncRunSummary, PersistenceError>>,
    },
    ListSyncRuns {
        respond_to: Sender<Result<Vec<SyncRunSummary>, PersistenceError>>,
    },
}

fn run_actor(
    owner_guard: db_owner::DatabaseOwner,
    receiver: Receiver<Request>,
    ready: mpsc::SyncSender<Result<(), PersistenceError>>,
    clock: Arc<dyn Clock>,
) {
    // A clock that cannot establish an instant must not block startup: the
    // projections it would rebuild are disposable caches, and the refresh
    // boundary reports the bounded clock failure instead.
    let startup_now_ms = clock_now_ms(clock.as_ref()).ok();
    let mut connection = match open_connection(owner_guard.path(), startup_now_ms) {
        Ok(connection) => {
            let _ = ready.send(Ok(()));
            connection
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let horizon_now_ms = || clock_now_ms(clock.as_ref());
    // In-flight provider stages cannot outlive this process or actor. Fresh
    // random epochs also reject a capture carried from a replaced actor.
    let mut sync_epochs: HashMap<String, Uuid> = HashMap::new();
    let mut binding_leases: HashMap<String, Vec<std::sync::Weak<std::sync::atomic::AtomicBool>>> =
        HashMap::new();
    for request in receiver {
        match request {
            Request::ProbeSourceActive { scope, respond_to } => {
                let _ = respond_to.send(probe_ledger::active(&connection, &scope));
            }
            Request::RecordProbeSourceClosure { scope, respond_to } => {
                let _ = respond_to.send(probe_ledger::record_closure(&connection, scope));
            }
            Request::ReconcileProbeSource {
                evidence,
                respond_to,
            } => {
                let _ = respond_to.send(probe_ledger::reconcile(&connection, evidence));
            }
            Request::BeginProbeSource {
                candidate,
                actor,
                respond_to,
            } => {
                let _ = respond_to.send(probe_ledger::begin(
                    &mut connection,
                    &sync_epochs,
                    candidate,
                    actor,
                ));
            }
            Request::FinishProbeSource {
                scope,
                retired,
                respond_to,
            } => {
                let _ = respond_to.send(probe_ledger::finish(&connection, &scope, retired));
            }
            Request::ProbeTargetSnapshot {
                selection,
                actor,
                respond_to,
            } => {
                let result = probe_target::capture(&connection, &mut sync_epochs, selection, actor);
                let _ = respond_to.send(result);
            }

            Request::ExactProbeCollections { actor, respond_to } => {
                let _ = respond_to.send(probe_target::capture_exact_pair(
                    &connection,
                    &mut sync_epochs,
                    actor,
                ));
            }
            Request::RevalidateExactProbeCollections {
                snapshot,
                respond_to,
            } => {
                let _ = respond_to.send(probe_target::revalidate_exact_pair(
                    &connection,
                    &sync_epochs,
                    snapshot,
                ));
            }
            Request::ImportProbeSeedWitness {
                candidate,
                revision,
                owner,
                respond_to,
            } => {
                let _ = respond_to.send(probe_seed::import(
                    &connection,
                    &sync_epochs,
                    candidate,
                    revision,
                    owner,
                ));
            }
            Request::PreviewProbeFirstRoute { parts, respond_to } => {
                let _ = respond_to.send(probe_first_route::preview(&connection, parts));
            }
            Request::RevalidateProbeTarget {
                snapshot,
                respond_to,
            } => {
                let result = probe_target::revalidate(&connection, &sync_epochs, snapshot);
                let _ = respond_to.send(result);
            }
            Request::RemoteCrudStatus {
                account,
                kind,
                respond_to,
            } => {
                let _ = respond_to.send(remote_crud::status(&mut connection, &account, kind));
            }
            Request::ResolveRemoteCrudConflict {
                kind,
                operation,
                choice,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    remote_crud::resolve(&mut connection, kind, &operation, choice, now)
                });
                let _ = respond_to.send(result);
            }
            Request::NextNotificationDeadline { respond_to } => {
                let result = horizon_now_ms().and_then(|now| {
                    if !reminder_notifications_enabled(&connection)? {
                        // No delivery authority means no scheduled wake either.
                        return Ok(None);
                    }
                    let next: Option<i64> = connection.query_row(
                        "SELECT MIN(j.fire_at_utc_ms) FROM notification_jobs j WHERE j.state='planned' AND ((j.source_kind='local_todo' AND EXISTS(SELECT 1 FROM local_todos t JOIN local_calendars c ON c.calendar_id=t.local_calendar_id LEFT JOIN work_item_state w ON w.local_todo_id=t.todo_id AND w.local_occurrence_key=j.occurrence_key WHERE t.todo_id=j.source_id AND t.active=1 AND c.hidden=0 AND COALESCE(w.completed,0)=0 AND (EXISTS(SELECT 1 FROM local_todo_occurrences o WHERE o.local_todo_id=t.todo_id AND o.occurrence_key=j.occurrence_key AND o.occurrence_state='active') OR (j.occurrence_key='SINGLETON' AND NOT EXISTS(SELECT 1 FROM local_todo_recurrence r WHERE r.local_todo_id=t.todo_id))))) OR (j.source_kind='calendar_occurrence' AND EXISTS(SELECT 1 FROM calendar_event_projections p JOIN remote_resources r ON r.resource_id=p.resource_id JOIN calendars c ON c.calendar_id=p.calendar_id JOIN event_series s ON s.series_id=p.series_id JOIN accounts a ON a.account_id=s.account_id LEFT JOIN work_item_state w ON w.calendar_series_id=p.series_id AND w.recurrence_key=p.recurrence_key WHERE p.series_id=j.source_id AND p.recurrence_key=j.occurrence_key AND p.projection_status='ready' AND p.cancelled=0 AND p.start_at_utc_ms IS NOT NULL AND p.end_at_utc_ms IS NOT NULL AND r.lifecycle='active' AND c.selected=1 AND c.available=1 AND a.enabled=1 AND a.removed_at_ms IS NULL AND COALESCE(w.completed,0)=0)))",
                        [],
                        |r| r.get(0),
                    )?;
                    Ok(next.map(|at| at.max(now)))
                });
                let _ = respond_to.send(result);
            }
            Request::NextAccountNotificationDeadline {
                account,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    if !reminder_notifications_enabled(&connection)? {
                        return Ok(None);
                    }
                    let next: Option<i64> = connection.query_row(
                        "SELECT MIN(j.fire_at_utc_ms) FROM notification_jobs j JOIN event_series s ON s.series_id=j.source_id JOIN accounts a ON a.account_id=s.account_id JOIN calendar_event_projections p ON p.series_id=j.source_id AND p.recurrence_key=j.occurrence_key JOIN remote_resources r ON r.resource_id=p.resource_id JOIN calendars c ON c.calendar_id=p.calendar_id LEFT JOIN work_item_state w ON w.calendar_series_id=p.series_id AND w.recurrence_key=p.recurrence_key WHERE j.source_kind='calendar_occurrence' AND j.state='planned' AND a.account_id=?1 AND a.enabled=1 AND a.removed_at_ms IS NULL AND a.remote_editing_enabled=1 AND p.projection_status='ready' AND p.cancelled=0 AND p.start_at_utc_ms IS NOT NULL AND p.end_at_utc_ms IS NOT NULL AND r.lifecycle='active' AND c.selected=1 AND c.available=1 AND COALESCE(w.completed,0)=0",
                        [account], |row| row.get(0))?;
                    Ok(next.map(|deadline| deadline.max(now)))
                });
                let _ = respond_to.send(result);
            }
            Request::ReconcileNotificationJobs { now_ms, respond_to } => {
                let _ = respond_to.send(reconcile_notification_jobs(&mut connection, now_ms));
            }
            Request::ClaimDueNotificationJobs { now_ms, respond_to } => {
                let _ = respond_to.send(claim_due_notification_jobs(&mut connection, now_ms));
            }
            Request::FinishNotificationJob {
                job_id,
                delivered,
                now_ms,
                respond_to,
            } => {
                let _ = respond_to.send(finish_notification_job(
                    &mut connection,
                    &job_id,
                    delivered,
                    now_ms,
                ));
            }
            Request::Create {
                title,
                priority,
                important,
                respond_to,
            } => {
                let _ = respond_to.send(create(&mut connection, title, priority, important));
            }
            Request::List { respond_to } => {
                let _ = respond_to.send(list(&connection));
            }
            Request::ListLocalCalendars { respond_to } => {
                let _ = respond_to.send(list_local_calendars(&connection));
            }
            Request::SaveLocalCalendar { input, respond_to } => {
                let _ = respond_to.send(save_local_calendar(&mut connection, input));
            }
            Request::SetLocalCalendarHidden {
                calendar_id,
                hidden,
                respond_to,
            } => {
                let _ = respond_to.send(set_local_calendar_hidden(
                    &mut connection,
                    &calendar_id,
                    hidden,
                ));
            }
            Request::DeleteLocalCalendar {
                calendar_id,
                respond_to,
            } => {
                let _ = respond_to.send(delete_local_calendar(&mut connection, &calendar_id));
            }
            Request::MoveLocalTodo {
                todo_id,
                calendar_id,
                respond_to,
            } => {
                let _ = respond_to.send(move_local_todo(&connection, &todo_id, &calendar_id));
            }
            Request::DesktopOrder { respond_to } => {
                let result = (|| {
                    let mut statement = connection
                        .prepare("SELECT item_key FROM desktop_item_order ORDER BY sort_order")?;
                    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                    rows.collect::<Result<Vec<_>, _>>()
                        .map_err(PersistenceError::from)
                })();
                let _ = respond_to.send(result);
            }
            Request::ResetDesktopOrder { respond_to } => {
                // Resolve the whole selected scope, independently of display
                // ranges, completion filters and the currently mounted rows.
                // A pending remote create owns its key under the temporary
                // `resource_id` series identity, so both identities are cleared.
                let result = connection.execute(
                    "DELETE FROM desktop_item_order WHERE EXISTS(
                       SELECT 1 FROM local_todos t JOIN local_calendars c ON c.calendar_id=t.local_calendar_id
                       WHERE c.hidden=0 AND item_key LIKE 'todo:' || t.todo_id || ':%'
                     ) OR EXISTS(
                       SELECT 1 FROM event_series s JOIN calendars c ON c.calendar_id=s.current_calendar_id
                       JOIN accounts a ON a.account_id=s.account_id
                       WHERE c.selected=1 AND c.available=1 AND a.enabled=1 AND a.removed_at_ms IS NULL
                         AND item_key LIKE 'calendar:' || s.series_id || ':%'
                     ) OR EXISTS(
                       SELECT 1 FROM remote_create_operations o JOIN calendars c ON c.calendar_id=o.calendar_id
                       JOIN accounts a ON a.account_id=o.account_id
                       WHERE o.state='pending' AND c.selected=1 AND c.available=1
                         AND a.enabled=1 AND a.removed_at_ms IS NULL
                         AND item_key LIKE 'calendar:' || o.resource_id || ':%'
                     )",
                    [],
                ).map(|_| ()).map_err(PersistenceError::from);
                let _ = respond_to.send(result);
            }
            Request::SetDesktopOrder { keys, respond_to } => {
                let result = (|| {
                    use std::collections::HashSet;
                    let mut seen = HashSet::new();
                    if keys.len() > 4096
                        || keys.iter().any(|key| {
                            key.len() > 512
                                || !(key.starts_with("todo:") || key.starts_with("calendar:"))
                                || !seen.insert(key)
                        })
                    {
                        return Err(PersistenceError::InvalidDesktopOrder);
                    }
                    let transaction = connection.transaction()?;
                    transaction.execute("DELETE FROM desktop_item_order", [])?;
                    for (position, key) in keys.iter().enumerate() {
                        transaction.execute(
                            "INSERT INTO desktop_item_order(item_key, sort_order) VALUES(?1, ?2)",
                            params![key, position as i64],
                        )?;
                    }
                    transaction.commit()?;
                    Ok(())
                })();
                let _ = respond_to.send(result);
            }
            Request::Update { input, respond_to } => {
                let result = horizon_now_ms().and_then(|now| update(&mut connection, input, now));
                let _ = respond_to.send(result);
            }
            Request::SaveLocalTodo { input, respond_to } => {
                let result =
                    horizon_now_ms().and_then(|now| save_local_todo(&mut connection, input, now));
                let _ = respond_to.send(result);
            }
            Request::SetCompleted {
                todo_id,
                occurrence_key,
                completed,
                respond_to,
            } => {
                let _ = respond_to.send(set_completed(
                    &mut connection,
                    &todo_id,
                    &occurrence_key,
                    completed,
                ));
            }
            Request::CreateSubtask { input, respond_to } => {
                let _ = respond_to.send(create_subtask(&mut connection, input));
            }
            Request::UpdateSubtask { input, respond_to } => {
                let _ = respond_to.send(update_subtask(&mut connection, input));
            }
            Request::MoveLocalSubtask { input, respond_to } => {
                let _ = respond_to.send(move_local_subtask(&mut connection, input));
            }
            Request::SetSubtaskCompleted {
                todo_id,
                subtask_id,
                completed,
                respond_to,
            } => {
                let _ = respond_to.send(set_subtask_completed(
                    &mut connection,
                    &todo_id,
                    &subtask_id,
                    completed,
                ));
            }
            Request::DeleteSubtask {
                todo_id,
                subtask_id,
                respond_to,
            } => {
                let _ = respond_to.send(delete_subtask(&mut connection, &todo_id, &subtask_id));
            }
            Request::SoftDelete {
                todo_id,
                respond_to,
            } => {
                let _ = respond_to.send(soft_delete(&mut connection, &todo_id));
            }
            Request::SoftDeleteOccurrence {
                todo_id,
                occurrence_key,
                respond_to,
            } => {
                let _ = respond_to.send(soft_delete_occurrence(
                    &mut connection,
                    &todo_id,
                    &occurrence_key,
                ));
            }
            Request::RestoreRecentlyDeletedOccurrence {
                todo_id,
                occurrence_key,
                respond_to,
            } => {
                let _ = respond_to.send(restore_recently_deleted_occurrence(
                    &mut connection,
                    &todo_id,
                    &occurrence_key,
                ));
            }
            Request::RestoreRecentlyDeleted {
                todo_id,
                respond_to,
            } => {
                let _ = respond_to.send(restore_recently_deleted(&mut connection, &todo_id));
            }
            Request::Backup {
                destination,
                respond_to,
            } => {
                let _ = respond_to.send(backup(&connection, &destination));
            }
            Request::SchemaVersion { respond_to } => {
                let result = connection
                    .pragma_query_value(None, "user_version", |row| row.get(0))
                    .map_err(PersistenceError::Sqlite);
                let _ = respond_to.send(result);
            }
            Request::GetAppSettings { respond_to } => {
                let _ = respond_to.send(get_app_settings(&connection));
            }
            Request::UpdateAppSettings { input, respond_to } => {
                let result = horizon_now_ms()
                    .and_then(|now| update_app_settings(&mut connection, input, now));
                let _ = respond_to.send(result);
            }
            Request::SetDesktopPinned { pinned, respond_to } => {
                let _ = respond_to.send(store_desktop_pinned(&connection, pinned));
            }
            Request::SetDesktopCollapsed {
                collapsed,
                respond_to,
            } => {
                let _ = respond_to.send(store_desktop_collapsed(&connection, collapsed));
            }
            Request::ListWindowPlacements { respond_to } => {
                let _ = respond_to.send(list_window_placements(&connection));
            }
            Request::SaveWindowPlacement {
                window_label,
                placement,
                respond_to,
            } => {
                let _ = respond_to.send(save_window_placement(
                    &mut connection,
                    &window_label,
                    placement,
                ));
            }
            Request::UpsertAccountProfile { input, respond_to } => {
                let account_id = input.account_id.clone();
                let result = upsert_account_profile(&mut connection, input);
                if result.is_ok() {
                    sync_epochs.insert(account_id, Uuid::new_v4());
                }
                let _ = respond_to.send(result);
            }
            Request::AccountProfile {
                account_id,
                respond_to,
            } => {
                let _ = respond_to.send(account_profile(&connection, &account_id));
            }
            Request::ListAccountProfiles { respond_to } => {
                let _ = respond_to.send(list_account_profiles(&connection));
            }
            Request::ArchiveAccountProfile {
                account_id,
                respond_to,
            } => {
                let result = archive_account_profile(&mut connection, &account_id);
                if result.is_ok() {
                    sync_epochs.insert(account_id, Uuid::new_v4());
                }
                let _ = respond_to.send(result);
            }
            Request::SetAccountEnabled {
                account_id,
                enabled,
                respond_to,
            } => {
                let result = set_account_enabled(&mut connection, &account_id, enabled);
                if result.is_ok() {
                    sync_epochs.insert(account_id, Uuid::new_v4());
                }
                let _ = respond_to.send(result);
            }
            Request::SetAccountCredentialConfigured {
                account_id,
                respond_to,
            } => {
                let _ = respond_to.send(set_account_credential_configured(
                    &mut connection,
                    &account_id,
                ));
            }
            Request::SetAccountRemoteEditingEnabled {
                account_id,
                enabled,
                respond_to,
            } => {
                let _ = respond_to.send(set_account_remote_editing_enabled(
                    &mut connection,
                    &account_id,
                    enabled,
                ));
            }
            Request::BeginSyncCapture {
                account_id,
                include_requests,
                respond_to,
            } => {
                let result = (|| {
                    validate_account_id(&account_id)?;
                    let requests = if include_requests {
                        account_sync_requests(&connection, &account_id)?
                    } else {
                        Vec::new()
                    };
                    let epoch = *sync_epochs
                        .entry(account_id.clone())
                        .or_insert_with(Uuid::new_v4);
                    Ok((SyncCapture { account_id, epoch }, requests))
                })();
                let _ = respond_to.send(result);
            }
            Request::PromoteAccountInventory {
                capture,
                stage,
                respond_to,
            } => {
                let result = if capture.account_id != stage.account_id
                    || sync_epochs.get(&capture.account_id) != Some(&capture.epoch)
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    (|| {
                        if !stage.complete || !stage.authoritative {
                            return Err(PersistenceError::UnsafeSyncStage);
                        }
                        let stage = stage.validate()?;
                        promote_account_inventory(&mut connection, stage, horizon_now_ms()?)
                    })()
                };
                if result.is_ok() {
                    sync_epochs.insert(capture.account_id, Uuid::new_v4());
                }
                let _ = respond_to.send(result);
            }
            Request::PromoteAccountIncremental {
                capture,
                stage,
                respond_to,
            } => {
                let result = if capture.account_id != stage.account_id
                    || sync_epochs.get(&capture.account_id) != Some(&capture.epoch)
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    (|| {
                        if !stage.complete || !stage.authoritative {
                            return Err(PersistenceError::UnsafeSyncStage);
                        }
                        let stage = stage.validate()?;
                        promote_account_incremental(&mut connection, stage, horizon_now_ms()?)
                    })()
                };
                if result.is_ok() {
                    sync_epochs.insert(capture.account_id, Uuid::new_v4());
                }
                let _ = respond_to.send(result);
            }
            Request::AccountSyncRequests {
                account_id,
                respond_to,
            } => {
                let _ = respond_to.send(account_sync_requests(&connection, &account_id));
            }
            Request::ListCachedCalendarResources { respond_to } => {
                let _ = respond_to.send(list_cached_calendar_resources(&connection));
            }
            Request::ConditionalTitleUpdateEvidence { target, respond_to } => {
                let _ = respond_to.send(conditional_title_update_evidence(&connection, target));
            }
            Request::QueueRemoteTitleUpdate { input, respond_to } => {
                let result = horizon_now_ms()
                    .and_then(|now_ms| queue_remote_title_update(&mut connection, input, now_ms));
                let _ = respond_to.send(result);
            }
            Request::QueueRemoteEventUpdateV3 { input, respond_to } => {
                let result = horizon_now_ms()
                    .and_then(|now| remote_update_event::queue(&mut connection, input, now));
                let _ = respond_to.send(result);
            }
            Request::RemoteRecurrenceTargets { respond_to } => {
                let _ = respond_to.send(remote_recurrence_targets(&connection));
            }
            Request::AdmitRemoteRecurrenceAdvance {
                resource_id,
                actor,
                connected,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    admit_remote_recurrence_advance(
                        &mut connection,
                        &resource_id,
                        actor,
                        now,
                        &connected,
                    )
                });
                let _ = respond_to.send(result);
            }
            Request::SelectConfirmedTarget {
                input,
                actor,
                respond_to,
            } => {
                let _ = respond_to.send(scoped_event::select_confirmed_target(
                    &connection,
                    &input,
                    actor,
                ));
            }
            Request::AdmitConfirmedTarget {
                input,
                target,
                actor,
                connected,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    scoped_event::admit_confirmed_target(
                        &mut connection,
                        input,
                        target,
                        actor,
                        now,
                        &connected,
                    )
                });
                let _ = respond_to.send(result);
            }
            Request::RehydrateScopedEvent {
                kind,
                operation_id,
                account_id,
                calendar_id,
                resource_id,
                actor,
                connected,
                respond_to,
            } => {
                let _ = respond_to.send(scoped_event::restore(
                    &connection,
                    (kind, &operation_id, &account_id, &calendar_id, &resource_id),
                    actor,
                    connected.as_ref(),
                ));
            }
            Request::AdmitScopedCrud {
                input,
                actor,
                connected,
                respond_to,
            } => {
                let _ = respond_to.send(horizon_now_ms().and_then(|now| {
                    scoped_event::admit_crud(&mut connection, input, actor, now, connected.as_ref())
                }));
            }
            Request::ClaimScopedCreate {
                scope,
                request,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    if !request.event_selection_current(&scope)
                        || scope.kind() != crate::domain::remote_crud::RemoteCrudKind::CreateUtcV2
                    {
                        return Err(PersistenceError::StaleSyncCapture);
                    }
                    let claim = remote_creation::claim_scoped(
                        &mut connection,
                        scope.ids().1,
                        now,
                        Some("utc-v2"),
                        Some(&scope),
                    )?;
                    if !request.event_selection_current(&scope) {
                        return Err(PersistenceError::StaleSyncCapture);
                    }
                    Ok(claim.map(|mut c| {
                        c.sync_epoch = Some(
                            *sync_epochs
                                .entry(c.account_id.clone())
                                .or_insert_with(Uuid::new_v4),
                        );
                        c
                    }))
                });
                let _ = respond_to.send(result);
            }
            Request::ClaimScopedDelete {
                scope,
                request,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    if !request.event_selection_current(&scope)
                        || scope.kind() != crate::domain::remote_crud::RemoteCrudKind::Delete
                    {
                        return Err(PersistenceError::StaleSyncCapture);
                    }
                    scoped_event::current_route(&connection, &scope)?;
                    let claim = claim_remote_deletion_scoped(
                        &mut connection,
                        scope.ids().1,
                        now,
                        Some(&scope),
                    )?;
                    if !request.event_selection_current(&scope) {
                        return Err(PersistenceError::StaleSyncCapture);
                    }
                    Ok(claim)
                });
                let _ = respond_to.send(result);
            }
            Request::AdmitScopedEvent {
                input,
                confirmation,
                actor,
                connected,
                respond_to,
            } => {
                let _ = respond_to.send(horizon_now_ms().and_then(|now| {
                    scoped_event::admit(
                        &mut connection,
                        input,
                        confirmation,
                        actor,
                        now,
                        connected.as_ref(),
                    )
                }));
            }
            Request::ClaimScopedEvent {
                scope,
                request,
                respond_to,
            } => {
                let result = horizon_now_ms()
                    .and_then(|now| {
                        if let Some(request) = &request {
                            scoped_event::claim_current(&mut connection, &scope, now, &|| {
                                request.event_selection_current(&scope)
                            })
                        } else {
                            scoped_event::claim(&mut connection, &scope, now)
                        }
                    })
                    .map(|claim| {
                        claim.map(|mut claim| {
                            claim.captured.sync_epoch = Some(
                                *sync_epochs
                                    .entry(claim.account_id().to_owned())
                                    .or_insert_with(Uuid::new_v4),
                            );
                            claim
                        })
                    });
                let _ = respond_to.send(result);
            }
            Request::ScopedEventSendSnapshot {
                scope,
                claim,
                respond_to,
            } => {
                let result = if claim.captured.sync_epoch.is_none()
                    || sync_epochs.get(claim.account_id()).copied() != claim.captured.sync_epoch
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms()
                        .and_then(|now| scoped_event::snapshot(&connection, &scope, &claim, now))
                };
                let _ = respond_to.send(result);
            }
            Request::ScopedEventReadAllowed {
                scope,
                claim,
                respond_to,
            } => {
                let result = if claim.captured.sync_epoch.is_none()
                    || sync_epochs.get(claim.account_id()).copied() != claim.captured.sync_epoch
                {
                    Ok(false)
                } else {
                    horizon_now_ms().and_then(|now| {
                        scoped_event::read_allowed(&connection, &scope, &claim, now)
                    })
                };
                let _ = respond_to.send(result);
            }
            Request::ScopedEventStatus { scope, respond_to } => {
                let _ = respond_to.send(scoped_event::status(&connection, &scope));
            }
            Request::ResolveScopedEvent {
                scope,
                choice,
                connected,
                respond_to,
            } => {
                let _ = respond_to.send(horizon_now_ms().and_then(|now| {
                    scoped_event::resolve(&mut connection, &scope, choice, now, connected.as_ref())
                }));
            }

            Request::ClaimRemoteEvent {
                account_id,
                respond_to,
            } => {
                let result = horizon_now_ms()
                    .and_then(|now| remote_update_event::claim(&mut connection, &account_id, now))
                    .map(|claim| {
                        claim.map(|mut claim| {
                            claim.captured.sync_epoch =
                                Some(*sync_epochs.entry(account_id).or_insert_with(Uuid::new_v4));
                            claim
                        })
                    });
                let _ = respond_to.send(result);
            }
            Request::RemoteEventReadAllowed { claim, respond_to } => {
                let result = if claim.captured.sync_epoch.is_none()
                    || sync_epochs.get(&claim.captured.account_id).copied()
                        != claim.captured.sync_epoch
                {
                    Ok(false)
                } else {
                    horizon_now_ms()
                        .and_then(|now| remote_update_event::read_allowed(&connection, &claim, now))
                };
                let _ = respond_to.send(result);
            }
            Request::RemoteEventSnapshot { claim, respond_to } => {
                let result = if claim.captured.sync_epoch.is_none()
                    || sync_epochs.get(&claim.captured.account_id).copied()
                        != claim.captured.sync_epoch
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms()
                        .and_then(|now| remote_update_event::snapshot(&connection, &claim, now))
                };
                let _ = respond_to.send(result);
            }
            Request::DeferRemoteEvent {
                claim,
                uncertain,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    remote_update_event::defer(&connection, &claim, uncertain, now)
                });
                let _ = respond_to.send(result);
            }
            Request::RestoreRemoteEvent {
                claim,
                remote,
                respond_to,
            } => {
                let result = if claim.captured.sync_epoch.is_none()
                    || sync_epochs.get(&claim.captured.account_id).copied()
                        != claim.captured.sync_epoch
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms().and_then(|now| {
                        remote_update_event::restore_base(&mut connection, &claim, &remote, now)
                    })
                };
                if result.is_err() {
                    let _ = horizon_now_ms()
                        .and_then(|now| remote_update_event::defer(&connection, &claim, true, now));
                }
                let _ = respond_to.send(result);
            }
            Request::QueueRemoteEventTextUpdateV2 { input, respond_to } => {
                let result = horizon_now_ms()
                    .and_then(|now| remote_update_fields::queue(&mut connection, input, now));
                let _ = respond_to.send(result);
            }

            Request::ClaimRemoteFields {
                account_id,
                respond_to,
            } => {
                let result = horizon_now_ms()
                    .and_then(|now| remote_update_fields::claim(&mut connection, &account_id, now))
                    .map(|claim| {
                        claim.map(|mut claim| {
                            claim.sync_epoch =
                                Some(*sync_epochs.entry(account_id).or_insert_with(Uuid::new_v4));
                            claim
                        })
                    });
                let _ = respond_to.send(result);
            }
            Request::RemoteFieldsReadAllowed { claim, respond_to } => {
                let result = if claim.sync_epoch.is_none()
                    || sync_epochs.get(&claim.account_id).copied() != claim.sync_epoch
                {
                    Ok(false)
                } else {
                    horizon_now_ms().and_then(|now| {
                        remote_update_fields::read_allowed(&connection, &claim, now)
                    })
                };
                let _ = respond_to.send(result);
            }
            Request::RemoteFieldsSnapshot { claim, respond_to } => {
                let result = if claim.sync_epoch.is_none()
                    || sync_epochs.get(&claim.account_id).copied() != claim.sync_epoch
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms()
                        .and_then(|now| remote_update_fields::snapshot(&connection, &claim, now))
                };
                let _ = respond_to.send(result);
            }
            Request::DeferRemoteFields {
                claim,
                uncertain,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    remote_update_fields::defer(&connection, &claim, uncertain, now)
                });
                let _ = respond_to.send(result);
            }
            Request::RestoreRemoteFields {
                claim,
                remote,
                respond_to,
            } => {
                let result = if claim.sync_epoch.is_none()
                    || sync_epochs.get(&claim.account_id).copied() != claim.sync_epoch
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms().and_then(|now| {
                        remote_update_fields::restore_base(&mut connection, &claim, &remote, now)
                    })
                };
                if result.is_err() {
                    let _ = horizon_now_ms().and_then(|now| {
                        remote_update_fields::defer(&connection, &claim, true, now)
                    });
                }
                let _ = respond_to.send(result);
            }
            Request::RestoreScopedEvent {
                claim,
                remote,
                request,
                respond_to,
            } => {
                let result = if claim.captured.sync_epoch.is_none()
                    || sync_epochs.get(claim.account_id()).copied() != claim.captured.sync_epoch
                    || !request.event_actor_current(&claim)
                    || !request.event_scope().is_some_and(|scope| {
                        scoped_event::current_route(&connection, scope).is_ok()
                    }) {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms()
                        .and_then(|now| {
                            remote_update_fields::restore_base_kind_guarded(
                                &mut connection,
                                &claim.captured,
                                &remote,
                                now,
                                remote_update_fields::UpdateKind::EventV3,
                                &|| request.event_actor_current(&claim),
                            )
                        })
                        .map(|captured| RemoteEventMutationClaimV3 { captured })
                };
                let _ = respond_to.send(result);
            }
            Request::FinishRemoteEvent {
                actor,
                request,
                scope_request,
                claim,
                remote,
                applied,
                respond_to,
            } => {
                let scoped_current = scope_request.as_ref().is_none_or(|request| {
                    request.event_actor_current(&claim)
                        && request.event_scope().is_some_and(|scope| {
                            scoped_event::current_route(&connection, scope).is_ok()
                        })
                });
                let result = if !scoped_current
                    || claim.captured.sync_epoch.is_none()
                    || sync_epochs.get(&claim.captured.account_id).copied()
                        != claim.captured.sync_epoch
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms().and_then(|now| {
                        (if scope_request.is_some() {
                            remote_update_fields::finish_event_guarded(
                                &mut connection,
                                &claim.captured,
                                &remote,
                                applied,
                                now,
                                &|| {
                                    scope_request
                                        .as_ref()
                                        .is_none_or(|request| request.event_actor_current(&claim))
                                },
                            )
                        } else {
                            remote_update_event::finish(
                                &mut connection,
                                &claim,
                                &remote,
                                applied,
                                now,
                            )
                        })
                        .map(|changed| CommittedEventScheduleChange {
                            actor,
                            request,
                            operation: claim.operation_id().to_owned(),
                            account: claim.captured.account_id.clone(),
                            changed,
                        })
                    })
                };
                if result.is_ok() {
                    sync_epochs.insert(claim.captured.account_id.clone(), Uuid::new_v4());
                } else {
                    let _ = horizon_now_ms()
                        .and_then(|now| remote_update_event::defer(&connection, &claim, true, now));
                }
                let _ = respond_to.send(result);
            }
            Request::FinishRemoteFields {
                claim,
                remote,
                applied,
                respond_to,
            } => {
                let result = if claim.sync_epoch.is_none()
                    || sync_epochs.get(&claim.account_id).copied() != claim.sync_epoch
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms().and_then(|now| {
                        remote_update_fields::finish(&mut connection, &claim, &remote, applied, now)
                    })
                };
                if result.is_ok() {
                    sync_epochs.insert(claim.account_id.clone(), Uuid::new_v4());
                } else {
                    let _ = horizon_now_ms().and_then(|now| {
                        remote_update_fields::defer(&connection, &claim, true, now)
                    });
                }
                let _ = respond_to.send(result);
            }
            Request::ResolveRemoteEvent {
                operation_id,
                choice,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    remote_update_event::resolve(&mut connection, &operation_id, choice, now)
                });
                let _ = respond_to.send(result);
            }
            Request::ResolveRemoteFields {
                operation_id,
                choice,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    remote_update_fields::resolve(&mut connection, &operation_id, choice, now)
                });
                let _ = respond_to.send(result);
            }
            Request::QueueRemoteEventDelete { input, respond_to } => {
                let result = horizon_now_ms()
                    .and_then(|now_ms| queue_remote_event_delete(&mut connection, input, now_ms));
                let _ = respond_to.send(result);
            }
            Request::QueueRemoteCrudDelete { input, respond_to } => {
                let result = horizon_now_ms().and_then(|now| {
                    queue_remote_event_delete_scoped(&mut connection, input, now, true)
                });
                let _ = respond_to.send(result);
            }
            Request::QueueRemoteEventCreateV2 { input, respond_to } => {
                let result = horizon_now_ms()
                    .and_then(|now| remote_creation::queue_v2(&mut connection, input, now));
                let _ = respond_to.send(result);
            }
            Request::QueueRemoteEventCreate { input, respond_to } => {
                let result = horizon_now_ms()
                    .and_then(|now| remote_creation::queue(&mut connection, input, now));
                let _ = respond_to.send(result);
            }
            Request::RemoteCreationAccountView {
                account_id,
                respond_to,
            } => {
                let _ =
                    respond_to.send(remote_creation::account_view(&mut connection, &account_id));
            }
            Request::CreationReservationSnapshot {
                account_id,
                operation_id,
                respond_to,
            } => {
                let result = remote_creation::reservation_target(&mut connection, &account_id, &operation_id)
                    .map(|(calendar_id,resource_id)| {
                        let epoch = *sync_epochs.entry(account_id.clone()).or_insert_with(Uuid::new_v4);
                        crate::application::production_creation_owner::CreationReservationSnapshot::actor_issued(account_id,operation_id,calendar_id,resource_id,epoch)
                    });
                let _ = respond_to.send(result);
            }
            Request::SelectRemoteCreation {
                account_id,
                version,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    remote_creation::select(&mut connection, &account_id, now, version)
                });
                let _ = respond_to.send(result);
            }
            Request::ClaimRemoteCreation {
                account_id,
                legacy_only,
                respond_to,
            } => {
                let result = horizon_now_ms()
                    .and_then(|now| {
                        remote_creation::claim(
                            &mut connection,
                            &account_id,
                            now,
                            legacy_only.then_some("utc-v1"),
                        )
                    })
                    .map(|claim| {
                        claim.map(|mut claim| {
                            claim.sync_epoch = Some(
                                *sync_epochs
                                    .entry(account_id.clone())
                                    .or_insert_with(Uuid::new_v4),
                            );
                            claim
                        })
                    });
                let _ = respond_to.send(result);
            }
            Request::ClaimRemoteCreationV2 {
                account_id,
                respond_to,
            } => {
                let result = horizon_now_ms()
                    .and_then(|now| {
                        remote_creation::claim(&mut connection, &account_id, now, Some("utc-v2"))
                    })
                    .map(|claim| {
                        claim.map(|mut claim| {
                            claim.sync_epoch = Some(
                                *sync_epochs
                                    .entry(account_id.clone())
                                    .or_insert_with(Uuid::new_v4),
                            );
                            claim
                        })
                    });
                let _ = respond_to.send(result);
            }
            Request::RemoteCreationReadAllowed { claim, respond_to } => {
                let result = if claim.sync_epoch.is_none()
                    || sync_epochs.get(&claim.account_id).copied() != claim.sync_epoch
                {
                    Ok(false)
                } else {
                    horizon_now_ms()
                        .and_then(|now| remote_creation::read_allowed(&connection, &claim, now))
                };
                let _ = respond_to.send(result);
            }
            Request::RecordRemoteCreationFirstSend {
                mut claim,
                respond_to,
            } => {
                let result = if claim.sync_epoch.is_none()
                    || sync_epochs.get(&claim.account_id).copied() != claim.sync_epoch
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms().and_then(|now| {
                        remote_creation::record_first_send(&mut connection, &claim, now)
                            .map(|id| (id, now))
                    })
                }
                .map(|(attempt_id, attempted_at_ms)| {
                    let epoch = Uuid::new_v4();
                    sync_epochs.insert(claim.account_id.clone(), epoch);
                    claim.sync_epoch = Some(epoch);
                    claim.phase = RemoteMutationExecutionPhase::VerifyFirst;
                    let evidence = RemoteCreationAttemptEvidence {
                        operation_id: claim.operation_id.clone(),
                        account_id: claim.account_id.clone(),
                        claim_token: claim.claim_token.clone(),
                        attempt_id,
                        attempted_at_ms,
                        sync_epoch: epoch,
                    };
                    debug_assert!(evidence.matches_claim(&claim));
                    (claim, evidence)
                });
                let _ = respond_to.send(result);
            }
            Request::RemoteCreationSendAllowed {
                claim,
                attempt_id,
                attempted_at_ms,
                respond_to,
            } => {
                let result = if claim.sync_epoch.is_none()
                    || sync_epochs.get(&claim.account_id).copied() != claim.sync_epoch
                {
                    Ok(false)
                } else {
                    horizon_now_ms().and_then(|now| {
                        remote_creation::send_allowed(
                            &mut connection,
                            &claim,
                            attempt_id,
                            attempted_at_ms,
                            now,
                        )
                    })
                };
                let _ = respond_to.send(result);
            }
            Request::ObserveRemoteCreation {
                claim,
                evidence,
                respond_to,
            } => {
                let result = if claim.sync_epoch.is_none()
                    || sync_epochs.get(&claim.account_id).copied() != claim.sync_epoch
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms().and_then(|now| {
                        remote_creation::observe(&mut connection, claim, evidence, now)
                    })
                };
                let _ = respond_to.send(result);
            }
            Request::CompleteRemoteCreation {
                actor,
                request,
                claim,
                remote,
                respond_to,
            } => {
                let result = if claim.sync_epoch.is_none()
                    || sync_epochs.get(&claim.account_id).copied() != claim.sync_epoch
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    horizon_now_ms().and_then(|now| {
                        remote_creation::complete(&mut connection, &claim, remote, now).map(
                            |changed| CommittedEventScheduleChange {
                                actor,
                                request,
                                operation: claim.operation_id.clone(),
                                account: claim.account_id.clone(),
                                changed,
                            },
                        )
                    })
                };
                if result.is_ok() {
                    sync_epochs.insert(claim.account_id.clone(), Uuid::new_v4());
                } else {
                    let _ = horizon_now_ms()
                        .and_then(|now| remote_creation::defer(&mut connection, claim, true, now));
                }
                let _ = respond_to.send(result);
            }
            Request::DeferRemoteCreation {
                claim,
                uncertain_delivery,
                respond_to,
            } => {
                let result = horizon_now_ms().and_then(|now| {
                    remote_creation::defer(&mut connection, claim, uncertain_delivery, now)
                });
                let _ = respond_to.send(result);
            }
            Request::ClaimRemoteDeletion {
                account_id,
                now_ms,
                respond_to,
            } => {
                let result = mutation_now_ms(clock.as_ref(), now_ms).and_then(|current| {
                    claim_remote_deletion(&mut connection, &account_id, current)
                });
                let _ = respond_to.send(result);
            }
            Request::RemoteDeletionSendAllowed {
                claim_token,
                now_ms,
                respond_to,
            } => {
                let result = mutation_now_ms(clock.as_ref(), now_ms).and_then(|current| {
                    remote_deletion_send_allowed(&connection, &claim_token, current)
                });
                let _ = respond_to.send(result);
            }
            Request::RemoteDeletionReadAllowed {
                claim_token,
                now_ms,
                respond_to,
            } => {
                let result = mutation_now_ms(clock.as_ref(), now_ms).and_then(|current| {
                    remote_deletion_read_allowed(&connection, &claim_token, current)
                });
                let _ = respond_to.send(result);
            }
            Request::DeferRemoteDeletion {
                claim_token,
                ambiguous,
                outcome,
                now_ms,
                respond_to,
            } => {
                let result = mutation_now_ms(clock.as_ref(), now_ms).and_then(|current| {
                    defer_remote_deletion(&connection, &claim_token, ambiguous, outcome, current)
                });
                let _ = respond_to.send(result);
            }
            Request::FinishRemoteDeletion {
                claim_token,
                evidence,
                now_ms,
                actor,
                request,
                respond_to,
            } => {
                let result = mutation_now_ms(clock.as_ref(), now_ms).and_then(|current| {
                    finish_remote_deletion(
                        &mut connection,
                        &claim_token,
                        evidence,
                        current,
                        &|operation, account, calendar, resource| {
                            request.as_ref().is_none_or(|request| {
                                if let Some(scope) = request.event_scope() {
                                    scope.kind()
                                        == crate::domain::remote_crud::RemoteCrudKind::Delete
                                        && scope.ids() == (operation, account, calendar, resource)
                                        && request.event_selection_current(scope)
                                } else {
                                    request.current()
                                }
                            })
                        },
                    )
                });
                let result = result.map(|(account_id, operation, changed)| {
                    sync_epochs.insert(account_id.clone(), Uuid::new_v4());
                    CommittedEventScheduleChange {
                        actor,
                        request: request
                            .as_ref()
                            .map_or_else(std::sync::Weak::new, |request| {
                                request.event_schedule_provenance()
                            }),
                        operation,
                        account: account_id,
                        changed,
                    }
                });
                let _ = respond_to.send(result);
            }
            Request::ListRemoteMutations { respond_to } => {
                let _ = respond_to.send(list_remote_mutations(&connection));
            }
            Request::ListRemoteConflicts { respond_to } => {
                let _ = respond_to.send(list_remote_conflicts(&connection));
            }
            Request::ClaimRemoteMutation {
                account_id,
                now_ms,
                respond_to,
            } => {
                let result = mutation_now_ms(clock.as_ref(), now_ms).and_then(|current| {
                    claim_remote_mutation(&mut connection, account_id.as_deref(), current)
                });
                let _ = respond_to.send(result);
            }
            Request::RemoteMutationSendAllowed {
                claim_token,
                now_ms,
                respond_to,
            } => {
                let result = mutation_now_ms(clock.as_ref(), now_ms).and_then(|current| {
                    remote_mutation_send_allowed(&connection, &claim_token, current)
                });
                let _ = respond_to.send(result);
            }
            Request::DeferRemoteMutation {
                claim_token,
                ambiguous,
                outcome,
                now_ms,
                respond_to,
            } => {
                let result = mutation_now_ms(clock.as_ref(), now_ms).and_then(|current| {
                    defer_remote_mutation(&connection, &claim_token, ambiguous, outcome, current)
                });
                let _ = respond_to.send(result);
            }
            Request::ConflictRemoteMutation {
                claim_token,
                remote,
                now_ms,
                respond_to,
            } => {
                let result = mutation_now_ms(clock.as_ref(), now_ms).and_then(|current| {
                    conflict_claimed_remote_mutation(&mut connection, &claim_token, remote, current)
                });
                let result = result.map(|account_id| {
                    sync_epochs.insert(account_id, Uuid::new_v4());
                });
                let _ = respond_to.send(result);
            }
            Request::ApplyRemoteMutation {
                claim_token,
                remote,
                now_ms,
                respond_to,
            } => {
                let result = mutation_now_ms(clock.as_ref(), now_ms).and_then(|current| {
                    apply_claimed_remote_mutation(&mut connection, &claim_token, remote, current)
                });
                let result = result.map(|account_id| {
                    sync_epochs.insert(account_id, Uuid::new_v4());
                });
                let _ = respond_to.send(result);
            }
            Request::ResolveRemoteConflict {
                operation_id,
                choice,
                now_ms,
                respond_to,
            } => {
                let _ = respond_to.send(resolve_remote_conflict(
                    &mut connection,
                    &operation_id,
                    choice,
                    now_ms,
                ));
            }
            Request::ListCalendarAgendaItems { respond_to } => {
                let _ = respond_to.send(list_calendar_agenda_items(&connection, None));
            }
            Request::ListCalendarAgendaItemsInDisplayWindow {
                now_utc_ms,
                respond_to,
            } => {
                let _ = respond_to.send(list_calendar_agenda_items(&connection, Some(now_utc_ms)));
            }
            Request::ListCalendarSettings { respond_to } => {
                let _ = respond_to.send(list_calendar_settings(&connection));
            }
            Request::SetCalendarColorOverride {
                calendar_id,
                color,
                respond_to,
            } => {
                let _ = respond_to.send(set_calendar_color_override(
                    &connection,
                    &calendar_id,
                    color.as_deref(),
                ));
            }
            Request::SetCalendarVisible {
                account_id,
                calendar_id,
                visible,
                respond_to,
            } => {
                let changed = connection.execute("UPDATE calendars SET selected=?3 WHERE account_id=?1 AND calendar_id=?2 AND available=1 AND EXISTS(SELECT 1 FROM accounts WHERE account_id=?1 AND removed_at_ms IS NULL)", params![account_id,calendar_id,visible]);
                let result = changed.map_err(PersistenceError::from).and_then(|count| {
                    if count == 1 {
                        Ok(())
                    } else {
                        Err(PersistenceError::NotFound)
                    }
                });
                let _ = respond_to.send(result);
            }
            Request::SetCalendarDisplayRange {
                account_id,
                calendar_id,
                display_past_days,
                display_future_days,
                respond_to,
            } => {
                let changed = connection.execute(
                    "UPDATE calendars SET display_past_days=COALESCE(?3,display_past_days),display_future_days=COALESCE(?4,display_future_days),display_past_unbounded=(?3 IS NULL),display_future_unbounded=(?4 IS NULL) WHERE account_id=?1 AND calendar_id=?2 AND available=1 AND EXISTS(SELECT 1 FROM accounts WHERE account_id=?1 AND removed_at_ms IS NULL)",
                    params![account_id, calendar_id, display_past_days, display_future_days],
                );
                let result = changed.map_err(PersistenceError::from).and_then(|count| {
                    if count == 1 {
                        Ok(())
                    } else {
                        Err(PersistenceError::NotFound)
                    }
                });
                let _ = respond_to.send(result);
            }
            Request::ICloudBindingLease {
                account_id,
                respond_to,
            } => {
                let result = icloud_binding::load(&connection, &account_id).map(|binding| {
                    binding.map(|binding| {
                        let valid = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let leases = binding_leases.entry(account_id).or_default();
                        leases.retain(|w| w.strong_count() > 0);
                        leases.push(Arc::downgrade(&valid));
                        (binding, valid)
                    })
                });
                let _ = respond_to.send(result);
            }
            Request::ICloudBinding {
                account_id,
                respond_to,
            } => {
                let _ = respond_to.send(icloud_binding::load(&connection, &account_id));
            }
            Request::ReplaceDiscoveredCalendars {
                account_id,
                mut calendars,
                binding,
                observed_at_ms,
                respond_to,
            } => {
                let result = (|| {
                    let transaction = connection.transaction()?;
                    // A disable or removal is newer user intent than an
                    // in-flight discovery, so it also wins at the write
                    // boundary and not only in the request registry.
                    let account = transaction
                        .query_row(
                            "SELECT enabled, removed_at_ms FROM accounts WHERE account_id=?1",
                            [&account_id],
                            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, Option<i64>>(1)?)),
                        )
                        .optional()?;
                    match account {
                        Some((true, None)) => {}
                        Some(_) => return Err(PersistenceError::AccountDisabled),
                        None => return Err(PersistenceError::NotFound),
                    }
                    if let Some(binding) = &binding {
                        icloud_binding::prepare(
                            &transaction,
                            &account_id,
                            binding,
                            &mut calendars,
                        )?;
                    }
                    let mut seen_ids = HashSet::new();
                    let mut seen_urls = HashSet::new();
                    for item in &calendars {
                        if !seen_ids.insert(&item.calendar_id)
                            || !seen_urls.insert(&item.collection_url)
                        {
                            return Err(PersistenceError::IdentityConflict);
                        }
                    }
                    {
                        let mut statement = transaction
                            .prepare("SELECT calendar_id FROM calendars WHERE account_id=?1")?;
                        let existing = statement
                            .query_map([&account_id], |row| row.get::<_, String>(0))?
                            .collect::<Result<Vec<_>, _>>()?;
                        for calendar_id in existing {
                            if !seen_ids.contains(&calendar_id) {
                                if calendar_has_inflight_mutation(&transaction, &calendar_id)? || transaction.query_row("SELECT EXISTS(SELECT 1 FROM remote_create_operations WHERE calendar_id=?1 AND state='pending')",[&calendar_id],|r|r.get::<_,bool>(0))? {
                                    return Err(PersistenceError::UnsafeSyncStage);
                                }
                                transaction.execute(
                                    "UPDATE remote_resources SET candidate_evidence_valid=0
                                     WHERE calendar_id=?1 AND lifecycle='missing_candidate'",
                                    [&calendar_id],
                                )?;
                            }
                        }
                    }
                    transaction.execute(
                        "UPDATE calendars SET available=0,updated_at_ms=?2 WHERE account_id=?1",
                        params![account_id, observed_at_ms],
                    )?;
                    for item in calendars {
                        let existing_calendar = transaction
                            .query_row(
                                "SELECT account_id,collection_url FROM calendars WHERE calendar_id=?1",
                                [&item.calendar_id],
                                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                            )
                            .optional()?;
                        if existing_calendar
                            .as_ref()
                            .is_some_and(|(existing, _)| existing != &account_id)
                        {
                            return Err(PersistenceError::IdentityConflict);
                        }
                        if existing_calendar
                            .as_ref()
                            .is_some_and(|(_, href)| href != &item.collection_url)
                            && calendar_has_pending_mutation(&transaction, &item.calendar_id)?
                        {
                            return Err(PersistenceError::UnsafeSyncStage);
                        }
                        let existing_url_identity = transaction
                            .query_row(
                                "SELECT calendar_id FROM calendars WHERE account_id=?1 AND collection_url=?2",
                                params![account_id, item.collection_url],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()?;
                        if existing_url_identity
                            .as_deref()
                            .is_some_and(|existing| existing != item.calendar_id.as_str())
                        {
                            return Err(PersistenceError::IdentityConflict);
                        }
                        transaction.execute(
                            "UPDATE remote_resources SET candidate_evidence_valid=0
                             WHERE lifecycle='missing_candidate' AND calendar_id=?1
                               AND EXISTS(SELECT 1 FROM calendars WHERE calendar_id=?1 AND collection_url<>?2)",
                            params![item.calendar_id, item.collection_url],
                        )?;
                        transaction.execute(
                            "UPDATE calendar_sync_state SET committed_sync_token=NULL,updated_at_ms=?3
                             WHERE calendar_id=?1 AND EXISTS(
                               SELECT 1 FROM calendars WHERE calendar_id=?1 AND collection_url<>?2)",
                            params![item.calendar_id, item.collection_url, observed_at_ms],
                        )?;
                        transaction.execute("INSERT INTO calendars(calendar_id,account_id,collection_url,display_name,server_color,privileges,available,created_at_ms,updated_at_ms,selected) VALUES(?1,?2,?3,?4,?5,?6,1,?7,?7,1) ON CONFLICT(calendar_id) DO UPDATE SET collection_url=excluded.collection_url,display_name=excluded.display_name,server_color=excluded.server_color,privileges=excluded.privileges,available=1,updated_at_ms=excluded.updated_at_ms", params![item.calendar_id,account_id,item.collection_url,item.display_name,item.server_color,item.privileges,observed_at_ms])?;
                    }
                    if let Some(binding) = &binding {
                        icloud_binding::canonicalize(&transaction, &account_id, binding)?;
                    }
                    if binding.is_some() {
                        for valid in binding_leases
                            .remove(&account_id)
                            .into_iter()
                            .flatten()
                            .filter_map(|w| w.upgrade())
                        {
                            valid.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                    transaction.commit()?;
                    Ok(())
                })();
                if result.is_ok() {
                    sync_epochs.insert(account_id, Uuid::new_v4());
                }
                let _ = respond_to.send(result);
            }
            Request::AvailableCalendarIdsByUrl {
                account_id,
                respond_to,
            } => {
                let result = (|| {
                    let mut statement = connection.prepare(
                        "SELECT collection_url,calendar_id FROM calendars WHERE account_id=?1 AND available=1 ORDER BY collection_url",
                    )?;
                    let rows = statement.query_map([account_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?;
                    rows.collect::<Result<BTreeMap<_, _>, _>>()
                        .map_err(Into::into)
                })();
                let _ = respond_to.send(result);
            }
            Request::RefreshCalendarProjections {
                now_ms,
                horizon,
                respond_to,
            } => {
                let _ = respond_to.send(refresh_calendar_event_projections(
                    &mut connection,
                    now_ms,
                    horizon,
                ));
            }
            Request::SetCalendarCompleted {
                series_id,
                recurrence_key,
                completed,
                respond_to,
            } => {
                let _ = respond_to.send(set_calendar_completed(
                    &mut connection,
                    &series_id,
                    &recurrence_key,
                    completed,
                ));
            }
            Request::SetCalendarImportant {
                series_id,
                recurrence_key,
                important,
                respond_to,
            } => {
                let _ = respond_to.send(set_calendar_important(
                    &mut connection,
                    &series_id,
                    &recurrence_key,
                    important,
                ));
            }
            Request::UpdateCalendarLocalMetadata { input, respond_to } => {
                let _ = respond_to.send(update_calendar_local_metadata(&mut connection, input));
            }
            Request::CreateCalendarSubtask { input, respond_to } => {
                let _ = respond_to.send(create_calendar_subtask(&mut connection, input));
            }
            Request::UpdateCalendarSubtask { input, respond_to } => {
                let _ = respond_to.send(update_calendar_subtask(&mut connection, input));
            }
            Request::SetCalendarSubtaskCompleted {
                series_id,
                recurrence_key,
                subtask_id,
                completed,
                respond_to,
            } => {
                let _ = respond_to.send(set_calendar_subtask_completed(
                    &mut connection,
                    &series_id,
                    &recurrence_key,
                    &subtask_id,
                    completed,
                ));
            }
            Request::DeleteCalendarSubtask {
                series_id,
                recurrence_key,
                subtask_id,
                respond_to,
            } => {
                let _ = respond_to.send(delete_calendar_subtask(
                    &mut connection,
                    &series_id,
                    &recurrence_key,
                    &subtask_id,
                ));
            }
            Request::ReconcileDeletionEvidence {
                capture,
                reconciliation,
                respond_to,
            } => {
                let result = if capture.account_id != reconciliation.account_id
                    || sync_epochs.get(&capture.account_id) != Some(&capture.epoch)
                {
                    Err(PersistenceError::StaleSyncCapture)
                } else {
                    reconcile_deletion_evidence(&mut connection, reconciliation)
                };
                if result.is_ok() {
                    sync_epochs.insert(capture.account_id, Uuid::new_v4());
                }
                let _ = respond_to.send(result);
            }
            Request::PurgeDeletedRawCache { now_ms, respond_to } => {
                let _ = respond_to.send(purge_expired_deleted_raw_cache(&mut connection, now_ms));
            }
            Request::CalendarSyncToken {
                calendar_id,
                respond_to,
            } => {
                let _ = respond_to.send(calendar_sync_token(&connection, &calendar_id));
            }
            Request::RecordSyncRun {
                account_id,
                trigger,
                outcome,
                failure_class,
                calendar_count,
                resource_count,
                respond_to,
            } => {
                let _ = respond_to.send(record_sync_run(
                    &mut connection,
                    account_id,
                    trigger,
                    outcome,
                    failure_class,
                    calendar_count,
                    resource_count,
                ));
            }
            Request::ListSyncRuns { respond_to } => {
                let _ = respond_to.send(list_sync_runs(&connection));
            }
        }
    }
    // The SQLite connection (including WAL cleanup) must be gone before its
    // non-clone owner permit releases directory/file exclusion.
    drop(connection);
    drop(owner_guard);
}

/// Latest schema version produced by [`migrations`]. Gates that require the
/// exact product schema compare against this instead of a literal.
pub(crate) const SCHEMA_VERSION: i64 = 60;
#[path = "persistence/icloud_binding.rs"]
mod icloud_binding;

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(MIGRATION_001),
        M::up(MIGRATION_002),
        M::up(MIGRATION_003),
        M::up(MIGRATION_004),
        M::up(MIGRATION_005),
        M::up(MIGRATION_006),
        M::up(MIGRATION_007),
        M::up(MIGRATION_008),
        M::up(MIGRATION_009),
        M::up(MIGRATION_010),
        M::up(MIGRATION_011),
        M::up(MIGRATION_012),
        M::up(MIGRATION_013),
        M::up(MIGRATION_014),
        M::up(MIGRATION_015),
        M::up(MIGRATION_016),
        M::up(MIGRATION_017),
        M::up(MIGRATION_018),
        M::up(MIGRATION_019),
        M::up(MIGRATION_020),
        M::up(MIGRATION_021),
        M::up(MIGRATION_022),
        M::up(MIGRATION_023),
        M::up(MIGRATION_024),
        M::up(MIGRATION_025),
        M::up(MIGRATION_026),
        M::up(MIGRATION_027),
        M::up(MIGRATION_028),
        M::up(MIGRATION_029),
        M::up(MIGRATION_030),
        M::up(MIGRATION_031),
        M::up(MIGRATION_032),
        M::up(MIGRATION_033),
        M::up(MIGRATION_034),
        M::up(MIGRATION_035),
        M::up(MIGRATION_036),
        M::up(remote_creation::MIGRATION),
        M::up(remote_creation::LEASE_MIGRATION),
        M::up(remote_creation::COMPLETION_MIGRATION),
        M::up(remote_creation::FIRST_SEND_MIGRATION),
        M::up(remote_update_fields::MIGRATION),
        M::up(remote_creation::VERSIONED_MIGRATION),
        M::up(remote_update_event::MIGRATION),
        M::up(probe_ledger::MIGRATION),
        M::up(probe_seed::MIGRATION),
        M::up(MIGRATION_046),
        M::up(MIGRATION_047),
        M::up(MIGRATION_048),
        M::up(MIGRATION_049),
        M::up(MIGRATION_050),
        M::up(MIGRATION_051),
        M::up(MIGRATION_052),
        M::up(MIGRATION_053),
        M::up(MIGRATION_054),
        M::up(MIGRATION_055),
        M::up(MIGRATION_056),
        M::up(MIGRATION_057),
        M::up(MIGRATION_058),
        M::up(remote_update_successor::MIGRATION),
        M::up(icloud_binding::MIGRATION),
    ])
}

fn open_connection(
    path: &Path,
    startup_now_ms: Option<i64>,
) -> Result<Connection, PersistenceError> {
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    migrations().to_latest(&mut connection)?;
    remote_update_event::recover_after_restart(&connection, startup_now_ms)?;
    probe_ledger::recover(&connection)?;
    if let Some(startup_now_ms) = startup_now_ms {
        refresh_calendar_event_projections(
            &mut connection,
            startup_now_ms,
            RecurrenceHorizon::standard(),
        )?;
    }
    Ok(connection)
}

/// Admits a wall-clock instant used to select a projection window. A bad or
/// absurd clock must not be able to select an arbitrary window.
fn admitted_now_ms(now_ms: i64) -> Result<i64, PersistenceError> {
    if !(1..=MAX_SUPPORTED_HORIZON_NOW_MS).contains(&now_ms) {
        return Err(PersistenceError::Clock);
    }
    Ok(now_ms)
}

fn clock_now_ms(clock: &dyn Clock) -> Result<i64, PersistenceError> {
    clock
        .now_utc_ms()
        .map_err(|_| PersistenceError::Clock)
        .and_then(admitted_now_ms)
}

fn mutation_now_ms(clock: &dyn Clock, requested_ms: i64) -> Result<i64, PersistenceError> {
    let requested_ms = admitted_now_ms(requested_ms)?;
    Ok(clock_now_ms(clock)?.max(requested_ms))
}

fn horizon_window(
    now_ms: i64,
    horizon: RecurrenceHorizon,
) -> Result<ExpansionWindow, PersistenceError> {
    let now_ms = admitted_now_ms(now_ms)?;
    horizon.window(now_ms).map_err(|_| PersistenceError::Clock)
}

fn standard_horizon_window(now_ms: i64) -> Result<ExpansionWindow, PersistenceError> {
    horizon_window(now_ms, RecurrenceHorizon::standard())
}

fn create(
    connection: &mut Connection,
    title: String,
    priority: Priority,
    important: bool,
) -> Result<LocalTodo, PersistenceError> {
    let now = unix_time_ms()?;
    let todo_id = Uuid::new_v4().to_string();
    let work_item_id = Uuid::new_v4().to_string();
    let transaction = connection.transaction()?;
    let calendar_id = local_todo_target_calendar(&transaction)?;
    transaction.execute(
        "INSERT INTO local_todos (todo_id, title, priority, important, created_at_ms, updated_at_ms, local_calendar_id) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        params![todo_id, title, priority.rank(), important, now, calendar_id],
    )?;
    transaction.execute(
        "INSERT INTO work_item_state (work_item_id, source_kind, local_todo_id, local_occurrence_key, last_seen_at_ms) VALUES (?1, 'local_todo_occurrence', ?2, ?3, ?4)",
        params![work_item_id, todo_id, SINGLETON_OCCURRENCE_KEY, now],
    )?;
    transaction.commit()?;
    get_active(connection, &todo_id)
}

/// Calendar a new Local TODO belongs to when the caller does not name one.
///
/// The default calendar wins while it is visible; otherwise the oldest visible
/// calendar is used, so a new task can never be created into a calendar the
/// user has hidden. A workspace with no visible calendar is rejected instead of
/// silently falling back to a hidden one.
fn local_todo_target_calendar(connection: &Connection) -> Result<String, PersistenceError> {
    connection
        .query_row(
            "SELECT calendar_id FROM local_calendars
             WHERE account_id=?1 AND hidden=0
             ORDER BY is_default DESC, created_at_ms, calendar_id
             LIMIT 1",
            [DEFAULT_LOCAL_ACCOUNT_ID],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(PersistenceError::Domain(
            crate::domain::local_todo::LocalTodoError::LastVisibleCalendar,
        ))
}

fn list(connection: &Connection) -> Result<Vec<LocalTodo>, PersistenceError> {
    let now = unix_time_ms()?;
    let mut statement = connection.prepare(
        "SELECT t.todo_id FROM local_todos t
         JOIN local_calendars c ON c.calendar_id=t.local_calendar_id
         WHERE t.active=1 AND c.hidden=0",
    )?;
    let mut todos = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut items = Vec::new();
    for id in todos.drain(..) {
        let template = get_active_at(connection, &id, now)?;
        let has_occurrences: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_todo_recurrence WHERE local_todo_id=?1)",
            [&id],
            |row| row.get(0),
        )?;
        if has_occurrences {
            let mut occurrences = connection.prepare("SELECT o.occurrence_key,o.nominal_start_at_utc_ms,o.nominal_due_at_utc_ms FROM local_todo_occurrences o LEFT JOIN work_item_state w ON w.local_todo_id=o.local_todo_id AND w.local_occurrence_key=o.occurrence_key WHERE o.local_todo_id=?1 AND o.occurrence_state<>'cancelled' AND (o.occurrence_state='active' OR COALESCE(w.completed,0)=1) ORDER BY o.nominal_due_at_utc_ms,o.sequence")?;
            let rows = occurrences
                .query_map([&id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(occurrences);
            for (key, start, due) in rows {
                let mut item = template.clone();
                item.occurrence_key = key.clone();
                item.start_at_utc_ms = start;
                item.due_at_utc_ms = Some(due);
                item.backlog_count = 0;
                let state = connection.query_row("SELECT work_item_id,completed,completed_at_ms FROM work_item_state WHERE local_todo_id=?1 AND local_occurrence_key=?2", params![id,key], |row| Ok((row.get::<_,String>(0)?,row.get::<_,bool>(1)?,row.get::<_,Option<i64>>(2)?))).optional()?;
                let (work_item_id, completed, completed_at_ms) = state.map_or(
                    (None, false, None),
                    |(work_item_id, completed, completed_at_ms)| {
                        (Some(work_item_id), completed, completed_at_ms)
                    },
                );
                item.work_item_id = work_item_id;
                item.completed = completed;
                item.completed_at_ms = completed_at_ms;
                item.subtasks = item
                    .work_item_id
                    .as_deref()
                    .map(|work_item_id| load_subtasks(connection, work_item_id))
                    .transpose()?
                    .unwrap_or_default();
                items.push(item);
            }
        } else {
            items.push(template);
        }
    }
    items.sort_by_key(|todo| {
        (
            !todo.important,
            std::cmp::Reverse(todo.priority.rank()),
            todo.due_at_utc_ms
                .or(todo.start_at_utc_ms)
                .unwrap_or(i64::MAX),
            todo.created_at_ms,
            todo.todo_id.clone(),
        )
    });
    Ok(items)
}

fn list_local_calendars(connection: &Connection) -> Result<Vec<LocalCalendar>, PersistenceError> {
    let mut statement = connection.prepare("SELECT calendar_id,name,color,is_default,hidden,display_past_days,display_future_days FROM local_calendars WHERE account_id=?1 ORDER BY is_default DESC,created_at_ms,calendar_id")?;
    statement
        .query_map([DEFAULT_LOCAL_ACCOUNT_ID], |row| {
            Ok(LocalCalendar {
                calendar_id: row.get(0)?,
                name: row.get(1)?,
                color: row.get(2)?,
                is_default: row.get(3)?,
                hidden: row.get(4)?,
                display_past_days: row.get(5)?,
                display_future_days: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn save_local_calendar(
    connection: &mut Connection,
    input: SaveLocalCalendar,
) -> Result<LocalCalendar, PersistenceError> {
    let now = unix_time_ms()?;
    let existing_id = input.calendar_id;
    let id = existing_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if existing_id.is_some() {
        let changed = connection.execute("UPDATE local_calendars SET name=?2,color=?3,display_past_days=?4,display_future_days=?5,updated_at_ms=?6 WHERE calendar_id=?1 AND account_id=?7", params![id,input.name,input.color,input.display_past_days,input.display_future_days,now,DEFAULT_LOCAL_ACCOUNT_ID])?;
        if changed != 1 {
            return Err(PersistenceError::NotFound);
        }
    } else {
        connection.execute("INSERT INTO local_calendars(calendar_id,account_id,name,color,is_default,display_past_days,display_future_days,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,0,?5,?6,?7,?7)", params![id,DEFAULT_LOCAL_ACCOUNT_ID,input.name,input.color,input.display_past_days,input.display_future_days,now])?;
    }
    connection
        .query_row(
            "SELECT calendar_id,name,color,is_default,hidden,display_past_days,display_future_days FROM local_calendars WHERE calendar_id=?1",
            [&id],
            |row| {
                Ok(LocalCalendar {
                    calendar_id: row.get(0)?,
                    name: row.get(1)?,
                    color: row.get(2)?,
                    is_default: row.get(3)?,
                    hidden: row.get(4)?,
                    display_past_days: row.get(5)?,
                    display_future_days: row.get(6)?,
                })
            },
        )
        .map_err(Into::into)
}

fn local_calendar(
    connection: &Connection,
    calendar_id: &str,
) -> Result<LocalCalendar, PersistenceError> {
    connection
        .query_row(
            "SELECT calendar_id,name,color,is_default,hidden,display_past_days,display_future_days FROM local_calendars
             WHERE calendar_id=?1 AND account_id=?2",
            params![calendar_id, DEFAULT_LOCAL_ACCOUNT_ID],
            |row| {
                Ok(LocalCalendar {
                    calendar_id: row.get(0)?,
                    name: row.get(1)?,
                    color: row.get(2)?,
                    is_default: row.get(3)?,
                    hidden: row.get(4)?,
                    display_past_days: row.get(5)?,
                    display_future_days: row.get(6)?,
                })
            },
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)
}

/// Hides or reveals one local calendar.
///
/// Hiding is presentation state: membership, completion, subtasks, and
/// recurrence rows are untouched, and revealing the calendar brings the same
/// tasks back. At least one calendar must stay visible so the product can
/// always show a local list and place a new task.
fn set_local_calendar_hidden(
    connection: &mut Connection,
    calendar_id: &str,
    hidden: bool,
) -> Result<LocalCalendar, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let calendar = local_calendar(&transaction, calendar_id)?;
    if hidden {
        let visible_others = transaction.query_row(
            "SELECT COUNT(*) FROM local_calendars
             WHERE account_id=?1 AND hidden=0 AND calendar_id<>?2",
            params![DEFAULT_LOCAL_ACCOUNT_ID, calendar_id],
            |row| row.get::<_, i64>(0),
        )?;
        if visible_others == 0 {
            return Err(PersistenceError::Domain(
                crate::domain::local_todo::LocalTodoError::LastVisibleCalendar,
            ));
        }
    }
    if calendar.hidden != hidden {
        let changed = transaction.execute(
            "UPDATE local_calendars SET hidden=?2,updated_at_ms=?3 WHERE calendar_id=?1",
            params![calendar_id, hidden, now],
        )?;
        if changed != 1 {
            return Err(PersistenceError::NotFound);
        }
    }
    let updated = local_calendar(&transaction, calendar_id)?;
    transaction.commit()?;
    Ok(updated)
}

/// Deletes one local calendar without ever deleting a task.
///
/// Every task owned by the calendar — including soft-deleted history kept for
/// undo and records — is reassigned to a remaining visible calendar before the
/// row is removed. When the deleted calendar was the default, the receiving
/// calendar becomes the new default so later task creation keeps working. The
/// last calendar, and any deletion that would leave no visible calendar, fail
/// closed instead of stranding user data.
fn delete_local_calendar(
    connection: &mut Connection,
    calendar_id: &str,
) -> Result<crate::domain::local_todo::DeletedLocalCalendar, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let calendar = local_calendar(&transaction, calendar_id)?;
    let target = transaction
        .query_row(
            "SELECT calendar_id FROM local_calendars
             WHERE account_id=?1 AND calendar_id<>?2 AND hidden=0
             ORDER BY is_default DESC, created_at_ms, calendar_id
             LIMIT 1",
            params![DEFAULT_LOCAL_ACCOUNT_ID, calendar_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(target) = target else {
        // Either this was the only calendar, or every other calendar is hidden.
        let remaining = transaction.query_row(
            "SELECT COUNT(*) FROM local_calendars WHERE account_id=?1 AND calendar_id<>?2",
            params![DEFAULT_LOCAL_ACCOUNT_ID, calendar_id],
            |row| row.get::<_, i64>(0),
        )?;
        return Err(PersistenceError::Domain(if remaining == 0 {
            crate::domain::local_todo::LocalTodoError::LastCalendar
        } else {
            crate::domain::local_todo::LocalTodoError::LastVisibleCalendar
        }));
    };
    if calendar.is_default {
        transaction.execute(
            "UPDATE local_calendars SET is_default=0 WHERE account_id=?1",
            [DEFAULT_LOCAL_ACCOUNT_ID],
        )?;
        transaction.execute(
            "UPDATE local_calendars SET is_default=1,updated_at_ms=?2 WHERE calendar_id=?1",
            params![target, now],
        )?;
    }
    let moved = transaction.execute(
        "UPDATE local_todos SET local_calendar_id=?2,updated_at_ms=CASE WHEN active=1 THEN ?3 ELSE updated_at_ms END WHERE local_calendar_id=?1",
        params![calendar_id, target, now],
    )?;
    transaction.execute(
        "DELETE FROM local_calendars WHERE calendar_id=?1",
        [calendar_id],
    )?;
    transaction.commit()?;
    Ok(crate::domain::local_todo::DeletedLocalCalendar {
        moved_todos: u32::try_from(moved).unwrap_or(u32::MAX),
    })
}

fn move_local_todo(
    connection: &Connection,
    todo_id: &str,
    calendar_id: &str,
) -> Result<LocalTodo, PersistenceError> {
    let now = unix_time_ms()?;
    let changed = connection.execute("UPDATE local_todos SET local_calendar_id=?2,updated_at_ms=?3 WHERE todo_id=?1 AND active=1 AND EXISTS(SELECT 1 FROM local_calendars WHERE calendar_id=?2)", params![todo_id,calendar_id,now])?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    get_active(connection, todo_id)
}

fn get_active(connection: &Connection, todo_id: &str) -> Result<LocalTodo, PersistenceError> {
    get_active_at(connection, todo_id, unix_time_ms()?)
}

fn get_occurrence(
    connection: &Connection,
    todo_id: &str,
    occurrence_key: &str,
) -> Result<LocalTodo, PersistenceError> {
    let mut item = get_active(connection, todo_id)?;
    if occurrence_key == SINGLETON_OCCURRENCE_KEY {
        return Ok(item);
    }
    let (start,due): (Option<i64>,i64) = connection.query_row("SELECT o.nominal_start_at_utc_ms,o.nominal_due_at_utc_ms FROM local_todo_occurrences o LEFT JOIN work_item_state w ON w.local_todo_id=o.local_todo_id AND w.local_occurrence_key=o.occurrence_key WHERE o.local_todo_id=?1 AND o.occurrence_key=?2 AND o.occurrence_state<>'cancelled' AND (o.occurrence_state='active' OR COALESCE(w.completed,0)=1)", params![todo_id,occurrence_key], |row| Ok((row.get(0)?,row.get(1)?))).optional()?.ok_or(PersistenceError::NotFound)?;
    item.occurrence_key = occurrence_key.to_owned();
    item.start_at_utc_ms = start;
    item.due_at_utc_ms = Some(due);
    item.backlog_count = 0;
    let state = connection.query_row("SELECT work_item_id,completed,completed_at_ms FROM work_item_state WHERE local_todo_id=?1 AND local_occurrence_key=?2", params![todo_id,occurrence_key], |row| Ok((row.get::<_,String>(0)?,row.get::<_,bool>(1)?,row.get::<_,Option<i64>>(2)?))).optional()?;
    let (work_item_id, completed, completed_at_ms) = state
        .map_or((None, false, None), |(id, completed, completed_at_ms)| {
            (Some(id), completed, completed_at_ms)
        });
    item.work_item_id = work_item_id;
    item.completed = completed;
    item.completed_at_ms = completed_at_ms;
    item.subtasks = item
        .work_item_id
        .as_deref()
        .map(|id| load_subtasks(connection, id))
        .transpose()?
        .unwrap_or_default();
    Ok(item)
}

fn get_active_at(
    connection: &Connection,
    todo_id: &str,
    now: i64,
) -> Result<LocalTodo, PersistenceError> {
    let (calendar_id, calendar_name, calendar_color, title, notes, location, master_start, master_due, rank, important, created_at_ms, updated_at_ms) = connection.query_row(
        "SELECT c.calendar_id,c.name,c.color,t.title,t.notes,t.location,t.start_at_utc_ms,t.due_at_utc_ms,t.priority,t.important,t.created_at_ms,t.updated_at_ms FROM local_todos t JOIN local_calendars c ON c.calendar_id=t.local_calendar_id WHERE t.todo_id=?1 AND t.active=1",
        [todo_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?,row.get::<_,i64>(8)?,row.get(9)?,row.get(10)?,row.get(11)?)),
    ).optional()?.ok_or(PersistenceError::NotFound)?;
    let priority = Priority::from_rank(rank)?;
    let schedule = load_recurrence(connection, todo_id)?;
    let (occurrence_key, start_at_utc_ms, due_at_utc_ms, backlog_count) =
        if let Some(schedule) = &schedule {
            select_recurrence_occurrence(connection, todo_id, schedule, now)?
        } else if let Some(retained) = connection.query_row("SELECT o.occurrence_key,o.nominal_start_at_utc_ms,o.nominal_due_at_utc_ms FROM local_todo_recurrence r JOIN local_todo_occurrences o ON o.local_todo_id=r.local_todo_id AND o.occurrence_key=r.retained_occurrence_key WHERE r.local_todo_id=?1 AND r.schedule_state='stopped'", [todo_id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,Option<i64>>(1)?,row.get::<_,i64>(2)?))).optional()? {
            (retained.0, retained.1, Some(retained.2), 0)
        } else {
            (
                SINGLETON_OCCURRENCE_KEY.to_owned(),
                master_start,
                master_due,
                0,
            )
        };
    let state = connection.query_row(
        "SELECT work_item_id,completed,completed_at_ms FROM work_item_state WHERE local_todo_id=?1 AND local_occurrence_key=?2",
        params![todo_id, occurrence_key], |row| Ok((row.get::<_,String>(0)?, row.get::<_,bool>(1)?, row.get::<_,Option<i64>>(2)?)),
    ).optional()?;
    let (work_item_id, completed, completed_at_ms) = state
        .map_or((None, false, None), |(id, completed, completed_at_ms)| {
            (Some(id), completed, completed_at_ms)
        });
    let mut todo = LocalTodo {
        todo_id: todo_id.to_owned(),
        calendar_id,
        calendar_name,
        calendar_color,
        work_item_id,
        occurrence_key,
        title,
        notes,
        location,
        start_at_utc_ms,
        due_at_utc_ms,
        priority,
        important,
        completed,
        completed_at_ms,
        recurrence: schedule,
        backlog_count,
        tags: vec![],
        subtasks: vec![],
        created_at_ms,
        updated_at_ms,
    };
    todo.tags = load_tags(connection, todo_id)?;
    if let Some(work_item_id) = &todo.work_item_id {
        todo.subtasks = load_subtasks(connection, work_item_id)?;
    }
    Ok(todo)
}

fn load_recurrence(
    connection: &Connection,
    todo_id: &str,
) -> Result<Option<FixedRecurrence>, PersistenceError> {
    connection.query_row("SELECT series_id,frequency,interval_value,timezone,weekdays_mask,month_days_mask,year_months_mask,end_kind,end_count,end_until_at_utc_ms FROM local_todo_recurrence WHERE local_todo_id=?1 AND schedule_state='active'", [todo_id], |row| {
        let frequency: String = row.get(1)?;
        Ok((row.get::<_,String>(0)?, frequency, row.get::<_,u32>(2)?, row.get::<_,String>(3)?, row.get::<_,i64>(4)?, row.get::<_,i64>(5)?, row.get::<_,i64>(6)?, row.get::<_,String>(7)?, row.get::<_,Option<u16>>(8)?, row.get::<_,Option<i64>>(9)?))
    }).optional()?.map(|(series_id, frequency, interval, timezone, weekdays_mask, month_days_mask, year_months_mask, end_kind, end_count, end_until_at_utc_ms)| Ok(FixedRecurrence { series_id, frequency: RecurrenceFrequency::from_storage(&frequency)?, interval, options: RecurrenceOptions { timezone, weekdays: values_from_mask(weekdays_mask, 0, 6), month_days: values_from_mask(month_days_mask, 1, 31), year_months: values_from_mask(year_months_mask, 1, 12), end_kind: RecurrenceEndKind::from_storage(&end_kind).map_err(|_| PersistenceError::RecurrenceRange)?, end_count, end_until_at_utc_ms } })).transpose()
}

fn values_mask(values: &[u8], offset: u8) -> i64 {
    values
        .iter()
        .fold(0_i64, |mask, value| mask | (1_i64 << (value - offset)))
}
fn values_from_mask(mask: i64, offset: u8, maximum: u8) -> Vec<u8> {
    (offset..=maximum)
        .filter(|value| mask & (1_i64 << (value - offset)) != 0)
        .collect()
}

fn select_recurrence_occurrence(
    connection: &Connection,
    todo_id: &str,
    schedule: &FixedRecurrence,
    now: i64,
) -> Result<(String, Option<i64>, Option<i64>, u64), PersistenceError> {
    let mut statement = connection.prepare("SELECT o.occurrence_key,o.nominal_start_at_utc_ms,o.nominal_due_at_utc_ms,COALESCE(w.completed,0) FROM local_todo_occurrences o LEFT JOIN work_item_state w ON w.local_todo_id=o.local_todo_id AND w.local_occurrence_key=o.occurrence_key WHERE o.local_todo_id=?1 AND o.series_id=?2 AND o.occurrence_state='active' ORDER BY o.nominal_due_at_utc_ms,o.sequence")?;
    let rows = statement
        .query_map(params![todo_id, schedule.series_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let selected = rows
        .iter()
        .find(|row| !row.3 && row.2 <= now)
        .or_else(|| rows.iter().find(|row| !row.3))
        .or_else(|| rows.last())
        .ok_or(PersistenceError::RecurrenceRange)?;
    let backlog = rows
        .iter()
        .filter(|row| !row.3 && row.2 <= now && row.0 != selected.0)
        .count() as u64;
    Ok((selected.0.clone(), selected.1, Some(selected.2), backlog))
}

fn load_tags(connection: &Connection, todo_id: &str) -> Result<Vec<Tag>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT g.tag_id, g.name FROM local_todo_tags r JOIN tags g ON g.tag_id = r.tag_id WHERE r.local_todo_id = ?1 ORDER BY r.sort_order, g.tag_id",
    )?;
    Ok(statement
        .query_map([todo_id], |row| {
            Ok(Tag {
                tag_id: row.get(0)?,
                name: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn load_calendar_tags(
    connection: &Connection,
    resource_id: &str,
) -> Result<Vec<Tag>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT g.tag_id,g.name FROM calendar_resource_tags r JOIN tags g ON g.tag_id=r.tag_id WHERE r.resource_id=?1 ORDER BY r.sort_order,g.tag_id",
    )?;
    Ok(statement
        .query_map([resource_id], |row| {
            Ok(Tag {
                tag_id: row.get(0)?,
                name: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn load_calendar_recurrence(
    connection: &Connection,
    resource_id: &str,
) -> Result<Option<CalendarFixedRecurrence>, PersistenceError> {
    connection
        .query_row(
            "SELECT frequency,interval_value,timezone,weekdays_mask,month_days_mask,year_months_mask,end_kind,end_count,end_until_at_utc_ms FROM calendar_resource_recurrence WHERE resource_id=?1",
            [resource_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?, row.get::<_, String>(6)?, row.get::<_, Option<u16>>(7)?, row.get::<_, Option<i64>>(8)?)),
        )
        .optional()?
        .map(|(frequency, interval, timezone, weekdays, month_days, year_months, end_kind, end_count, end_until_at_utc_ms)| {
            Ok(CalendarFixedRecurrence {
                frequency: RecurrenceFrequency::from_storage(&frequency)?,
                interval,
                options: RecurrenceOptions {
                    timezone,
                    weekdays: values_from_mask(weekdays, 0, 6),
                    month_days: values_from_mask(month_days, 1, 31),
                    year_months: values_from_mask(year_months, 1, 12),
                    end_kind: RecurrenceEndKind::from_storage(&end_kind).map_err(|_| PersistenceError::RecurrenceRange)?,
                    end_count,
                    end_until_at_utc_ms,
                },
            })
        })
        .transpose()
}

fn load_subtasks(
    connection: &Connection,
    work_item_id: &str,
) -> Result<Vec<Subtask>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT subtask_id, work_item_id, text, completed, sort_order, created_at_ms, updated_at_ms
         FROM subtasks WHERE work_item_id = ?1 ORDER BY sort_order, subtask_id",
    )?;
    Ok(statement
        .query_map([work_item_id], |row| {
            Ok(Subtask {
                subtask_id: row.get(0)?,
                work_item_id: row.get(1)?,
                text: row.get(2)?,
                completed: row.get(3)?,
                sort_order: row.get(4)?,
                created_at_ms: row.get(5)?,
                updated_at_ms: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn update(
    connection: &mut Connection,
    input: UpdateLocalTodo,
    now: i64,
) -> Result<LocalTodo, PersistenceError> {
    let transaction = connection.transaction()?;
    update_local_todo_details(&transaction, &input, now)?;
    replan_local_notification_jobs_after_edit(&transaction, &input.todo_id, now)?;
    let result = if let Some(key) = input.selected_occurrence_key.as_deref() {
        get_occurrence(&transaction, &input.todo_id, key)?
    } else {
        get_active_at(&transaction, &input.todo_id, now)?
    };
    transaction.commit()?;
    Ok(result)
}

fn save_local_todo(
    connection: &mut Connection,
    input: SaveLocalTodo,
    now: i64,
) -> Result<LocalTodo, PersistenceError> {
    let todo_id = input
        .todo_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let transaction = connection.transaction()?;
    let calendar_is_visible = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_calendars WHERE calendar_id=?1 AND account_id=?2 AND hidden=0)",
        params![input.calendar_id, DEFAULT_LOCAL_ACCOUNT_ID],
        |row| row.get::<_, bool>(0),
    )?;
    if !calendar_is_visible {
        return Err(PersistenceError::NotFound);
    }
    if input.todo_id.is_none() {
        transaction.execute(
            "INSERT INTO local_todos (todo_id, title, priority, important, created_at_ms, updated_at_ms, local_calendar_id) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            params![todo_id, input.title, input.priority.rank(), input.important, now, input.calendar_id],
        )?;
        transaction.execute(
            "INSERT INTO work_item_state (work_item_id, source_kind, local_todo_id, local_occurrence_key, last_seen_at_ms) VALUES (?1, 'local_todo_occurrence', ?2, ?3, ?4)",
            params![Uuid::new_v4().to_string(), todo_id, SINGLETON_OCCURRENCE_KEY, now],
        )?;
    } else {
        let changed = transaction.execute(
            "UPDATE local_todos SET local_calendar_id=?2 WHERE todo_id=?1 AND active=1",
            params![todo_id, input.calendar_id],
        )?;
        if changed != 1 {
            return Err(PersistenceError::NotFound);
        }
    }
    let details = UpdateLocalTodo {
        todo_id: todo_id.clone(),
        selected_occurrence_key: input.selected_occurrence_key.clone(),
        title: input.title,
        notes: input.notes,
        location: input.location,
        start_at_utc_ms: input.start_at_utc_ms,
        due_at_utc_ms: input.due_at_utc_ms,
        priority: input.priority,
        important: input.important,
        tag_names: input.tag_names,
        recurrence: input.recurrence,
    };
    update_local_todo_details(&transaction, &details, now)?;
    replan_local_notification_jobs_after_edit(&transaction, &todo_id, now)?;
    let result = if let Some(key) = input.selected_occurrence_key.as_deref() {
        get_occurrence(&transaction, &todo_id, key)?
    } else {
        get_active_at(&transaction, &todo_id, now)?
    };
    transaction.commit()?;
    Ok(result)
}

fn update_local_todo_details(
    transaction: &Transaction<'_>,
    input: &UpdateLocalTodo,
    now: i64,
) -> Result<(), PersistenceError> {
    let changed = transaction.execute(
        "UPDATE local_todos SET title=?1, notes=?2, location=?3, start_at_utc_ms=?4, due_at_utc_ms=?5, priority=?6, important=?7, updated_at_ms=?8
         WHERE todo_id=?9 AND active=1",
        params![input.title, input.notes, input.location, input.start_at_utc_ms, input.due_at_utc_ms, input.priority.rank(), input.important, now, input.todo_id],
    )?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    configure_recurrence(transaction, input, now)?;
    transaction.execute(
        "DELETE FROM local_todo_tags WHERE local_todo_id=?1",
        [&input.todo_id],
    )?;
    for (sort_order, name) in input.tag_names.iter().enumerate() {
        let normalized = normalize_tag_name(name);
        let tag_id = transaction
            .query_row(
                "SELECT tag_id FROM tags WHERE normalized_name=?1",
                [&normalized],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        transaction.execute(
            "INSERT INTO tags (tag_id, name, normalized_name, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(normalized_name) DO UPDATE SET name=excluded.name, updated_at_ms=excluded.updated_at_ms",
            params![tag_id, name, normalized, now],
        )?;
        let stable_tag_id: String = transaction.query_row(
            "SELECT tag_id FROM tags WHERE normalized_name=?1",
            [&normalized],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO local_todo_tags (local_todo_id, tag_id, sort_order) VALUES (?1, ?2, ?3)",
            params![input.todo_id, stable_tag_id, sort_order as i64],
        )?;
    }
    Ok(())
}

fn configure_recurrence(
    transaction: &Transaction<'_>,
    input: &UpdateLocalTodo,
    now: i64,
) -> Result<(), PersistenceError> {
    let existing: Option<(String,)> = transaction
        .query_row(
            "SELECT series_id FROM local_todo_recurrence WHERE local_todo_id=?1 AND schedule_state='active'",
            [&input.todo_id],
            |row| Ok((row.get(0)?,)),
        )
        .optional()?;
    match (&existing, &input.recurrence) {
        (None, None) => {
            if let Some(selected) = &input.selected_occurrence_key {
                let changed = transaction.execute("UPDATE local_todo_occurrences SET nominal_start_at_utc_ms=?3,nominal_due_at_utc_ms=?4,nominal_identity='RETAINED:'||?2 WHERE local_todo_id=?1 AND occurrence_key=?2 AND occurrence_state='active' AND EXISTS(SELECT 1 FROM local_todo_recurrence r WHERE r.local_todo_id=?1 AND r.schedule_state='stopped' AND r.retained_occurrence_key=?2)", params![input.todo_id,selected,input.start_at_utc_ms,input.due_at_utc_ms])?;
                if changed == 0
                    && transaction.query_row(
                        "SELECT EXISTS(SELECT 1 FROM local_todo_recurrence WHERE local_todo_id=?1 AND schedule_state='stopped')",
                        [&input.todo_id],
                        |row| row.get::<_, bool>(0),
                    )?
                {
                    return Err(PersistenceError::NotFound);
                }
            }
            Ok(())
        }
        (None, Some(recurrence)) => {
            let stopped_series = transaction.query_row("SELECT series_id FROM local_todo_recurrence WHERE local_todo_id=?1 AND schedule_state='stopped'", [&input.todo_id], |row| row.get::<_,String>(0)).optional()?;
            let series_id = stopped_series
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let due = input
                .due_at_utc_ms
                .ok_or(PersistenceError::RecurrenceRange)?;
            let start_offset = input.start_at_utc_ms.map(|start| start - due);
            let generated = generate_occurrences(
                recurrence.frequency,
                recurrence.interval,
                &recurrence.options,
                input.start_at_utc_ms,
                due,
            )
            .map_err(|_| PersistenceError::RecurrenceRange)?;
            if stopped_series.is_some() {
                transaction.execute("UPDATE local_todo_recurrence SET schedule_state='active',retained_occurrence_key=NULL,frequency=?2,interval_value=?3,timezone=?4,weekdays_mask=?5,month_days_mask=?6,year_months_mask=?7,end_kind=?8,end_count=?9,end_until_at_utc_ms=?10,anchor_due_at_utc_ms=?11,start_offset_ms=?12,updated_at_ms=?13 WHERE local_todo_id=?1", params![input.todo_id,recurrence.frequency.as_str(),recurrence.interval,recurrence.options.timezone,values_mask(&recurrence.options.weekdays,0),values_mask(&recurrence.options.month_days,1),values_mask(&recurrence.options.year_months,1),recurrence.options.end_kind.as_str(),recurrence.options.end_count,recurrence.options.end_until_at_utc_ms,due,start_offset,now])?;
            } else {
                transaction.execute("INSERT INTO local_todo_recurrence(local_todo_id,series_id,schedule_state,frequency,interval_value,timezone,weekdays_mask,month_days_mask,year_months_mask,end_kind,end_count,end_until_at_utc_ms,anchor_due_at_utc_ms,start_offset_ms,created_at_ms,updated_at_ms) VALUES(?1,?2,'active',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14)", params![input.todo_id,series_id,recurrence.frequency.as_str(),recurrence.interval,recurrence.options.timezone,values_mask(&recurrence.options.weekdays,0),values_mask(&recurrence.options.month_days,1),values_mask(&recurrence.options.year_months,1),recurrence.options.end_kind.as_str(),recurrence.options.end_count,recurrence.options.end_until_at_utc_ms,due,start_offset,now])?;
            }
            reconcile_local_occurrences(transaction, &input.todo_id, &series_id, &generated, now)?;
            if stopped_series.is_none() {
                let occurrence_key = transaction.query_row("SELECT occurrence_key FROM local_todo_occurrences WHERE local_todo_id=?1 ORDER BY nominal_due_at_utc_ms,sequence LIMIT 1", [&input.todo_id], |row| row.get::<_,String>(0))?;
                transaction.execute("UPDATE work_item_state SET local_occurrence_key=?1 WHERE local_todo_id=?2 AND local_occurrence_key=?3", params![occurrence_key,input.todo_id,SINGLETON_OCCURRENCE_KEY])?;
            }
            transaction.execute(
                "UPDATE local_todos SET recurrence_rule='fixed-v1' WHERE todo_id=?1",
                [&input.todo_id],
            )?;
            Ok(())
        }
        (Some((series_id,)), Some(recurrence)) => {
            let selected_due = input
                .due_at_utc_ms
                .ok_or(PersistenceError::RecurrenceRange)?;
            let (stored_anchor, selected_nominal): (i64, i64) = transaction.query_row("SELECT r.anchor_due_at_utc_ms,o.nominal_due_at_utc_ms FROM local_todo_recurrence r JOIN local_todo_occurrences o ON o.local_todo_id=r.local_todo_id AND o.occurrence_key=?2 WHERE r.local_todo_id=?1", params![input.todo_id,input.selected_occurrence_key.as_deref().ok_or(PersistenceError::NotFound)?], |row| Ok((row.get(0)?,row.get(1)?)))?;
            let due = stored_anchor
                .checked_add(
                    selected_due
                        .checked_sub(selected_nominal)
                        .ok_or(PersistenceError::RecurrenceRange)?,
                )
                .ok_or(PersistenceError::RecurrenceRange)?;
            let start_offset = input.start_at_utc_ms.map(|start| start - selected_due);
            let anchor_start = start_offset.and_then(|offset| due.checked_add(offset));
            let generated = generate_occurrences(
                recurrence.frequency,
                recurrence.interval,
                &recurrence.options,
                anchor_start,
                due,
            )
            .map_err(|_| PersistenceError::RecurrenceRange)?;
            transaction.execute("UPDATE local_todo_recurrence SET schedule_state='active',frequency=?1,interval_value=?2,timezone=?3,weekdays_mask=?4,month_days_mask=?5,year_months_mask=?6,end_kind=?7,end_count=?8,end_until_at_utc_ms=?9,anchor_due_at_utc_ms=?10,start_offset_ms=?11,updated_at_ms=?12 WHERE local_todo_id=?13", params![recurrence.frequency.as_str(),recurrence.interval,recurrence.options.timezone,values_mask(&recurrence.options.weekdays,0),values_mask(&recurrence.options.month_days,1),values_mask(&recurrence.options.year_months,1),recurrence.options.end_kind.as_str(),recurrence.options.end_count,recurrence.options.end_until_at_utc_ms,due,start_offset,now,input.todo_id])?;
            reconcile_local_occurrences(transaction, &input.todo_id, series_id, &generated, now)?;
            Ok(())
        }
        (Some((series_id,)), None) => {
            let selected = input
                .selected_occurrence_key
                .as_deref()
                .ok_or(PersistenceError::NotFound)?;
            let belongs: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM local_todo_occurrences WHERE local_todo_id=?1 AND series_id=?2 AND occurrence_key=?3 AND occurrence_state='active')", params![input.todo_id,series_id,selected], |row| row.get(0))?;
            if !belongs {
                return Err(PersistenceError::NotFound);
            }
            transaction.execute("UPDATE local_todo_occurrences SET occurrence_state=CASE WHEN occurrence_key=?2 THEN 'active' WHEN occurrence_state='active' THEN 'retired' ELSE occurrence_state END WHERE local_todo_id=?1", params![input.todo_id,selected])?;
            transaction.execute("UPDATE local_todo_recurrence SET schedule_state='stopped',retained_occurrence_key=?2,updated_at_ms=?3 WHERE local_todo_id=?1", params![input.todo_id,selected,now])?;
            transaction.execute(
                "UPDATE local_todos SET recurrence_rule=NULL WHERE todo_id=?1",
                [&input.todo_id],
            )?;
            Ok(())
        }
    }
}

fn reconcile_local_occurrences(
    transaction: &Transaction<'_>,
    todo_id: &str,
    series_id: &str,
    generated: &[GeneratedOccurrence],
    now: i64,
) -> Result<(), PersistenceError> {
    let mut statement = transaction.prepare("SELECT o.occurrence_id,o.sequence,o.occurrence_key,o.nominal_identity,COALESCE(w.completed,0),o.occurrence_state FROM local_todo_occurrences o LEFT JOIN work_item_state w ON w.local_todo_id=o.local_todo_id AND w.local_occurrence_key=o.occurrence_key WHERE o.local_todo_id=?1 ORDER BY o.sequence")?;
    let existing = statement
        .query_map([todo_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let completed_identities = existing
        .iter()
        .filter(|row| row.4)
        .map(|row| row.3.as_str())
        .collect::<HashSet<_>>();
    let mut retained_count = completed_identities.len();
    let mut desired = Vec::new();
    for occurrence in generated {
        let identity = format!("UTC:{}", occurrence.nominal_due_at_utc_ms);
        if completed_identities.contains(identity.as_str()) {
            desired.push((identity, *occurrence));
        } else if retained_count
            < usize::from(crate::domain::local_recurrence::MAX_RECURRENCE_OCCURRENCES)
        {
            retained_count += 1;
            desired.push((identity, *occurrence));
        }
    }
    for (occurrence_id, _, _, identity, completed, state) in &existing {
        if !completed
            && state != "cancelled"
            && !desired
                .iter()
                .any(|(desired_identity, _)| desired_identity == identity)
        {
            transaction.execute("UPDATE local_todo_occurrences SET occurrence_state='retired' WHERE occurrence_id=?1", [occurrence_id])?;
        }
    }
    let mut next_sequence = existing.iter().map(|row| row.1).max().unwrap_or(-1) + 1;
    for (identity, occurrence) in desired {
        if let Some((occurrence_id, _, _, _, _, state)) =
            existing.iter().find(|row| row.3 == identity)
        {
            if state != "cancelled" {
                transaction.execute("UPDATE local_todo_occurrences SET nominal_start_at_utc_ms=?1,nominal_due_at_utc_ms=?2,occurrence_state='active' WHERE occurrence_id=?3", params![occurrence.nominal_start_at_utc_ms,occurrence.nominal_due_at_utc_ms,occurrence_id])?;
            }
        } else {
            let key = fixed_occurrence_key(
                series_id,
                u64::try_from(next_sequence).map_err(|_| PersistenceError::RecurrenceRange)?,
            );
            transaction.execute("INSERT INTO local_todo_occurrences(occurrence_id,local_todo_id,series_id,sequence,occurrence_key,nominal_identity,nominal_start_at_utc_ms,nominal_due_at_utc_ms,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![Uuid::new_v4().to_string(),todo_id,series_id,next_sequence,key,identity,occurrence.nominal_start_at_utc_ms,occurrence.nominal_due_at_utc_ms,now])?;
            next_sequence += 1;
        }
    }
    Ok(())
}

fn active_work_item(
    transaction: &Transaction<'_>,
    todo_id: &str,
    occurrence_key: &str,
    now: i64,
) -> Result<String, PersistenceError> {
    let active: bool = transaction
        .query_row(
            "SELECT active FROM local_todos WHERE todo_id=?1",
            [todo_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    if !active {
        return Err(PersistenceError::NotFound);
    }
    let exists: bool = if occurrence_key == SINGLETON_OCCURRENCE_KEY {
        transaction.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM local_todo_recurrence WHERE local_todo_id=?1)",
            [todo_id],
            |row| row.get(0),
        )?
    } else {
        transaction.query_row("SELECT EXISTS(SELECT 1 FROM local_todo_occurrences WHERE local_todo_id=?1 AND occurrence_key=?2 AND occurrence_state='active')", params![todo_id,occurrence_key], |row| row.get(0))?
    };
    if !exists {
        return Err(PersistenceError::NotFound);
    }
    if let Some(id) = transaction.query_row("SELECT work_item_id FROM work_item_state WHERE local_todo_id=?1 AND local_occurrence_key=?2", params![todo_id,occurrence_key], |row| row.get::<_,String>(0)).optional()? { return Ok(id); }
    let work_item_id = Uuid::new_v4().to_string();
    transaction.execute("INSERT INTO work_item_state(work_item_id,source_kind,local_todo_id,local_occurrence_key,last_seen_at_ms) VALUES(?1,'local_todo_occurrence',?2,?3,?4)", params![work_item_id,todo_id,occurrence_key,now])?;
    Ok(work_item_id)
}

fn subtask_work_item(
    transaction: &Transaction<'_>,
    todo_id: &str,
    subtask_id: &str,
) -> Result<String, PersistenceError> {
    transaction.query_row("SELECT w.work_item_id FROM subtasks s JOIN work_item_state w ON w.work_item_id=s.work_item_id JOIN local_todos t ON t.todo_id=w.local_todo_id WHERE s.subtask_id=?1 AND t.todo_id=?2 AND t.active=1", params![subtask_id,todo_id], |row| row.get(0)).optional()?.ok_or(PersistenceError::NotFound)
}

fn set_completed(
    connection: &mut Connection,
    todo_id: &str,
    occurrence_key: &str,
    completed: bool,
) -> Result<LocalTodo, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let work_item_id = active_work_item(&transaction, todo_id, occurrence_key, now)?;
    transaction.execute(
        "UPDATE work_item_state SET completed=?1, completed_at_ms=CASE WHEN ?1 THEN ?2 ELSE NULL END, last_seen_at_ms=?2 WHERE work_item_id=?3",
        params![completed, now, work_item_id],
    )?;
    transaction.execute(
        "UPDATE local_todos SET updated_at_ms=?1 WHERE todo_id=?2",
        params![now, todo_id],
    )?;
    transaction.commit()?;
    get_occurrence(connection, todo_id, occurrence_key)
}

fn create_subtask(
    connection: &mut Connection,
    input: CreateSubtask,
) -> Result<LocalTodo, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let work_item_id = active_work_item(&transaction, &input.todo_id, &input.occurrence_key, now)?;
    let sort_order: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(sort_order)+1, 0) FROM subtasks WHERE work_item_id=?1",
        [&work_item_id],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO subtasks (subtask_id, work_item_id, text, sort_order, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![Uuid::new_v4().to_string(), work_item_id, input.text, sort_order, now],
    )?;
    transaction.execute(
        "UPDATE local_todos SET updated_at_ms=?1 WHERE todo_id=?2",
        params![now, input.todo_id],
    )?;
    transaction.commit()?;
    get_active(connection, &input.todo_id)
}

/// Adjacent stable-ID CAS, not a whole-list revision or ABA guarantee.
fn move_local_subtask(
    connection: &mut Connection,
    input: crate::domain::local_todo::MoveLocalSubtask,
) -> Result<LocalTodo, PersistenceError> {
    use crate::domain::local_todo::SubtaskMoveDirection;
    let input = input.validate()?;
    let transaction = connection.transaction()?;
    let target = transaction
        .query_row(
            "SELECT w.work_item_id,s.sort_order FROM subtasks s
         JOIN work_item_state w ON w.work_item_id=s.work_item_id
         JOIN local_todos t ON t.todo_id=w.local_todo_id
         WHERE t.todo_id=?1 AND t.active=1 AND t.recurrence_rule IS NULL
           AND NOT EXISTS(SELECT 1 FROM local_todo_recurrence r WHERE r.local_todo_id=t.todo_id)
           AND w.source_kind='local_todo_occurrence' AND w.local_occurrence_key='SINGLETON'
           AND s.subtask_id=?2",
            params![input.todo_id, input.subtask_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        .ok_or(PersistenceError::SubtaskMoveUnavailable)?;
    let (work_item_id, rank) = target;
    if rank < 0 {
        return Err(PersistenceError::InvalidSubtaskOrder);
    }
    let neighbor_sql = match input.direction {
        SubtaskMoveDirection::Up => {
            "SELECT subtask_id,sort_order FROM subtasks WHERE work_item_id=?1 AND sort_order<?2 ORDER BY sort_order DESC LIMIT 1"
        }
        SubtaskMoveDirection::Down => {
            "SELECT subtask_id,sort_order FROM subtasks WHERE work_item_id=?1 AND sort_order>?2 ORDER BY sort_order ASC LIMIT 1"
        }
    };
    let neighbor = transaction
        .query_row(neighbor_sql, params![work_item_id, rank], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .optional()?;
    if neighbor.as_ref().map(|(id, _)| id) != input.expected_neighbor_id.as_ref() {
        return Err(PersistenceError::StaleSubtaskOrder);
    }
    let now = unix_time_ms()?;
    if let Some((neighbor_id, neighbor_rank)) = neighbor {
        let maximum: i64 = transaction.query_row(
            "SELECT MAX(sort_order) FROM subtasks WHERE work_item_id=?1",
            [&work_item_id],
            |row| row.get(0),
        )?;
        let spare = maximum
            .checked_add(1)
            .filter(|value| *value >= 0)
            .ok_or(PersistenceError::InvalidSubtaskOrder)?;
        if neighbor_rank < 0 {
            return Err(PersistenceError::InvalidSubtaskOrder);
        }
        // UNIQUE is immediate: free one rank before swapping; never delete/recreate.
        for (id, old, new) in [
            (input.subtask_id.as_str(), rank, spare),
            (neighbor_id.as_str(), neighbor_rank, rank),
            (input.subtask_id.as_str(), spare, neighbor_rank),
        ] {
            let changed = transaction.execute(
                "UPDATE subtasks SET sort_order=?1 WHERE work_item_id=?2 AND subtask_id=?3 AND sort_order=?4",
                params![new, work_item_id, id, old],
            )?;
            if changed != 1 {
                return Err(PersistenceError::InvalidSubtaskOrder);
            }
        }
        // Preserve every non-ordering field, including all modification stamps.
    }
    let result = get_active_at(&transaction, &input.todo_id, now)?;
    transaction.commit()?;
    Ok(result)
}

fn update_subtask(
    connection: &mut Connection,
    input: UpdateSubtask,
) -> Result<LocalTodo, PersistenceError> {
    mutate_subtask(
        connection,
        &input.todo_id,
        &input.subtask_id,
        |transaction, work_item_id, now| {
            transaction.execute("UPDATE subtasks SET text=?1, updated_at_ms=?2 WHERE subtask_id=?3 AND work_item_id=?4", params![input.text, now, input.subtask_id, work_item_id])
        },
    )
}

fn set_subtask_completed(
    connection: &mut Connection,
    todo_id: &str,
    subtask_id: &str,
    completed: bool,
) -> Result<LocalTodo, PersistenceError> {
    mutate_subtask(
        connection,
        todo_id,
        subtask_id,
        |transaction, work_item_id, now| {
            transaction.execute("UPDATE subtasks SET completed=?1, updated_at_ms=?2 WHERE subtask_id=?3 AND work_item_id=?4", params![completed, now, subtask_id, work_item_id])
        },
    )
}

fn mutate_subtask(
    connection: &mut Connection,
    todo_id: &str,
    subtask_id: &str,
    mutate: impl FnOnce(&Transaction<'_>, &str, i64) -> rusqlite::Result<usize>,
) -> Result<LocalTodo, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let work_item_id = subtask_work_item(&transaction, todo_id, subtask_id)?;
    if mutate(&transaction, &work_item_id, now)? != 1 {
        return Err(PersistenceError::NotFound);
    }
    transaction.execute(
        "UPDATE local_todos SET updated_at_ms=?1 WHERE todo_id=?2",
        params![now, todo_id],
    )?;
    transaction.commit()?;
    get_active(connection, todo_id)
}

fn delete_subtask(
    connection: &mut Connection,
    todo_id: &str,
    subtask_id: &str,
) -> Result<LocalTodo, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let work_item_id = subtask_work_item(&transaction, todo_id, subtask_id)?;
    let sort_order = transaction
        .query_row(
            "SELECT sort_order FROM subtasks WHERE subtask_id=?1 AND work_item_id=?2",
            params![subtask_id, work_item_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    transaction.execute(
        "DELETE FROM subtasks WHERE subtask_id=?1 AND work_item_id=?2",
        params![subtask_id, work_item_id],
    )?;
    transaction.execute(
        "UPDATE subtasks SET sort_order=sort_order-1 WHERE work_item_id=?1 AND sort_order>?2",
        params![work_item_id, sort_order],
    )?;
    transaction.execute(
        "UPDATE local_todos SET updated_at_ms=?1 WHERE todo_id=?2",
        params![now, todo_id],
    )?;
    transaction.commit()?;
    get_active(connection, todo_id)
}

fn soft_delete(connection: &mut Connection, todo_id: &str) -> Result<(), PersistenceError> {
    let now = unix_time_ms()?;
    soft_delete_at(connection, todo_id, now)
}

fn soft_delete_occurrence(
    connection: &mut Connection,
    todo_id: &str,
    occurrence_key: &str,
) -> Result<(), PersistenceError> {
    let now = unix_time_ms()?;
    let changed = connection.execute("UPDATE local_todo_occurrences SET occurrence_state='cancelled',cancelled_at_ms=?3 WHERE local_todo_id=?1 AND occurrence_key=?2 AND occurrence_state='active' AND EXISTS(SELECT 1 FROM local_todo_recurrence r WHERE r.local_todo_id=?1 AND r.schedule_state='active')", params![todo_id,occurrence_key,now])?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    connection.execute("UPDATE notification_jobs SET state='cancelled',finished_at_utc_ms=?3 WHERE source_kind='local_todo' AND source_id=?1 AND occurrence_key=?2 AND state='planned'", params![todo_id,occurrence_key,now])?;
    Ok(())
}

fn restore_recently_deleted_occurrence(
    connection: &mut Connection,
    todo_id: &str,
    occurrence_key: &str,
) -> Result<LocalTodo, PersistenceError> {
    let now = unix_time_ms()?;
    let changed = connection.execute("UPDATE local_todo_occurrences SET occurrence_state='active',cancelled_at_ms=NULL WHERE local_todo_id=?1 AND occurrence_key=?2 AND occurrence_state='cancelled' AND cancelled_at_ms BETWEEN ?3 AND ?4", params![todo_id,occurrence_key,now.saturating_sub(6_000),now])?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    get_occurrence(connection, todo_id, occurrence_key)
}

fn soft_delete_at(
    connection: &mut Connection,
    todo_id: &str,
    now: i64,
) -> Result<(), PersistenceError> {
    let transaction = connection.transaction()?;
    let changed = transaction.execute(
        "UPDATE local_todos SET active=0, updated_at_ms=?1 WHERE todo_id=?2 AND active=1",
        params![now, todo_id],
    )?;
    if changed == 1 {
        // Delete wins before a later actor claim. Already claimed/terminal work
        // is not recalled; recurring/calendar semantics remain unchanged.
        transaction.execute(
            "UPDATE notification_jobs SET state='cancelled',finished_at_utc_ms=?2
             WHERE source_kind='local_todo' AND source_id=?1 AND occurrence_key='SINGLETON' AND state='planned'
               AND EXISTS(SELECT 1 FROM local_todos t WHERE t.todo_id=?1 AND t.recurrence_rule IS NULL
                 AND NOT EXISTS(SELECT 1 FROM local_todo_recurrence r WHERE r.local_todo_id=t.todo_id))",
            params![todo_id, now],
        )?;
        transaction.commit()?;
        Ok(())
    } else {
        Err(PersistenceError::NotFound)
    }
}

fn restore_recently_deleted(
    connection: &mut Connection,
    todo_id: &str,
) -> Result<LocalTodo, PersistenceError> {
    let now = unix_time_ms()?;
    restore_recently_deleted_at(connection, todo_id, now)
}

fn restore_recently_deleted_at(
    connection: &mut Connection,
    todo_id: &str,
    now: i64,
) -> Result<LocalTodo, PersistenceError> {
    let changed = connection.execute(
        "UPDATE local_todos SET active=1, updated_at_ms=?1 WHERE todo_id=?2 AND active=0 AND updated_at_ms BETWEEN ?3 AND ?1",
        params![now, todo_id, now.saturating_sub(6_000)],
    )?;
    if changed == 1 {
        get_active(connection, todo_id)
    } else {
        Err(PersistenceError::NotFound)
    }
}

fn get_app_settings(connection: &Connection) -> Result<AppSettings, PersistenceError> {
    connection.query_row(
        "SELECT window_mode,sync_interval_minutes,sync_on_startup,desktop_list_mode,desktop_opacity_percent,desktop_show_calendar_name,desktop_show_time,desktop_show_tags,main_show_calendar_name,main_show_time,main_show_tags,start_hidden_in_tray,launch_at_login,local_todo_color,updated_at_ms,desktop_pinned,desktop_collapsed,default_calendar_id,language,local_display_past_days,local_display_future_days,theme_palette,color_mode,reminder_notifications_enabled FROM app_settings WHERE singleton_id=1",
        [],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, u16>(1)?, row.get::<_, bool>(2)?, row.get::<_, bool>(3)?, row.get::<_, u8>(4)?, row.get::<_, bool>(5)?, row.get::<_, bool>(6)?, row.get::<_, bool>(7)?, row.get::<_, bool>(8)?, row.get::<_, bool>(9)?, row.get::<_, bool>(10)?, row.get::<_, bool>(11)?, row.get::<_, bool>(12)?, row.get::<_, String>(13)?, row.get::<_, i64>(14)?, row.get::<_, bool>(15)?, row.get::<_, bool>(16)?, row.get::<_, Option<String>>(17)?, row.get::<_, String>(18)?, row.get::<_, Option<u16>>(19)?, row.get::<_, Option<u16>>(20)?, row.get::<_, String>(21)?, row.get::<_, String>(22)?, row.get::<_, bool>(23)?)),
    ).map_err(PersistenceError::Sqlite).and_then(|(mode, sync_interval_minutes, sync_on_startup, desktop_list_mode, desktop_opacity_percent, desktop_show_calendar_name, desktop_show_time, desktop_show_tags, main_show_calendar_name, main_show_time, main_show_tags, start_hidden_in_tray, launch_at_login, local_todo_color, updated_at_ms, desktop_pinned, desktop_collapsed, default_calendar_id, language, local_display_past_days, local_display_future_days, theme_palette, color_mode, reminder_notifications_enabled)| {
        Ok(AppSettings { language: crate::domain::settings::AppLanguage::from_storage(&language)?, theme_palette: crate::domain::settings::ThemePalette::from_storage(&theme_palette)?, color_mode: crate::domain::settings::ColorMode::from_storage(&color_mode)?, window_mode: WindowMode::from_storage(&mode)?, sync_interval_minutes, sync_on_startup, desktop_list_mode, desktop_opacity_percent, desktop_show_calendar_name, desktop_show_time, desktop_show_tags, main_show_calendar_name, main_show_time, main_show_tags, start_hidden_in_tray, launch_at_login, local_todo_color, local_display_past_days, local_display_future_days, default_calendar_id, desktop_pinned, desktop_collapsed, reminder_notifications_enabled, updated_at_ms })
    })
}

/// Writes the desktop list's position lock. The Settings form never covers this
/// column, so its own updates cannot clear the user's choice.
fn store_desktop_pinned(connection: &Connection, pinned: bool) -> Result<(), PersistenceError> {
    let now = unix_time_ms()?;
    let changed = connection.execute(
        "UPDATE app_settings SET desktop_pinned=?1,updated_at_ms=?2 WHERE singleton_id=1",
        params![pinned, now],
    )?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    Ok(())
}

/// Writes the desktop list's folded presentation choice. The folded rectangle
/// itself stays out of `window_placements`, so a launch restores the expanded
/// rectangle and only then folds it.
fn store_desktop_collapsed(
    connection: &Connection,
    collapsed: bool,
) -> Result<(), PersistenceError> {
    let now = unix_time_ms()?;
    let changed = connection.execute(
        "UPDATE app_settings SET desktop_collapsed=?1,updated_at_ms=?2 WHERE singleton_id=1",
        params![collapsed, now],
    )?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    Ok(())
}

fn update_app_settings(
    connection: &mut Connection,
    input: UpdateAppSettings,
    now: i64,
) -> Result<AppSettings, PersistenceError> {
    let transaction = connection.transaction()?;
    let was_enabled: bool = transaction.query_row(
        "SELECT reminder_notifications_enabled FROM app_settings WHERE singleton_id=1",
        [],
        |row| row.get(0),
    )?;
    if !was_enabled && input.reminder_notifications_enabled {
        // Everything that came due while delivery was off is spent. Only fires
        // still in the future may resume, so re-enabling cannot flush a batch.
        transaction.execute(
            "UPDATE notification_jobs SET state='suppressed',finished_at_utc_ms=?1 WHERE state='planned' AND fire_at_utc_ms<=?1",
            [now],
        )?;
    }
    // The desktop list's own position lock and folded choice are intentionally
    // absent here: the Settings form must never overwrite them.
    let changed = transaction.execute(
        "UPDATE app_settings SET window_mode=?1,sync_interval_minutes=?2,sync_on_startup=?3,desktop_list_mode=?4,desktop_opacity_percent=?5,desktop_show_calendar_name=?6,desktop_show_time=?7,desktop_show_tags=?8,main_show_calendar_name=?9,main_show_time=?10,main_show_tags=?11,start_hidden_in_tray=?12,launch_at_login=?13,local_todo_color=?14,default_calendar_id=?15,updated_at_ms=?16,language=?17,local_display_past_days=?18,local_display_future_days=?19,theme_palette=?20,color_mode=?21,reminder_notifications_enabled=?22 WHERE singleton_id=1",
        params![input.window_mode.as_str(), input.sync_interval_minutes, input.sync_on_startup, input.desktop_list_mode, input.desktop_opacity_percent, input.desktop_show_calendar_name, input.desktop_show_time, input.desktop_show_tags, input.main_show_calendar_name, input.main_show_time, input.main_show_tags, input.start_hidden_in_tray, input.launch_at_login, input.local_todo_color, input.default_calendar_id, now, input.language.as_str(), input.local_display_past_days, input.local_display_future_days, input.theme_palette.as_str(), input.color_mode.as_str(), input.reminder_notifications_enabled],
    )?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    let settings = get_app_settings(&transaction)?;
    transaction.commit()?;
    Ok(settings)
}

/// Window labels that may own a persisted placement. `tray_menu` is excluded on
/// purpose: it is anchored to the tray icon and sized from its own content.
const PLACEMENT_WINDOW_LABELS: [&str; 2] = ["main", "desktop"];

fn list_window_placements(
    connection: &Connection,
) -> Result<Vec<StoredWindowPlacement>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT window_label,monitor_name,x_dip,y_dip,width_dip,height_dip,maximized FROM window_placements ORDER BY window_label",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(StoredWindowPlacement {
            window_label: row.get(0)?,
            placement: WindowPlacement {
                monitor_name: row.get(1)?,
                x_dip: row.get(2)?,
                y_dip: row.get(3)?,
                width_dip: row.get(4)?,
                height_dip: row.get(5)?,
                maximized: row.get(6)?,
            },
        })
    })?;
    let mut placements = Vec::new();
    for row in rows {
        placements.push(row?);
    }
    Ok(placements)
}

fn save_window_placement(
    connection: &mut Connection,
    window_label: &str,
    placement: WindowPlacement,
) -> Result<(), PersistenceError> {
    if !PLACEMENT_WINDOW_LABELS.contains(&window_label) {
        return Err(PersistenceError::InvalidWindowLabel);
    }
    let placement = placement.validate()?;
    let now = unix_time_ms()?;
    connection.execute(
        "INSERT INTO window_placements(window_label,monitor_name,x_dip,y_dip,width_dip,height_dip,maximized,updated_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(window_label) DO UPDATE SET
             monitor_name=excluded.monitor_name,x_dip=excluded.x_dip,y_dip=excluded.y_dip,
             width_dip=excluded.width_dip,height_dip=excluded.height_dip,maximized=excluded.maximized,
             updated_at_ms=excluded.updated_at_ms",
        params![
            window_label,
            placement.monitor_name,
            placement.x_dip,
            placement.y_dip,
            placement.width_dip,
            placement.height_dip,
            placement.maximized,
            now
        ],
    )?;
    Ok(())
}

fn upsert_account_profile(
    connection: &mut Connection,
    input: RegisterAccountProfile,
) -> Result<AccountProfile, PersistenceError> {
    let input = input.validate()?;
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let owner = transaction
        .query_row(
            "SELECT account_id FROM accounts WHERE credential_ref=?1",
            [&input.credential_ref],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if owner.as_deref().is_some_and(|id| id != input.account_id) {
        return Err(PersistenceError::IdentityConflict);
    }
    let archived = transaction
        .query_row(
            "SELECT removed_at_ms FROM accounts WHERE account_id=?1",
            [&input.account_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?;
    if matches!(archived, Some(Some(_))) {
        return Err(PersistenceError::NotFound);
    }
    transaction.execute(
        "INSERT INTO accounts(account_id,provider_kind,user_label,credential_ref,region_profile,enabled,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)
         ON CONFLICT(account_id) DO UPDATE SET provider_kind=excluded.provider_kind,user_label=excluded.user_label,credential_ref=excluded.credential_ref,region_profile=excluded.region_profile,enabled=excluded.enabled,updated_at_ms=excluded.updated_at_ms",
        params![input.account_id, input.provider_kind.as_str(), input.user_label, input.credential_ref, input.region_profile, input.enabled, now],
    )?;
    transaction.commit()?;
    account_profile(connection, &input.account_id)
}

type AccountRow = (
    String,
    String,
    String,
    String,
    String,
    bool,
    bool,
    bool,
    i64,
    i64,
);

fn account_profile(
    connection: &Connection,
    account_id: &str,
) -> Result<AccountProfile, PersistenceError> {
    validate_account_id(account_id)?;
    connection.query_row(
        "SELECT account_id,provider_kind,user_label,credential_ref,region_profile,enabled,credential_configured,remote_editing_enabled,created_at_ms,updated_at_ms FROM accounts WHERE account_id=?1 AND removed_at_ms IS NULL",
        [account_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?)),
    ).optional()?.ok_or(PersistenceError::NotFound).and_then(account_profile_from_row)
}

fn list_account_profiles(connection: &Connection) -> Result<Vec<AccountProfile>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT account_id,provider_kind,user_label,credential_ref,region_profile,enabled,credential_configured,remote_editing_enabled,created_at_ms,updated_at_ms FROM accounts WHERE removed_at_ms IS NULL ORDER BY account_id",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })?
        .collect::<Result<Vec<AccountRow>, _>>()?;
    rows.into_iter().map(account_profile_from_row).collect()
}

fn archive_account_profile(
    connection: &mut Connection,
    account_id: &str,
) -> Result<(), PersistenceError> {
    validate_account_id(account_id)?;
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let changed = transaction.execute(
        "UPDATE accounts SET enabled=0,credential_configured=0,remote_editing_enabled=0,removed_at_ms=?2,updated_at_ms=?2 WHERE account_id=?1 AND removed_at_ms IS NULL",
        params![account_id, now],
    )?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    transaction.execute(
        "UPDATE notification_jobs SET state='cancelled',finished_at_utc_ms=?2 WHERE source_kind='calendar_occurrence' AND state='planned' AND source_id IN (SELECT series_id FROM event_series WHERE account_id=?1)",
        params![account_id, now],
    )?;
    transaction.commit()?;
    Ok(())
}

fn account_profile_from_row(row: AccountRow) -> Result<AccountProfile, PersistenceError> {
    Ok(AccountProfile {
        account_id: row.0,
        provider_kind: ProviderKind::from_storage(&row.1)?,
        user_label: row.2,
        credential_ref: row.3,
        region_profile: row.4,
        enabled: row.5,
        credential_configured: row.6,
        remote_editing_enabled: row.7,
        created_at_ms: row.8,
        updated_at_ms: row.9,
    })
}

fn set_account_enabled(
    connection: &mut Connection,
    account_id: &str,
    enabled: bool,
) -> Result<AccountProfile, PersistenceError> {
    validate_account_id(account_id)?;
    let changed = connection.execute(
        "UPDATE accounts SET enabled=?2,updated_at_ms=?3 WHERE account_id=?1 AND removed_at_ms IS NULL",
        params![account_id, enabled, unix_time_ms()?],
    )?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    account_profile(connection, account_id)
}

fn set_account_credential_configured(
    connection: &mut Connection,
    account_id: &str,
) -> Result<AccountProfile, PersistenceError> {
    validate_account_id(account_id)?;
    let changed = connection.execute(
        "UPDATE accounts SET credential_configured=1,updated_at_ms=?2 WHERE account_id=?1 AND removed_at_ms IS NULL",
        params![account_id, unix_time_ms()?],
    )?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    account_profile(connection, account_id)
}

fn set_account_remote_editing_enabled(
    connection: &mut Connection,
    account_id: &str,
    enabled: bool,
) -> Result<AccountProfile, PersistenceError> {
    validate_account_id(account_id)?;
    let changed = connection.execute(
        "UPDATE accounts SET remote_editing_enabled=?2,updated_at_ms=?3 WHERE account_id=?1 AND removed_at_ms IS NULL",
        params![account_id, enabled, unix_time_ms()?],
    )?;
    if changed != 1 {
        return Err(PersistenceError::NotFound);
    }
    account_profile(connection, account_id)
}

fn promote_account_inventory(
    connection: &mut Connection,
    stage: AccountInventoryStage,
    horizon_now_ms: i64,
) -> Result<(), PersistenceError> {
    promote_account_inventory_scoped(connection, stage, horizon_now_ms, None)
}
fn promote_account_inventory_scoped(
    connection: &mut Connection,
    stage: AccountInventoryStage,
    horizon_now_ms: i64,
    probe_rows: Option<&Vec<(String, String, String)>>,
) -> Result<(), PersistenceError> {
    if !stage.complete || !stage.authoritative {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    let window = standard_horizon_window(horizon_now_ms)?;
    let transaction = connection.transaction()?;
    let usable = transaction
        .query_row(
            "SELECT enabled=1 AND removed_at_ms IS NULL FROM accounts WHERE account_id=?1",
            [&stage.account_id],
            |row| row.get::<_, bool>(0),
        )
        .optional()?;
    if usable == Some(false) {
        return Err(PersistenceError::AccountDisabled);
    }
    let selected_calendars = if let Some(rows) = probe_rows {
        if rows
            .iter()
            .any(|(account, _, _)| account != &stage.account_id)
            || stage.calendars.iter().any(|calendar| {
                !rows.iter().any(|(_, id, route)| {
                    id == &calendar.calendar_id
                        && collection_routes_match(route, &calendar.collection_url)
                })
            })
        {
            return Err(PersistenceError::UnsafeSyncStage);
        }
        rows.iter()
            .map(|(_, id, _)| id.clone())
            .collect::<HashSet<_>>()
    } else {
        let mut statement = transaction.prepare(
            "SELECT calendar_id FROM calendars WHERE account_id=?1 AND available=1 AND selected=1",
        )?;
        statement
            .query_map([&stage.account_id], |row| row.get::<_, String>(0))?
            .collect::<Result<HashSet<_>, _>>()?
    };
    let existing_calendar_count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM calendars WHERE account_id=?1",
        [&stage.account_id],
        |row| row.get(0),
    )?;
    if existing_calendar_count > 0
        && (selected_calendars.len() != stage.calendars.len()
            || stage
                .calendars
                .iter()
                .any(|calendar| !selected_calendars.contains(&calendar.calendar_id)))
    {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    let prior_projection_sources = {
        let mut statement = transaction.prepare(
            "SELECT DISTINCT p.series_id,p.resource_id FROM calendar_event_projections p
             JOIN event_series s ON s.series_id=p.series_id WHERE s.account_id=?1",
        )?;
        statement
            .query_map([&stage.account_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<HashMap<_, _>, _>>()?
    };
    let mut staged_series = HashSet::new();
    let mut omitted_protected_resources = HashSet::new();
    transaction.execute(
        "INSERT INTO accounts(account_id,provider_kind,user_label,credential_ref,region_profile,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?6)
         ON CONFLICT(account_id) DO UPDATE SET updated_at_ms=excluded.updated_at_ms",
        params![stage.account_id, stage.provider_kind, stage.user_label, stage.credential_ref, stage.region_profile, stage.observed_at_ms],
    )?;
    // Validate versioned pending edits against committed availability, writability and
    // active cache before full inventory introduces temporary omission markers.
    // Any later stage failure rolls back these conflict records with the stage.
    for calendar in &stage.calendars {
        for resource in &calendar.resources {
            let pending_resource: Option<String> = transaction.query_row(
                "SELECT r.resource_id FROM remote_resources r JOIN outbox_operations o ON o.resource_id=r.resource_id
                 WHERE r.calendar_id=?1 AND r.exact_href=?2 AND o.operation_kind IN ('update_fields','update_event') AND o.state='pending'",
                params![calendar.calendar_id, resource.href], |row| row.get(0),
            ).optional()?;
            if let Some(resource_id) = pending_resource {
                record_remote_change_conflict(
                    &transaction,
                    &resource_id,
                    &resource.etag,
                    &resource.raw_ics,
                    stage.observed_at_ms,
                )?;
            }
        }
    }
    for calendar in &stage.calendars {
        if let Some(rows) = probe_rows {
            for resource in &calendar.resources {
                let foreign_alias: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM event_series s JOIN remote_resources r ON r.series_id=s.series_id WHERE s.account_id=?1 AND s.uid=?2 AND r.calendar_id<>?3 AND r.calendar_id<>?4)",
                    params![stage.account_id, resource.uid, rows[0].1, rows[1].1], |r| r.get(0),
                )?;
                if foreign_alias {
                    return Err(PersistenceError::IdentityConflict);
                }
            }
        }
        let existing_calendar = transaction
            .query_row(
                "SELECT account_id,collection_url FROM calendars WHERE calendar_id=?1",
                [&calendar.calendar_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if existing_calendar
            .as_ref()
            .is_some_and(|(account_id, _)| account_id != &stage.account_id)
        {
            return Err(PersistenceError::IdentityConflict);
        }
        let collection_href = existing_calendar
            .as_ref()
            .map_or(calendar.collection_url.as_str(), |(_, href)| href.as_str());
        if !collection_routes_match(collection_href, &calendar.collection_url)
            || calendar
                .resources
                .iter()
                .any(|resource| !resource_belongs_to_collection(collection_href, &resource.href))
        {
            return Err(PersistenceError::UnsafeSyncStage);
        }
        transaction.execute(
            "INSERT INTO calendars(calendar_id,account_id,collection_url,display_name,server_color,privileges,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)
             ON CONFLICT(calendar_id) DO UPDATE SET account_id=excluded.account_id,display_name=excluded.display_name,server_color=excluded.server_color,privileges=excluded.privileges,available=1,updated_at_ms=excluded.updated_at_ms",
            params![calendar.calendar_id, stage.account_id, calendar.collection_url, calendar.display_name, calendar.server_color, calendar.privileges, stage.observed_at_ms],
        )?;
        {
            let mut statement = transaction.prepare(
                "SELECT resource_id,exact_href FROM remote_resources WHERE calendar_id=?1 AND lifecycle IN ('active','missing_candidate')",
            )?;
            let resources = statement
                .query_map([&calendar.calendar_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (resource_id, href) in resources {
                // Ordinary inventory absence cannot consume exact-target recovery.
                // Preserve its cache and evidence while the rest of the stage commits.
                if resource_has_inflight_mutation(&transaction, &resource_id)? {
                    if !calendar
                        .resources
                        .iter()
                        .any(|resource| resource.href == href)
                    {
                        omitted_protected_resources.insert(resource_id);
                    }
                    continue;
                }
                transaction.execute(
                    "UPDATE remote_resources SET lifecycle='missing_candidate',candidate_evidence_valid=0,updated_at_ms=?2 WHERE resource_id=?1",
                    params![resource_id, stage.observed_at_ms],
                )?;
            }
        }
        {
            let mut statement = transaction.prepare(
                "SELECT resource_id,exact_href FROM remote_resources
                 WHERE calendar_id=?1 AND lifecycle='missing_candidate'",
            )?;
            let candidates = statement
                .query_map([&calendar.calendar_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (resource_id, href) in candidates {
                if resource_has_inflight_mutation(&transaction, &resource_id)? {
                    continue;
                }
                if resource_belongs_to_collection(collection_href, &href) {
                    transaction.execute(
                        "UPDATE remote_resources SET candidate_evidence_valid=1 WHERE resource_id=?1",
                        [&resource_id],
                    )?;
                }
            }
        }

        for resource in &calendar.resources {
            let series_id = transaction
                .query_row(
                    "SELECT series_id FROM event_series WHERE account_id=?1 AND uid=?2",
                    params![stage.account_id, resource.uid],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            transaction.execute(
                "INSERT INTO event_series(series_id,account_id,uid,current_calendar_id,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?5)
                 ON CONFLICT(series_id) DO UPDATE SET current_calendar_id=excluded.current_calendar_id,updated_at_ms=excluded.updated_at_ms",
                params![series_id, stage.account_id, resource.uid, calendar.calendar_id, stage.observed_at_ms],
            )?;
            let existing_resource = transaction
                .query_row(
                    "SELECT resource_id,series_id FROM remote_resources WHERE calendar_id=?1 AND exact_href=?2",
                    params![calendar.calendar_id, resource.href],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if existing_resource
                .as_ref()
                .is_some_and(|(_, existing_series_id)| existing_series_id != &series_id)
            {
                return Err(PersistenceError::IdentityConflict);
            }
            let resource_id = remote_creation::staged_identity(
                &transaction,
                &stage.account_id,
                &calendar.calendar_id,
                collection_href,
                resource,
                existing_resource.as_ref().map(|(id, _)| id.as_str()),
                &series_id,
                horizon_now_ms,
            )?;
            staged_series.insert(series_id.clone());
            record_remote_change_conflict(
                &transaction,
                &resource_id,
                &resource.etag,
                &resource.raw_ics,
                stage.observed_at_ms,
            )?;
            transaction.execute(
                "INSERT INTO remote_resources(resource_id,calendar_id,series_id,exact_href,etag,content_type,raw_ics,parse_status,lifecycle,last_seen_at_ms,created_at_ms,updated_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'active',?9,?9,?9)
                 ON CONFLICT(resource_id) DO UPDATE SET series_id=excluded.series_id,etag=excluded.etag,content_type=excluded.content_type,raw_ics=excluded.raw_ics,parse_status=excluded.parse_status,lifecycle='active',candidate_evidence_valid=0,last_seen_at_ms=excluded.last_seen_at_ms,remote_deleted_at_ms=NULL,updated_at_ms=excluded.updated_at_ms",
                params![resource_id, calendar.calendar_id, series_id, resource.href, resource.etag, resource.content_type, resource.raw_ics, resource.parse_status.as_str(), stage.observed_at_ms],
            )?;
            transaction.execute(
                "INSERT INTO identity_aliases(series_id,calendar_id,exact_href,first_observed_at_ms,last_observed_at_ms) VALUES(?1,?2,?3,?4,?4)
                 ON CONFLICT(series_id,calendar_id,exact_href) DO UPDATE SET last_observed_at_ms=excluded.last_observed_at_ms",
                params![series_id, calendar.calendar_id, resource.href, stage.observed_at_ms],
            )?;
        }
        transaction.execute(
            "INSERT INTO calendar_sync_state(calendar_id,committed_sync_token,last_complete_at_ms,last_success_at_ms,updated_at_ms) VALUES(?1,?2,?3,?3,?3)
             ON CONFLICT(calendar_id) DO UPDATE SET committed_sync_token=excluded.committed_sync_token,last_complete_at_ms=excluded.last_complete_at_ms,last_success_at_ms=excluded.last_success_at_ms,updated_at_ms=excluded.updated_at_ms",
            params![calendar.calendar_id, calendar.sync_token, stage.observed_at_ms],
        )?;
    }
    transaction.execute(
        "UPDATE remote_resources SET candidate_evidence_valid=0
         WHERE lifecycle='missing_candidate' AND calendar_id IN
           (SELECT calendar_id FROM calendars WHERE account_id=?1 AND available=0)
           AND (?2 IS NULL OR calendar_id=?2 OR calendar_id=?3)",
        params![
            stage.account_id,
            probe_rows.map(|r| &r[0].1),
            probe_rows.map(|r| &r[1].1)
        ],
    )?;
    let mut affected_series = staged_series.clone();
    {
        // Series whose projection is about to be dropped by the retention sweep
        // must be reconciled too, or their pending jobs would outlive the data.
        let mut statement = transaction.prepare(
            "SELECT DISTINCT p.series_id FROM calendar_event_projections p
             JOIN remote_resources r ON r.resource_id=p.resource_id
             JOIN calendars c ON c.calendar_id=r.calendar_id
             WHERE c.account_id=?1 AND r.lifecycle<>'active'
             AND (?2 IS NULL OR r.calendar_id=?2 OR r.calendar_id=?3)",
        )?;
        let rows = statement.query_map(
            params![
                stage.account_id,
                probe_rows.map(|r| &r[0].1),
                probe_rows.map(|r| &r[1].1)
            ],
            |row| row.get::<_, String>(0),
        )?;
        for row in rows {
            affected_series.insert(row?);
        }
    }
    transaction.execute(
        "DELETE FROM calendar_event_projections
         WHERE resource_id IN (
             SELECT r.resource_id FROM remote_resources r
             JOIN calendars c ON c.calendar_id=r.calendar_id
             WHERE c.account_id=?1 AND r.lifecycle<>'active'
             AND (?2 IS NULL OR r.calendar_id=?2 OR r.calendar_id=?3)
         )",
        params![
            stage.account_id,
            probe_rows.map(|r| &r[0].1),
            probe_rows.map(|r| &r[1].1)
        ],
    )?;
    for series_id in &staged_series {
        let preferred = prior_projection_sources
            .get(series_id.as_str())
            .map(String::as_str);
        let selected_resource: Option<String> = transaction.query_row(
            "SELECT r.resource_id FROM remote_resources r
             JOIN event_series s ON s.series_id=r.series_id
             JOIN calendars c ON c.calendar_id=r.calendar_id AND c.account_id=s.account_id
             WHERE r.series_id=?1 AND r.lifecycle='active' AND c.available=1
             ORDER BY CASE WHEN r.resource_id=?2 THEN 0 ELSE 1 END,r.exact_href,r.resource_id LIMIT 1",
            params![series_id, preferred], |row| row.get(0),
        ).optional()?;
        if selected_resource
            .as_ref()
            .is_some_and(|resource| omitted_protected_resources.contains(resource))
        {
            // A staged same-UID alias must not touch the retained source's
            // projection or local last-seen/deletion metadata through rebuilding.
            continue;
        }
        restore_active_alias_projection(
            &transaction,
            series_id,
            preferred,
            stage.observed_at_ms,
            window,
        )?;
    }
    // A first full sync introduces many projections at once. Reconciling each
    // affected series inside this transaction keeps newly imported overdue
    // reminders from being mistaken for previously scheduled work.
    reconcile_calendar_notification_series(
        &transaction,
        &affected_series,
        stage.observed_at_ms,
        NotificationReconcileContext::ScheduleMutation,
    )?;
    transaction.commit()?;
    Ok(())
}

fn account_sync_requests(
    connection: &Connection,
    account_id: &str,
) -> Result<Vec<IncrementalSyncRequest>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT c.calendar_id,s.committed_sync_token
         FROM calendars c JOIN calendar_sync_state s ON s.calendar_id=c.calendar_id
         JOIN accounts a ON a.account_id=c.account_id
         WHERE c.account_id=?1 AND a.enabled=1 AND a.removed_at_ms IS NULL
           AND c.available=1 AND c.selected=1 AND s.committed_sync_token IS NOT NULL
           AND s.committed_sync_token<>''
           AND NOT EXISTS (
               SELECT 1
               FROM calendars pending
               LEFT JOIN calendar_sync_state pending_state ON pending_state.calendar_id=pending.calendar_id
               WHERE pending.account_id=c.account_id AND pending.available=1 AND pending.selected=1
                 AND (pending_state.committed_sync_token IS NULL OR pending_state.committed_sync_token='')
           )
         ORDER BY c.calendar_id",
    )?;
    statement
        .query_map([account_id], |row| {
            Ok(IncrementalSyncRequest {
                calendar_id: row.get(0)?,
                sync_token: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(PersistenceError::Sqlite)
}

fn promote_account_incremental(
    connection: &mut Connection,
    stage: AccountIncrementalStage,
    horizon_now_ms: i64,
) -> Result<(), PersistenceError> {
    if !stage.complete || !stage.authoritative {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    let window = standard_horizon_window(horizon_now_ms)?;
    let transaction = connection.transaction()?;
    let usable = transaction
        .query_row(
            "SELECT enabled=1 AND removed_at_ms IS NULL FROM accounts WHERE account_id=?1",
            [&stage.account_id],
            |row| row.get::<_, bool>(0),
        )
        .optional()?;
    if usable == Some(false) {
        return Err(PersistenceError::AccountDisabled);
    }
    let available_calendars = {
        let mut statement = transaction.prepare(
            "SELECT calendar_id FROM calendars WHERE account_id=?1 AND available=1 AND selected=1",
        )?;
        statement
            .query_map([&stage.account_id], |row| row.get::<_, String>(0))?
            .collect::<Result<HashSet<_>, _>>()?
    };
    if available_calendars.len() != stage.calendars.len()
        || stage
            .calendars
            .iter()
            .any(|calendar| !available_calendars.contains(&calendar.calendar_id))
    {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    for calendar in &stage.calendars {
        let current = transaction
            .query_row(
                "SELECT s.committed_sync_token,c.collection_url
                 FROM calendar_sync_state s JOIN calendars c ON c.calendar_id=s.calendar_id
                 WHERE s.calendar_id=?1 AND c.account_id=?2 AND c.available=1 AND c.selected=1",
                params![calendar.calendar_id, stage.account_id],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let collection_href = current
            .as_ref()
            .map(|(_, href)| href.as_str())
            .ok_or(PersistenceError::UnsafeSyncStage)?;
        if calendar
            .changed_resources
            .iter()
            .any(|resource| !resource_belongs_to_collection(collection_href, &resource.href))
            || calendar
                .removed_hrefs
                .iter()
                .any(|href| !resource_belongs_to_collection(collection_href, href))
        {
            return Err(PersistenceError::UnsafeSyncStage);
        }
        if current.and_then(|(token, _)| token).as_deref()
            != Some(calendar.base_sync_token.as_str())
        {
            return Err(PersistenceError::SyncTokenConflict);
        }
    }

    // Changed and removed resources are reconciled after the whole stage has
    // written its projections, so one sync rewrites each affected series once.
    let mut touched_series = HashSet::new();
    for calendar in &stage.calendars {
        for resource in &calendar.changed_resources {
            let collection_href: String = transaction.query_row(
                "SELECT collection_url FROM calendars WHERE calendar_id=?1 AND account_id=?2",
                params![calendar.calendar_id, stage.account_id],
                |r| r.get(0),
            )?;
            let series_id = transaction
                .query_row(
                    "SELECT series_id FROM event_series WHERE account_id=?1 AND uid=?2",
                    params![stage.account_id, resource.uid],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            transaction.execute(
                "INSERT INTO event_series(series_id,account_id,uid,current_calendar_id,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?5)
                 ON CONFLICT(series_id) DO UPDATE SET current_calendar_id=excluded.current_calendar_id,updated_at_ms=excluded.updated_at_ms",
                params![series_id, stage.account_id, resource.uid, calendar.calendar_id, stage.observed_at_ms],
            )?;
            let existing_resource = transaction
                .query_row(
                    "SELECT resource_id,series_id FROM remote_resources WHERE calendar_id=?1 AND exact_href=?2",
                    params![calendar.calendar_id, resource.href],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if existing_resource
                .as_ref()
                .is_some_and(|(_, existing_series_id)| existing_series_id != &series_id)
            {
                return Err(PersistenceError::IdentityConflict);
            }
            let resource_id = remote_creation::staged_identity(
                &transaction,
                &stage.account_id,
                &calendar.calendar_id,
                &collection_href,
                resource,
                existing_resource.as_ref().map(|(id, _)| id.as_str()),
                &series_id,
                horizon_now_ms,
            )?;
            touched_series.insert(series_id.clone());
            record_remote_change_conflict(
                &transaction,
                &resource_id,
                &resource.etag,
                &resource.raw_ics,
                stage.observed_at_ms,
            )?;
            transaction.execute(
                "INSERT INTO remote_resources(resource_id,calendar_id,series_id,exact_href,etag,content_type,raw_ics,parse_status,lifecycle,last_seen_at_ms,created_at_ms,updated_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'active',?9,?9,?9)
                 ON CONFLICT(resource_id) DO UPDATE SET series_id=excluded.series_id,etag=excluded.etag,content_type=excluded.content_type,raw_ics=excluded.raw_ics,parse_status=excluded.parse_status,lifecycle='active',candidate_evidence_valid=0,last_seen_at_ms=excluded.last_seen_at_ms,remote_deleted_at_ms=NULL,updated_at_ms=excluded.updated_at_ms",
                params![resource_id, calendar.calendar_id, series_id, resource.href, resource.etag, resource.content_type, resource.raw_ics, resource.parse_status.as_str(), stage.observed_at_ms],
            )?;
            transaction.execute(
                "INSERT INTO identity_aliases(series_id,calendar_id,exact_href,first_observed_at_ms,last_observed_at_ms) VALUES(?1,?2,?3,?4,?4)
                 ON CONFLICT(series_id,calendar_id,exact_href) DO UPDATE SET last_observed_at_ms=excluded.last_observed_at_ms",
                params![series_id, calendar.calendar_id, resource.href, stage.observed_at_ms],
            )?;
            let other_active_source = transaction
                .query_row(
                    "SELECT 1 FROM calendar_event_projections p
                     JOIN remote_resources r ON r.resource_id=p.resource_id
                     JOIN calendars c ON c.calendar_id=r.calendar_id
                     WHERE p.series_id=?1 AND p.resource_id<>?2
                       AND r.lifecycle='active' AND c.available=1 LIMIT 1",
                    params![series_id, resource_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if other_active_source {
                continue;
            }
            upsert_calendar_event_projection(
                &transaction,
                ProjectionSource {
                    series_id: &series_id,
                    resource_id: &resource_id,
                    calendar_id: &calendar.calendar_id,
                    etag: &resource.etag,
                    raw_ics: &resource.raw_ics,
                    uid: &resource.uid,
                },
                stage.observed_at_ms,
                window,
            )?;
        }
        for href in &calendar.removed_hrefs {
            if !remote_creation::removal_allowed(&transaction, &calendar.calendar_id, href)? {
                continue;
            }
            let resource = transaction
                .query_row(
                    "SELECT resource_id,series_id FROM remote_resources WHERE calendar_id=?1 AND exact_href=?2 AND lifecycle IN ('active','missing_candidate')",
                    params![calendar.calendar_id, href],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if let Some((resource_id, series_id)) = resource {
                if resource_has_inflight_mutation(&transaction, &resource_id)? {
                    continue;
                }
                touched_series.insert(series_id.clone());
                transaction.execute(
                    "UPDATE remote_resources SET lifecycle='missing_candidate',candidate_evidence_valid=1,updated_at_ms=?2 WHERE resource_id=?1",
                    params![resource_id, stage.observed_at_ms],
                )?;
                let removed_projection = transaction.execute(
                    "DELETE FROM calendar_event_projections WHERE resource_id=?1",
                    [&resource_id],
                )?;
                if removed_projection > 0 {
                    restore_active_alias_projection(
                        &transaction,
                        &series_id,
                        None,
                        stage.observed_at_ms,
                        window,
                    )?;
                }
            }
        }
        transaction.execute(
            "UPDATE calendar_sync_state
             SET committed_sync_token=?2,last_success_at_ms=?3,updated_at_ms=?3
             WHERE calendar_id=?1",
            params![
                calendar.calendar_id,
                calendar.new_sync_token,
                stage.observed_at_ms
            ],
        )?;
    }
    reconcile_calendar_notification_series(
        &transaction,
        &touched_series,
        stage.observed_at_ms,
        NotificationReconcileContext::ScheduleMutation,
    )?;
    transaction.commit()?;
    Ok(())
}

fn conditional_title_update_evidence(
    connection: &Connection,
    target: ConditionalTitleUpdateTarget,
) -> Result<ConditionalTitleUpdateEvidence, PersistenceError> {
    if remote_creation::has_pending_target(connection, target.resource_id())? {
        return Ok(ConditionalTitleUpdateEvidence::unavailable(target));
    }
    type CacheFacts = (
        bool,
        Option<i64>,
        bool,
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
    );
    let facts: Option<CacheFacts> = connection
        .query_row(
            "SELECT a.enabled,a.removed_at_ms,c.available,c.privileges,
                    r.lifecycle,r.parse_status,r.etag,r.raw_ics,s.uid
             FROM remote_resources r
             JOIN calendars c ON c.calendar_id=r.calendar_id
             JOIN accounts a ON a.account_id=c.account_id
             JOIN event_series s ON s.series_id=r.series_id AND s.account_id=a.account_id
             WHERE a.account_id=?1 AND c.calendar_id=?2 AND r.resource_id=?3",
            params![
                target.account_id(),
                target.calendar_id(),
                target.resource_id()
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .optional()?;
    let Some((
        enabled,
        removed_at_ms,
        available,
        privileges,
        lifecycle,
        parse_status,
        etag,
        raw,
        series_uid,
    )) = facts
    else {
        return Ok(ConditionalTitleUpdateEvidence::unavailable(target));
    };
    let writable = collection_content_writable(&privileges);
    Ok(ConditionalTitleUpdateEvidence::from_committed_cache(
        target,
        CommittedConditionalTitleUpdateFacts {
            account_usable: enabled && removed_at_ms.is_none(),
            collection_available: available,
            collection_writable: writable,
            resource_active: lifecycle == "active",
            parse_safe: parse_status == "parsed"
                && single_vevent_uid(&raw).is_ok_and(|uid| uid == series_uid),
            etag_usable: strong_mutation_etag(&etag),
            raw_ics: &raw,
        },
    ))
}

fn queue_remote_title_update(
    connection: &mut Connection,
    input: QueueRemoteTitleUpdate,
    now_ms: i64,
) -> Result<RemoteMutationSummary, PersistenceError> {
    let input = input.validate()?;
    let intent_json = serde_json::json!({ "title": input.title }).to_string();
    let transaction = connection.transaction()?;
    if transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_create_operations WHERE idempotency_key=?1)",
        [&input.idempotency_key],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::IdempotencyConflict);
    }

    if transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_delete_operations WHERE idempotency_key=?1)",
        [&input.idempotency_key],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::IdempotencyConflict);
    }

    let existing = transaction
        .query_row(
            "SELECT operation_id,idempotency_key,account_id,resource_id,state,created_at_ms,updated_at_ms,intent_json,operation_kind
             FROM outbox_operations WHERE idempotency_key=?1",
            [&input.idempotency_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .optional()?;
    if let Some((
        operation_id,
        idempotency_key,
        account_id,
        resource_id,
        state,
        created_at_ms,
        updated_at_ms,
        stored_intent,
        operation_kind,
    )) = existing
    {
        if resource_id != input.resource_id
            || stored_intent != intent_json
            || operation_kind != "update_title"
        {
            return Err(PersistenceError::IdempotencyConflict);
        }
        return Ok(RemoteMutationSummary {
            operation_id,
            idempotency_key,
            account_id,
            resource_id,
            state: RemoteMutationState::from_storage(&state)?,
            created_at_ms,
            updated_at_ms,
        });
    }

    type MutationBase = (
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
        String,
        String,
        bool,
        Option<i64>,
        bool,
        String,
        String,
        String,
    );
    let base: MutationBase = transaction
        .query_row(
            "SELECT a.account_id,c.calendar_id,r.series_id,r.etag,r.raw_ics,r.parse_status,
                    r.lifecycle,c.privileges,a.enabled,a.removed_at_ms,c.available,
                    c.collection_url,r.exact_href,s.uid
             FROM remote_resources r
             JOIN calendars c ON c.calendar_id=r.calendar_id
             JOIN accounts a ON a.account_id=c.account_id
             JOIN event_series s ON s.series_id=r.series_id AND s.account_id=a.account_id
             WHERE r.resource_id=?1",
            [&input.resource_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    let (
        account_id,
        calendar_id,
        series_id,
        base_etag,
        base_raw_ics,
        parse_status,
        lifecycle,
        privileges,
        enabled,
        removed_at_ms,
        available,
        collection_href,
        exact_href,
        series_uid,
    ) = base;
    if !enabled || removed_at_ms.is_some() || !available {
        return Err(PersistenceError::AccountDisabled);
    }
    if parse_status != "parsed" || lifecycle != "active" {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let can_write = collection_content_writable(&privileges);
    if !can_write
        || !resource_belongs_to_collection(&collection_href, &exact_href)
        || !strong_mutation_etag(&base_etag)
        || contains_invitation_property(&base_raw_ics)
        || !single_vevent_uid(&base_raw_ics).is_ok_and(|uid| uid == series_uid)
        || patch_single_vevent_title(&base_raw_ics, &input.title).is_err()
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let pending = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM outbox_operations WHERE resource_id=?1 AND state='pending'
            UNION ALL
            SELECT 1 FROM remote_delete_operations WHERE resource_id=?1 AND state='pending'
         )",
        [&input.resource_id],
        |row| row.get::<_, bool>(0),
    )?;
    if pending || remote_creation::has_pending_target(&transaction, &input.resource_id)? {
        return Err(PersistenceError::PendingMutationExists);
    }

    let operation_id = Uuid::new_v4().to_string();
    transaction.execute(
        "INSERT INTO outbox_operations(
            operation_id,idempotency_key,account_id,calendar_id,resource_id,series_id,
            operation_kind,base_etag,base_raw_ics,intent_json,state,created_at_ms,updated_at_ms
         ) VALUES(?1,?2,?3,?4,?5,?6,'update_title',?7,?8,?9,'pending',?10,?10)",
        params![
            operation_id,
            input.idempotency_key,
            account_id,
            calendar_id,
            input.resource_id,
            series_id,
            base_etag,
            base_raw_ics,
            intent_json,
            now_ms
        ],
    )?;
    transaction.commit()?;
    Ok(RemoteMutationSummary {
        operation_id,
        idempotency_key: input.idempotency_key,
        account_id,
        resource_id: input.resource_id,
        state: RemoteMutationState::Pending,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    })
}

fn queue_remote_event_delete(
    connection: &mut Connection,
    input: QueueRemoteEventDelete,
    now_ms: i64,
) -> Result<RemoteMutationSummary, PersistenceError> {
    queue_remote_event_delete_scoped(connection, input, now_ms, false)
}

fn queue_remote_event_delete_scoped(
    connection: &mut Connection,
    input: QueueRemoteEventDelete,
    now_ms: i64,
    supported_crud: bool,
) -> Result<RemoteMutationSummary, PersistenceError> {
    let input = input.validate()?;
    let intent_json = serde_json::json!({ "delete": true }).to_string();
    let transaction = connection.transaction()?;
    if transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_create_operations WHERE idempotency_key=?1)",
        [&input.idempotency_key],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::IdempotencyConflict);
    }

    let existing = transaction
        .query_row(
            "SELECT operation_id,idempotency_key,account_id,resource_id,state,created_at_ms,updated_at_ms,intent_json
             FROM remote_delete_operations WHERE idempotency_key=?1",
            [&input.idempotency_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()?;
    if let Some((
        operation_id,
        idempotency_key,
        account_id,
        resource_id,
        state,
        created_at_ms,
        updated_at_ms,
        stored_intent,
    )) = existing
    {
        if resource_id != input.resource_id || stored_intent != intent_json {
            return Err(PersistenceError::IdempotencyConflict);
        }
        return Ok(RemoteMutationSummary {
            operation_id,
            idempotency_key,
            account_id,
            resource_id,
            state: RemoteMutationState::from_storage(&state)?,
            created_at_ms,
            updated_at_ms,
        });
    }

    if transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM outbox_operations WHERE idempotency_key=?1)",
        [&input.idempotency_key],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::IdempotencyConflict);
    }

    type DeleteBase = (
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
        String,
        String,
        bool,
        Option<i64>,
        bool,
        String,
        String,
        String,
    );
    let base: DeleteBase = transaction
        .query_row(
            "SELECT a.account_id,c.calendar_id,r.series_id,r.etag,r.raw_ics,r.parse_status,
                    r.lifecycle,c.privileges,a.enabled,a.removed_at_ms,c.available,
                    c.collection_url,r.exact_href,s.uid
             FROM remote_resources r
             JOIN calendars c ON c.calendar_id=r.calendar_id
             JOIN accounts a ON a.account_id=c.account_id
             JOIN event_series s ON s.series_id=r.series_id AND s.account_id=a.account_id
             WHERE r.resource_id=?1",
            [&input.resource_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    let (
        account_id,
        calendar_id,
        series_id,
        base_etag,
        base_raw_ics,
        parse_status,
        lifecycle,
        privileges,
        enabled,
        removed_at_ms,
        available,
        collection_href,
        exact_href,
        series_uid,
    ) = base;
    if supported_crud {
        let enabled: bool = transaction.query_row(
            "SELECT provider_kind='caldav' AND remote_editing_enabled=1 AND credential_configured=1 AND region_profile='icloud-cn' FROM accounts WHERE account_id=?1",
            [&account_id], |row| row.get(0),
        )?;
        if !enabled
            || (parse_status != "parsed"
                && crate::domain::remote_event_update::validate_event_base_v3(
                    &base_raw_ics,
                    &series_uid,
                    false,
                )
                .is_err())
        {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
    }
    if !enabled || removed_at_ms.is_some() || !available {
        return Err(PersistenceError::AccountDisabled);
    }
    if lifecycle != "active" || (!supported_crud && parse_status != "parsed") {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    if !collection_content_writable(&privileges)
        || !resource_belongs_to_collection(&collection_href, &exact_href)
        || !strong_mutation_etag(&base_etag)
        || contains_invitation_property(&base_raw_ics)
        || contains_recurrence_sensitive_property(&base_raw_ics)
        || !single_vevent_uid(&base_raw_ics).is_ok_and(|uid| uid == series_uid)
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let pending = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM outbox_operations WHERE resource_id=?1 AND state='pending'
            UNION ALL
            SELECT 1 FROM remote_delete_operations WHERE resource_id=?1 AND state='pending'
         )",
        [&input.resource_id],
        |row| row.get::<_, bool>(0),
    )?;
    if pending || remote_creation::has_pending_target(&transaction, &input.resource_id)? {
        return Err(PersistenceError::PendingMutationExists);
    }
    if supported_crud && parse_status != "parsed" {
        transaction.execute(
            "UPDATE remote_resources SET parse_status='parsed' WHERE resource_id=?1",
            [&input.resource_id],
        )?;
    }

    let operation_id = Uuid::new_v4().to_string();
    transaction.execute(
        "INSERT INTO remote_delete_operations(
            operation_id,idempotency_key,account_id,calendar_id,resource_id,series_id,
            base_etag,base_raw_ics,intent_json,state,created_at_ms,updated_at_ms
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending',?10,?10)",
        params![
            operation_id,
            input.idempotency_key,
            account_id,
            calendar_id,
            input.resource_id,
            series_id,
            base_etag,
            base_raw_ics,
            intent_json,
            now_ms
        ],
    )?;
    transaction.commit()?;
    Ok(RemoteMutationSummary {
        operation_id,
        idempotency_key: input.idempotency_key,
        account_id,
        resource_id: input.resource_id,
        state: RemoteMutationState::Pending,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    })
}

fn list_remote_mutations(
    connection: &Connection,
) -> Result<Vec<RemoteMutationSummary>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT operation_id,idempotency_key,account_id,resource_id,state,created_at_ms,updated_at_ms
         FROM outbox_operations
         UNION ALL
         SELECT operation_id,idempotency_key,account_id,resource_id,state,created_at_ms,updated_at_ms
         FROM remote_delete_operations
         ORDER BY created_at_ms,operation_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    rows.map(|row| {
        let (
            operation_id,
            idempotency_key,
            account_id,
            resource_id,
            state,
            created_at_ms,
            updated_at_ms,
        ) = row?;
        Ok(RemoteMutationSummary {
            operation_id,
            idempotency_key,
            account_id,
            resource_id,
            state: RemoteMutationState::from_storage(&state)?,
            created_at_ms,
            updated_at_ms,
        })
    })
    .collect()
}

fn list_remote_conflicts(
    connection: &Connection,
) -> Result<Vec<RemoteConflictSummary>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT c.operation_id AS operation_id,o.account_id,o.resource_id,c.resolution_state,
                c.retry_operation_id,c.created_at_ms AS detected_at_ms,c.resolved_at_ms
         FROM conflicts c JOIN outbox_operations o ON o.operation_id=c.operation_id
         WHERE o.operation_kind='update_title'
         UNION ALL
         SELECT d.operation_id AS operation_id,d.account_id,d.resource_id,d.resolution_state,
                d.retry_operation_id,d.conflict_at_ms AS detected_at_ms,d.resolved_at_ms
         FROM remote_delete_operations d
         WHERE d.resolution_state IS NOT NULL AND d.conflict_at_ms IS NOT NULL
         ORDER BY detected_at_ms,operation_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<i64>>(6)?,
        ))
    })?;
    rows.map(|row| {
        let (
            operation_id,
            account_id,
            resource_id,
            resolution_state,
            retry_operation_id,
            detected_at_ms,
            resolved_at_ms,
        ) = row?;
        Ok(RemoteConflictSummary {
            operation_id,
            account_id,
            resource_id,
            resolution_state: ConflictResolutionState::from_storage(&resolution_state)?,
            retry_operation_id,
            detected_at_ms,
            resolved_at_ms,
        })
    })
    .collect()
}

pub(crate) const REMOTE_MUTATION_CLAIM_LEASE_MS: i64 = 5 * 60 * 1_000;
const REMOTE_MUTATION_CLAIM_SCAN_LIMIT: i64 = 64;

fn claim_remote_deletion(
    connection: &mut Connection,
    account_id: &str,
    now_ms: i64,
) -> Result<Option<RemoteDeletionClaim>, PersistenceError> {
    claim_remote_deletion_scoped(connection, account_id, now_ms, None)
}
fn claim_remote_deletion_scoped(
    connection: &mut Connection,
    account_id: &str,
    now_ms: i64,
    scope: Option<&scoped_event::ScopedEventSnapshot>,
) -> Result<Option<RemoteDeletionClaim>, PersistenceError> {
    admitted_now_ms(now_ms)?;
    Uuid::parse_str(account_id)
        .map_err(|_| crate::domain::remote_mutation::RemoteMutationError::InvalidIdentity)?;
    let transaction = connection.transaction()?;
    let stale_before = now_ms.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS);
    transaction.execute(
        "UPDATE remote_delete_operations SET claim_token=NULL,claimed_at_ms=NULL,
             execution_phase='verify_first',last_outcome='lease_expired',updated_at_ms=?2
         WHERE state='pending' AND claim_token IS NOT NULL AND claimed_at_ms < ?1
           AND (?3 IS NULL OR (operation_id=?3 AND calendar_id=?4 AND account_id=?5))",
        params![
            stale_before,
            now_ms,
            scoped_event::scope_ids(scope).0,
            scoped_event::scope_ids(scope).1,
            account_id
        ],
    )?;

    type Candidate = (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
        Vec<u8>,
        String,
        String,
        String,
        String,
        i64,
    );
    let mut statement = transaction.prepare(
        "SELECT d.operation_id,d.account_id,d.calendar_id,c.collection_url,d.resource_id,
                r.exact_href,d.base_etag,d.base_raw_ics,r.etag,r.raw_ics,r.parse_status,
                c.privileges,s.uid,d.execution_phase,d.attempt_count
         FROM remote_delete_operations d
         JOIN accounts a ON a.account_id=d.account_id
         JOIN calendars c ON c.calendar_id=d.calendar_id AND c.account_id=d.account_id
         JOIN remote_resources r ON r.resource_id=d.resource_id
             AND r.calendar_id=d.calendar_id AND r.series_id=d.series_id
         JOIN event_series s ON s.series_id=d.series_id AND s.account_id=d.account_id
         WHERE d.account_id=?1 AND d.state='pending' AND d.claim_token IS NULL
           AND a.enabled=1 AND a.removed_at_ms IS NULL AND c.available=1
           AND r.lifecycle='active' AND (?3 IS NULL OR (d.operation_id=?3 AND d.calendar_id=?4))
         ORDER BY d.created_at_ms,d.operation_id LIMIT ?2",
    )?;
    let rows = statement
        .query_map(
            params![
                account_id,
                REMOTE_MUTATION_CLAIM_SCAN_LIMIT,
                scoped_event::scope_ids(scope).0,
                scoped_event::scope_ids(scope).1
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let Some((
        operation_id,
        account_id,
        calendar_id,
        collection_href,
        resource_id,
        exact_href,
        base_etag,
        base_raw_ics,
        _current_etag,
        _current_raw_ics,
        _parse_status,
        _privileges,
        _series_uid,
        phase,
        attempts,
    )) = rows.into_iter().find(|row: &Candidate| {
        let phase = RemoteMutationExecutionPhase::from_storage(&row.13).ok();
        strong_mutation_etag(&row.6)
            && row.10 == "parsed"
            && collection_content_writable(&row.11)
            && resource_belongs_to_collection(&row.3, &row.5)
            && !contains_invitation_property(&row.7)
            && !contains_recurrence_sensitive_property(&row.7)
            && single_vevent_uid(&row.7).is_ok_and(|uid| uid == row.12)
            && phase.is_some()
            && (phase == Some(RemoteMutationExecutionPhase::VerifyFirst)
                || (row.8 == row.6 && row.9 == row.7))
            && row
                .14
                .checked_add(1)
                .and_then(|v| u32::try_from(v).ok())
                .is_some()
    })
    else {
        transaction.commit()?;
        return Ok(None);
    };
    let phase = RemoteMutationExecutionPhase::from_storage(&phase)?;
    let attempt_count = u32::try_from(
        attempts
            .checked_add(1)
            .ok_or(PersistenceError::OutboxClaimLost)?,
    )
    .map_err(|_| PersistenceError::OutboxClaimLost)?;
    let claim_token = Uuid::new_v4().to_string();
    if transaction.execute(
        "UPDATE remote_delete_operations SET claim_token=?2,claimed_at_ms=?3,
             attempt_count=attempt_count+1,updated_at_ms=?3
         WHERE operation_id=?1 AND state='pending' AND claim_token IS NULL",
        params![operation_id, claim_token, now_ms],
    )? != 1
    {
        return Err(PersistenceError::OutboxClaimLost);
    }
    transaction.commit()?;
    Ok(Some(RemoteDeletionClaim {
        operation_id,
        claim_token,
        account_id,
        calendar_id,
        resource_id,
        collection_href,
        exact_href,
        base_etag,
        base_raw_ics,
        phase,
        attempt_count,
    }))
}

fn remote_deletion_send_allowed(
    connection: &Connection,
    claim_token: &str,
    now_ms: i64,
) -> Result<bool, PersistenceError> {
    remote_deletion_transport_allowed(connection, claim_token, now_ms, true)
}

fn remote_deletion_read_allowed(
    connection: &Connection,
    claim_token: &str,
    now_ms: i64,
) -> Result<bool, PersistenceError> {
    remote_deletion_transport_allowed(connection, claim_token, now_ms, false)
}

fn remote_deletion_transport_allowed(
    connection: &Connection,
    claim_token: &str,
    now_ms: i64,
    require_ready: bool,
) -> Result<bool, PersistenceError> {
    admitted_now_ms(now_ms)?;
    let evidence = connection
        .query_row(
            "SELECT c.collection_url,r.exact_href,c.privileges
         FROM remote_delete_operations d
         JOIN accounts a ON a.account_id=d.account_id
         JOIN calendars c ON c.calendar_id=d.calendar_id AND c.account_id=d.account_id
         JOIN remote_resources r ON r.resource_id=d.resource_id
             AND r.calendar_id=d.calendar_id AND r.series_id=d.series_id
         WHERE d.claim_token=?1 AND d.state='pending' AND (?4=0 OR d.execution_phase='ready')
           AND d.claimed_at_ms BETWEEN ?2 AND ?3 AND a.enabled=1
           AND a.removed_at_ms IS NULL AND c.available=1 AND r.lifecycle='active'
           AND r.parse_status='parsed' AND r.etag=d.base_etag AND r.raw_ics=d.base_raw_ics",
            params![
                claim_token,
                now_ms.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),
                now_ms,
                require_ready,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(evidence.is_some_and(|(collection, href, privileges)| {
        collection_content_writable(&privileges)
            && resource_belongs_to_collection(&collection, &href)
    }))
}

fn defer_remote_deletion(
    connection: &Connection,
    claim_token: &str,
    ambiguous: bool,
    outcome: &str,
    now_ms: i64,
) -> Result<(), PersistenceError> {
    admitted_now_ms(now_ms)?;
    if !matches!(
        outcome,
        "transient" | "ambiguous" | "unauthorized" | "unsafe"
    ) {
        return Err(PersistenceError::OutboxClaimLost);
    }
    let changed = connection.execute(
        "UPDATE remote_delete_operations SET claim_token=NULL,claimed_at_ms=NULL,
             execution_phase=CASE WHEN execution_phase='verify_first' OR ?2
                 THEN 'verify_first' ELSE 'ready' END,
             last_outcome=?3,updated_at_ms=?4
         WHERE claim_token=?1 AND state='pending' AND claimed_at_ms BETWEEN ?5 AND ?4",
        params![
            claim_token,
            ambiguous,
            outcome,
            now_ms,
            now_ms.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS)
        ],
    )?;
    (changed == 1)
        .then_some(())
        .ok_or(PersistenceError::OutboxClaimLost)
}

fn finish_remote_deletion(
    connection: &mut Connection,
    claim_token: &str,
    evidence: RemoteDeletionReadBack,
    now_ms: i64,
    current: &dyn Fn(&str, &str, &str, &str) -> bool,
) -> Result<(String, String, bool), PersistenceError> {
    admitted_now_ms(now_ms)?;
    let transaction = connection.transaction()?;
    type Stored = (
        String,
        String,
        Vec<u8>,
        String,
        String,
        Vec<u8>,
        String,
        String,
        String,
    );
    let stored: Stored = transaction
        .query_row(
            "SELECT d.operation_id,d.base_etag,d.base_raw_ics,s.uid,r.etag,r.raw_ics,
                d.account_id,c.collection_url,r.exact_href
         FROM remote_delete_operations d
         JOIN remote_resources r ON r.resource_id=d.resource_id
             AND r.calendar_id=d.calendar_id AND r.series_id=d.series_id
         JOIN calendars c ON c.calendar_id=d.calendar_id AND c.account_id=d.account_id
         JOIN event_series s ON s.series_id=d.series_id AND s.account_id=d.account_id
         WHERE d.claim_token=?1 AND d.state='pending'
           AND d.claimed_at_ms BETWEEN ?2 AND ?3",
            params![
                claim_token,
                now_ms.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),
                now_ms
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::OutboxClaimLost)?;
    let (resource_id, series_id, calendar_id): (String, String, String) = transaction.query_row(
        "SELECT resource_id,series_id,calendar_id FROM remote_delete_operations WHERE operation_id=?1 AND claim_token=?2 AND state='pending'",
        params![stored.0, claim_token],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if !current(&stored.0, &stored.6, &calendar_id, &resource_id) {
        return Err(PersistenceError::StaleSyncCapture);
    }
    if !strong_mutation_etag(&stored.1)
        || stored.4 != stored.1
        || stored.5 != stored.2
        || !resource_belongs_to_collection(&stored.7, &stored.8)
        || !single_vevent_uid(&stored.2).is_ok_and(|uid| uid == stored.3)
        || contains_invitation_property(&stored.2)
        || contains_recurrence_sensitive_property(&stored.2)
    {
        return Err(PersistenceError::RemoteReadBackMismatch);
    }
    let mut jobs_changed = false;
    match evidence {
        RemoteDeletionReadBack::Absent { status: 404 | 410 } => {
            let planned_before =
                remote_update_fields::event_planned_fires(&transaction, &series_id)?;
            let changed = transaction.execute(
                "UPDATE remote_resources SET lifecycle='remote_deleted',remote_deleted_at_ms=?2,updated_at_ms=?2 WHERE resource_id=?1 AND lifecycle='active' AND calendar_id=?3 AND series_id=?4",
                params![resource_id, now_ms, calendar_id, series_id],
            )?;
            if changed != 1 {
                return Err(PersistenceError::RemoteReadBackMismatch);
            }
            let alias: Option<(String, String, String, Vec<u8>, String)> = transaction.query_row(
                "SELECT r.resource_id,r.calendar_id,r.etag,r.raw_ics,s.uid FROM remote_resources r JOIN event_series s ON s.series_id=r.series_id WHERE r.series_id=?1 AND r.resource_id<>?2 AND r.lifecycle='active' ORDER BY CASE WHEN EXISTS(SELECT 1 FROM calendar_event_projections p WHERE p.series_id=r.series_id AND p.resource_id=r.resource_id) THEN 0 ELSE 1 END,r.exact_href,r.resource_id LIMIT 1",
                params![series_id, resource_id],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
            ).optional()?;
            if let Some((alias_id, alias_calendar, alias_etag, alias_raw, alias_uid)) = alias {
                upsert_calendar_event_projection(
                    &transaction,
                    ProjectionSource {
                        series_id: &series_id,
                        resource_id: &alias_id,
                        calendar_id: &alias_calendar,
                        etag: &alias_etag,
                        raw_ics: &alias_raw,
                        uid: &alias_uid,
                    },
                    now_ms,
                    standard_horizon_window(now_ms)?,
                )?;
            } else {
                transaction.execute(
                    "DELETE FROM calendar_event_projections WHERE series_id=?1",
                    [&series_id],
                )?;
                let unresolved: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM remote_resources WHERE series_id=?1 AND lifecycle IN ('active','missing_candidate'))",
                    [&series_id], |row| row.get(0),
                )?;
                if !unresolved {
                    transaction.execute("UPDATE work_item_state SET remote_deleted_at_ms=?2,last_seen_at_ms=?2 WHERE source_kind='calendar_occurrence' AND calendar_series_id=?1", params![series_id, now_ms])?;
                }
            }
            // Deletion removes projections but must not spend the catch-up
            // opportunity of a surviving alias projection: scoped reconciliation
            // keeps previously planned instants and only cancels the vanished ones.
            reconcile_calendar_notification_jobs(
                &transaction,
                &series_id,
                now_ms,
                NotificationReconcileContext::ScheduleMutation,
            )?;
            jobs_changed = planned_before
                != remote_update_fields::event_planned_fires(&transaction, &series_id)?;
            transaction.execute(
                "UPDATE remote_delete_operations SET state='applied',claim_token=NULL,
                     claimed_at_ms=NULL,last_outcome='absent',updated_at_ms=?2
                 WHERE operation_id=?1 AND claim_token=?3 AND state='pending'",
                params![stored.0, now_ms, claim_token],
            )?;
        }
        RemoteDeletionReadBack::Absent { .. } => {
            return Err(PersistenceError::RemoteReadBackMismatch);
        }
        RemoteDeletionReadBack::Retained(remote) => {
            validate_remote_read_back(&remote)?;
            let remote_uid = single_vevent_uid(&remote.raw_ics)
                .map_err(|_| PersistenceError::RemoteReadBackMismatch)?;
            if remote_uid != stored.3 || remote.etag == stored.1 && remote.raw_ics != stored.2 {
                return Err(PersistenceError::RemoteReadBackMismatch);
            }
            if remote.etag == stored.1 {
                transaction.execute(
                    "UPDATE remote_delete_operations SET claim_token=NULL,claimed_at_ms=NULL,
                         execution_phase='ready',last_outcome='retained_base',updated_at_ms=?2
                     WHERE operation_id=?1 AND claim_token=?3 AND state='pending'",
                    params![stored.0, now_ms, claim_token],
                )?;
            } else {
                transaction.execute(
                    "UPDATE remote_delete_operations SET state='conflict',claim_token=NULL,
                         claimed_at_ms=NULL,last_outcome='retained_changed',remote_etag=?2,
                         remote_raw_ics=?3,resolution_state='unresolved',conflict_at_ms=?4,
                         updated_at_ms=?4
                     WHERE operation_id=?1 AND claim_token=?5 AND state='pending'",
                    params![stored.0, remote.etag, remote.raw_ics, now_ms, claim_token],
                )?;
            }
        }
    }
    if !current(&stored.0, &stored.6, &calendar_id, &resource_id) {
        return Err(PersistenceError::StaleSyncCapture);
    }
    transaction.commit()?;
    Ok((stored.6, stored.0, jobs_changed))
}

fn collection_content_writable(privileges: &str) -> bool {
    privileges
        .split(',')
        .map(str::trim)
        .any(|privilege| matches!(privilege, "write" | "write-content"))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredTitleIntent {
    title: String,
}

fn parse_stored_title_intent(json: &str) -> Result<StoredTitleIntent, PersistenceError> {
    let intent = serde_json::from_str::<StoredTitleIntent>(json)
        .map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    validate_title(&intent.title).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    if intent.title.trim() != intent.title {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    Ok(intent)
}

fn claim_remote_mutation(
    connection: &mut Connection,
    account_filter: Option<&str>,
    now_ms: i64,
) -> Result<Option<RemoteMutationClaim>, PersistenceError> {
    admitted_now_ms(now_ms)?;
    if let Some(account_id) = account_filter {
        validate_account_id(account_id)?;
    }
    let transaction = connection.transaction()?;
    let stale_before = now_ms.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS);
    transaction.execute(
        "UPDATE outbox_operations SET claim_token=NULL,claimed_at_ms=NULL,
            execution_phase='verify_first',last_outcome='ambiguous',updated_at_ms=?1
         WHERE operation_kind='update_title' AND state='pending' AND claim_token IS NOT NULL AND claimed_at_ms<?2",
        params![now_ms, stale_before],
    )?;
    // Inspect routing and bounded intent before loading retained ICS bytes.
    // An unsafe head row cannot lease or starve a later safe row in this window.
    let candidates = {
        let mut statement = transaction.prepare(
            "SELECT o.operation_id,c.collection_url,r.exact_href,o.intent_json,
                    o.execution_phase,o.attempt_count,o.base_etag,r.etag,
                    r.parse_status,c.privileges
             FROM outbox_operations o
             JOIN remote_resources r ON r.resource_id=o.resource_id
                 AND r.calendar_id=o.calendar_id AND r.series_id=o.series_id
             JOIN calendars c ON c.calendar_id=o.calendar_id AND c.account_id=o.account_id
             JOIN event_series s ON s.series_id=o.series_id AND s.account_id=o.account_id
             JOIN accounts a ON a.account_id=o.account_id
             WHERE o.operation_kind='update_title' AND o.state='pending' AND o.claim_token IS NULL
                AND (?1 IS NULL OR o.account_id=?1) AND a.enabled=1
                AND a.removed_at_ms IS NULL AND c.available=1 AND r.lifecycle='active'
                AND length(c.collection_url)>0 AND length(r.exact_href)>0
             ORDER BY o.created_at_ms,o.operation_id LIMIT ?2",
        )?;
        statement
            .query_map(
                params![account_filter, REMOTE_MUTATION_CLAIM_SCAN_LIMIT],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?
    };
    let Some((selected_id, _, _, title, phase, attempt_count)) = candidates
        .iter()
        .filter_map(
            |(
                operation_id,
                collection_href,
                exact_href,
                intent_json,
                phase,
                attempts,
                base_etag,
                current_etag,
                parse_status,
                privileges,
            )| {
                if !strong_mutation_etag(base_etag)
                    || parse_status != "parsed"
                    || !collection_content_writable(privileges)
                {
                    return None;
                }
                let intent = parse_stored_title_intent(intent_json).ok()?;
                let phase = RemoteMutationExecutionPhase::from_storage(phase).ok()?;
                if phase == RemoteMutationExecutionPhase::Ready && current_etag != base_etag {
                    return None;
                }
                let attempt_count = u32::try_from(attempts.checked_add(1)?).ok()?;
                resource_belongs_to_collection(collection_href, exact_href).then_some((
                    operation_id,
                    collection_href,
                    exact_href,
                    intent.title,
                    phase,
                    attempt_count,
                ))
            },
        )
        .next()
    else {
        transaction.commit()?;
        return if candidates.is_empty() {
            Ok(None)
        } else {
            Err(PersistenceError::RemoteMutationReadOnly)
        };
    };
    type StoredClaim = (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
        String,
        String,
        String,
        Vec<u8>,
    );
    let stored: Option<StoredClaim> = transaction
        .query_row(
            "SELECT o.operation_id,o.account_id,o.calendar_id,c.collection_url,o.resource_id,
                    r.exact_href,o.base_etag,o.base_raw_ics,r.etag,
                    r.parse_status,c.privileges,s.uid,r.raw_ics
             FROM outbox_operations o
             JOIN remote_resources r ON r.resource_id=o.resource_id
                 AND r.calendar_id=o.calendar_id AND r.series_id=o.series_id
             JOIN calendars c ON c.calendar_id=o.calendar_id AND c.account_id=o.account_id
             JOIN event_series s ON s.series_id=o.series_id AND s.account_id=o.account_id
             JOIN accounts a ON a.account_id=o.account_id
             WHERE o.operation_kind='update_title' AND o.state='pending' AND o.claim_token IS NULL
                AND o.operation_id=?1 AND a.enabled=1
                AND a.removed_at_ms IS NULL AND c.available=1 AND r.lifecycle='active'
                AND length(c.collection_url)>0 AND length(r.exact_href)>0
             LIMIT 1",
            [selected_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ))
            },
        )
        .optional()?;
    let Some((
        operation_id,
        account_id,
        calendar_id,
        collection_href,
        resource_id,
        exact_href,
        base_etag,
        base_raw_ics,
        current_etag,
        parse_status,
        privileges,
        series_uid,
        current_raw_ics,
    )) = stored
    else {
        transaction.commit()?;
        return Ok(None);
    };
    if !resource_belongs_to_collection(&collection_href, &exact_href)
        || !strong_mutation_etag(&base_etag)
        || (phase == RemoteMutationExecutionPhase::Ready
            && (current_etag != base_etag || current_raw_ics != base_raw_ics))
        || parse_status != "parsed"
        || !collection_content_writable(&privileges)
        || contains_invitation_property(&base_raw_ics)
        || !single_vevent_uid(&base_raw_ics).is_ok_and(|uid| uid == series_uid)
        || patch_single_vevent_title(&base_raw_ics, &title).is_err()
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let claim_token = Uuid::new_v4().to_string();
    let changed = transaction.execute(
        "UPDATE outbox_operations SET claim_token=?2,claimed_at_ms=?3,
            attempt_count=attempt_count+1,updated_at_ms=?3
         WHERE operation_id=?1 AND state='pending' AND claim_token IS NULL",
        params![operation_id, claim_token, now_ms],
    )?;
    if changed != 1 {
        return Err(PersistenceError::OutboxClaimLost);
    }
    transaction.commit()?;
    Ok(Some(RemoteMutationClaim {
        operation_id,
        claim_token,
        account_id,
        calendar_id,
        resource_id,
        collection_href,
        exact_href,
        base_etag,
        base_raw_ics,
        title,
        phase,
        attempt_count,
    }))
}

fn defer_remote_mutation(
    connection: &Connection,
    claim_token: &str,
    ambiguous: bool,
    outcome: &str,
    now_ms: i64,
) -> Result<(), PersistenceError> {
    admitted_now_ms(now_ms)?;
    if !matches!(
        outcome,
        "transient" | "ambiguous" | "unauthorized" | "unsafe"
    ) {
        return Err(PersistenceError::OutboxClaimLost);
    }
    let changed = connection.execute(
        "UPDATE outbox_operations SET claim_token=NULL,claimed_at_ms=NULL,
            execution_phase=CASE WHEN execution_phase='verify_first' OR ?2
                THEN 'verify_first' ELSE 'ready' END,
            last_outcome=?3,updated_at_ms=?4 WHERE operation_kind='update_title' AND claim_token=?1 AND state='pending'
            AND claimed_at_ms BETWEEN ?5 AND ?4",
        params![
            claim_token,
            ambiguous,
            outcome,
            now_ms,
            now_ms.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS)
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(PersistenceError::OutboxClaimLost)
    }
}

// Current actor snapshot only; callers must still gate ownership and cancellation.
fn remote_mutation_send_allowed(
    connection: &Connection,
    claim_token: &str,
    now_ms: i64,
) -> Result<bool, PersistenceError> {
    let evidence = connection.query_row(
        "SELECT c.collection_url,r.exact_href,c.privileges
         FROM outbox_operations o
         JOIN accounts a ON a.account_id=o.account_id
         JOIN calendars c ON c.calendar_id=o.calendar_id AND c.account_id=o.account_id
         JOIN remote_resources r ON r.resource_id=o.resource_id AND r.calendar_id=o.calendar_id AND r.series_id=o.series_id
         WHERE o.operation_kind='update_title' AND o.claim_token=?1 AND o.state='pending'
           AND o.claimed_at_ms BETWEEN ?2 AND ?3
           AND a.enabled=1 AND a.removed_at_ms IS NULL AND c.available=1
           AND r.lifecycle='active' AND r.parse_status='parsed'
           AND r.etag=o.base_etag AND r.raw_ics=o.base_raw_ics",
        params![claim_token,now_ms.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),now_ms],
        |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)),
    ).optional()?;
    Ok(evidence.is_some_and(|(collection, href, privileges)| {
        collection_content_writable(&privileges)
            && resource_belongs_to_collection(&collection, &href)
    }))
}

fn conflict_claimed_remote_mutation(
    connection: &mut Connection,
    claim_token: &str,
    remote: RemoteReadBack,
    now_ms: i64,
) -> Result<String, PersistenceError> {
    validate_remote_read_back(&remote)?;
    admitted_now_ms(now_ms)?;
    let transaction = connection.transaction()?;
    let evidence = transaction
        .query_row(
            "SELECT o.operation_id,o.base_etag,o.base_raw_ics,o.intent_json,s.uid,
                    r.etag,r.raw_ics,o.account_id
             FROM outbox_operations o
             JOIN remote_resources r ON r.resource_id=o.resource_id
                 AND r.calendar_id=o.calendar_id AND r.series_id=o.series_id
             JOIN calendars cal ON cal.calendar_id=o.calendar_id AND cal.account_id=o.account_id
             JOIN event_series s ON s.series_id=o.series_id AND s.account_id=o.account_id
             WHERE o.operation_kind='update_title' AND o.claim_token=?1 AND o.state='pending'
                 AND o.claimed_at_ms BETWEEN ?2 AND ?3",
            params![
                claim_token,
                now_ms.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),
                now_ms
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::OutboxClaimLost)?;
    let base_uid =
        single_vevent_uid(&evidence.2).map_err(|_| PersistenceError::RemoteReadBackMismatch)?;
    let remote_uid =
        single_vevent_uid(&remote.raw_ics).map_err(|_| PersistenceError::RemoteReadBackMismatch)?;
    let intent = parse_stored_title_intent(&evidence.3)?;
    if !strong_mutation_etag(&evidence.1)
        || base_uid != evidence.4
        || remote_uid != base_uid
        || evidence.5 != evidence.1
        || evidence.6 != evidence.2
        || patch_single_vevent_title(&evidence.2, &intent.title).is_err()
        || single_vevent_title(&remote.raw_ics).is_err()
    {
        return Err(PersistenceError::RemoteReadBackMismatch);
    }
    // Neither contradictory bytes nor the unchanged base can prove a
    // competing remote revision under the captured ETag.
    if remote.etag == evidence.1 {
        return Err(PersistenceError::RemoteReadBackMismatch);
    }
    transaction.execute(
        "INSERT INTO conflicts(conflict_id,operation_id,reason,base_etag,base_raw_ics,
            local_intent_json,remote_etag,remote_raw_ics,resolution_state,created_at_ms)
         VALUES(?1,?2,'concurrent_remote_change',?3,?4,?5,?6,?7,'unresolved',?8)",
        params![
            Uuid::new_v4().to_string(),
            evidence.0,
            evidence.1,
            evidence.2,
            evidence.3,
            remote.etag,
            remote.raw_ics,
            now_ms
        ],
    )?;
    transaction.execute(
        "UPDATE outbox_operations SET state='conflict',claim_token=NULL,claimed_at_ms=NULL,
            updated_at_ms=?2 WHERE operation_id=?1",
        params![evidence.0, now_ms],
    )?;
    transaction.commit()?;
    Ok(evidence.7)
}

fn apply_claimed_remote_mutation(
    connection: &mut Connection,
    claim_token: &str,
    remote: RemoteReadBack,
    now_ms: i64,
) -> Result<String, PersistenceError> {
    validate_remote_read_back(&remote)?;
    admitted_now_ms(now_ms)?;
    let canonical_title = single_vevent_title(&remote.raw_ics)?;
    let transaction = connection.transaction()?;
    let operation = transaction
        .query_row(
            "SELECT o.operation_id,o.resource_id,o.series_id,o.calendar_id,o.intent_json,o.base_raw_ics,
                    o.base_etag,s.uid,r.etag,r.raw_ics,o.account_id
             FROM outbox_operations o
             JOIN remote_resources r ON r.resource_id=o.resource_id
                 AND r.calendar_id=o.calendar_id AND r.series_id=o.series_id
             JOIN calendars cal ON cal.calendar_id=o.calendar_id AND cal.account_id=o.account_id
             JOIN event_series s ON s.series_id=o.series_id AND s.account_id=o.account_id
             WHERE o.operation_kind='update_title' AND o.claim_token=?1 AND o.state='pending'
                 AND o.claimed_at_ms BETWEEN ?2 AND ?3",
            params![
                claim_token,
                now_ms.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),
                now_ms
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Vec<u8>>(9)?,
                    row.get::<_, String>(10)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::OutboxClaimLost)?;
    let intent = parse_stored_title_intent(&operation.4)?;
    let expected_raw = patch_single_vevent_title(&operation.5, &intent.title)
        .map_err(|_| PersistenceError::RemoteReadBackMismatch)?;
    if !strong_mutation_etag(&operation.6)
        || !single_vevent_uid(&operation.5).is_ok_and(|uid| uid == operation.7)
        || operation.8 != operation.6
        || operation.9 != operation.5
        || canonical_title != intent.title
        || remote.raw_ics != expected_raw
        || (remote.etag == operation.6 && remote.raw_ics != operation.5)
    {
        return Err(PersistenceError::RemoteReadBackMismatch);
    }
    transaction.execute(
        "UPDATE remote_resources SET etag=?2,raw_ics=?3,lifecycle='active',
            remote_deleted_at_ms=NULL,last_seen_at_ms=?4,updated_at_ms=?4 WHERE resource_id=?1",
        params![operation.1, remote.etag, remote.raw_ics, now_ms],
    )?;
    transaction.execute(
        "UPDATE calendar_event_projections SET title=?3,source_etag=?4,updated_at_ms=?5
         WHERE series_id=?1 AND resource_id=?2",
        params![
            operation.2,
            operation.1,
            canonical_title,
            remote.etag,
            now_ms
        ],
    )?;
    transaction.execute(
        "UPDATE outbox_operations SET state='applied',claim_token=NULL,claimed_at_ms=NULL,
            last_outcome=NULL,updated_at_ms=?2 WHERE operation_id=?1",
        params![operation.0, now_ms],
    )?;
    transaction.commit()?;
    Ok(operation.10)
}

fn validate_remote_read_back(remote: &RemoteReadBack) -> Result<(), PersistenceError> {
    if !remote.has_bounded_strong_revision() {
        Err(PersistenceError::RemoteReadBackMismatch)
    } else {
        Ok(())
    }
}

fn resolve_remote_conflict(
    connection: &mut Connection,
    operation_id: &str,
    choice: ConflictResolutionChoice,
    now_ms: i64,
) -> Result<ConflictResolutionResult, PersistenceError> {
    let is_deletion = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_delete_operations WHERE operation_id=?1)",
        [operation_id],
        |row| row.get::<_, bool>(0),
    )?;
    if is_deletion {
        return resolve_remote_deletion_conflict(connection, operation_id, choice, now_ms);
    }
    resolve_remote_title_conflict(connection, operation_id, choice, now_ms)
}

fn resolve_remote_title_conflict(
    connection: &mut Connection,
    operation_id: &str,
    choice: ConflictResolutionChoice,
    now_ms: i64,
) -> Result<ConflictResolutionResult, PersistenceError> {
    admitted_now_ms(now_ms)?;
    Uuid::parse_str(operation_id).map_err(|_| PersistenceError::OutboxClaimLost)?;
    let transaction = connection.transaction()?;
    type ConflictRow = (
        String,
        String,
        Option<String>,
        String,
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
        String,
        String,
        bool,
        Option<i64>,
        bool,
        String,
        String,
        String,
        String,
        String,
        String,
    );
    let row: ConflictRow = transaction
        .query_row(
            "SELECT c.resolution_state,o.state,c.retry_operation_id,o.account_id,o.calendar_id,
                    o.resource_id,o.series_id,r.etag,r.raw_ics,o.intent_json,r.parse_status,
                    r.lifecycle,a.enabled,a.removed_at_ms,cal.available,cal.privileges,o.idempotency_key,
                    cal.collection_url,r.exact_href,s.uid,o.base_etag
             FROM conflicts c
             JOIN outbox_operations o ON o.operation_id=c.operation_id
             JOIN remote_resources r ON r.resource_id=o.resource_id
                 AND r.calendar_id=o.calendar_id AND r.series_id=o.series_id
             JOIN calendars cal ON cal.calendar_id=o.calendar_id AND cal.account_id=o.account_id
             JOIN accounts a ON a.account_id=o.account_id
             JOIN event_series s ON s.series_id=o.series_id AND s.account_id=o.account_id
             WHERE o.operation_kind='update_title' AND c.operation_id=?1",
            [operation_id],
            |row| {
                Ok((
                    row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
                    row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?,
                    row.get(10)?, row.get(11)?, row.get(12)?, row.get(13)?, row.get(14)?,
                    row.get(15)?, row.get(16)?, row.get(17)?, row.get(18)?, row.get(19)?,
                    row.get(20)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    let (
        resolution_state,
        operation_state,
        retry_operation_id,
        account_id,
        calendar_id,
        resource_id,
        series_id,
        current_etag,
        current_raw_ics,
        intent_json,
        parse_status,
        lifecycle,
        enabled,
        removed_at_ms,
        available,
        privileges,
        _old_idempotency_key,
        collection_href,
        exact_href,
        series_uid,
        base_etag,
    ) = row;
    if resolution_state != "unresolved" {
        if resolution_state != choice.as_str() {
            return Err(PersistenceError::ConflictAlreadyResolved);
        }
        let retry_operation = retry_operation_id
            .as_deref()
            .map(|id| remote_mutation_summary(&transaction, id))
            .transpose()?;
        transaction.commit()?;
        return Ok(ConflictResolutionResult {
            operation_id: operation_id.to_owned(),
            account_id,
            choice,
            retry_operation,
        });
    }
    if operation_state != "conflict" {
        return Err(PersistenceError::ConflictAlreadyResolved);
    }

    let retry_operation = if choice == ConflictResolutionChoice::KeepRemote {
        transaction.execute(
            "UPDATE outbox_operations SET state='cancelled',updated_at_ms=?2 WHERE operation_id=?1",
            params![operation_id, now_ms],
        )?;
        None
    } else {
        if !enabled || removed_at_ms.is_some() || !available {
            return Err(PersistenceError::AccountDisabled);
        }
        let can_write = collection_content_writable(&privileges);
        let intent = parse_stored_title_intent(&intent_json)?;
        if parse_status != "parsed"
            || lifecycle != "active"
            || !can_write
            || !resource_belongs_to_collection(&collection_href, &exact_href)
            || !strong_mutation_etag(&current_etag)
            || current_etag == base_etag
            || contains_invitation_property(&current_raw_ics)
            || !single_vevent_uid(&current_raw_ics).is_ok_and(|uid| uid == series_uid)
            || patch_single_vevent_title(&current_raw_ics, &intent.title).is_err()
        {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
        let new_operation_id = Uuid::new_v4().to_string();
        let new_idempotency_key = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO outbox_operations(
                operation_id,idempotency_key,account_id,calendar_id,resource_id,series_id,
                operation_kind,base_etag,base_raw_ics,intent_json,state,created_at_ms,updated_at_ms
             ) VALUES(?1,?2,?3,?4,?5,?6,'update_title',?7,?8,?9,'pending',?10,?10)",
            params![
                new_operation_id,
                new_idempotency_key,
                account_id,
                calendar_id,
                resource_id,
                series_id,
                current_etag,
                current_raw_ics,
                intent_json,
                now_ms
            ],
        )?;
        Some(RemoteMutationSummary {
            operation_id: new_operation_id,
            idempotency_key: new_idempotency_key,
            account_id: account_id.clone(),
            resource_id,
            state: RemoteMutationState::Pending,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        })
    };
    transaction.execute(
        "UPDATE conflicts SET resolution_state=?2,resolved_at_ms=?3,retry_operation_id=?4
         WHERE operation_id=?1 AND resolution_state='unresolved'",
        params![
            operation_id,
            choice.as_str(),
            now_ms,
            retry_operation.as_ref().map(|item| &item.operation_id)
        ],
    )?;
    transaction.commit()?;
    Ok(ConflictResolutionResult {
        operation_id: operation_id.to_owned(),
        account_id,
        choice,
        retry_operation,
    })
}

fn resolve_remote_deletion_conflict(
    connection: &mut Connection,
    operation_id: &str,
    choice: ConflictResolutionChoice,
    now_ms: i64,
) -> Result<ConflictResolutionResult, PersistenceError> {
    resolve_remote_deletion_conflict_scoped(connection, operation_id, choice, now_ms, false)
}

fn resolve_remote_deletion_conflict_scoped(
    connection: &mut Connection,
    operation_id: &str,
    choice: ConflictResolutionChoice,
    now_ms: i64,
    supported_crud: bool,
) -> Result<ConflictResolutionResult, PersistenceError> {
    admitted_now_ms(now_ms)?;
    Uuid::parse_str(operation_id).map_err(|_| PersistenceError::OutboxClaimLost)?;
    let transaction = connection.transaction()?;
    type ConflictRow = (
        Option<String>,
        String,
        Option<String>,
        String,
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
        String,
        bool,
        Option<i64>,
        bool,
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<Vec<u8>>,
    );
    let row: ConflictRow = transaction
        .query_row(
            "SELECT d.resolution_state,d.state,d.retry_operation_id,d.account_id,d.calendar_id,
                    d.resource_id,d.series_id,r.etag,r.raw_ics,r.parse_status,r.lifecycle,
                    a.enabled,a.removed_at_ms,c.available,c.privileges,c.collection_url,
                    r.exact_href,s.uid,d.base_etag,d.intent_json,d.remote_etag,d.remote_raw_ics
             FROM remote_delete_operations d
             JOIN remote_resources r ON r.resource_id=d.resource_id
                 AND r.calendar_id=d.calendar_id AND r.series_id=d.series_id
             JOIN calendars c ON c.calendar_id=d.calendar_id AND c.account_id=d.account_id
             JOIN accounts a ON a.account_id=d.account_id
             JOIN event_series s ON s.series_id=d.series_id AND s.account_id=d.account_id
             WHERE d.operation_id=?1",
            [operation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                    row.get(16)?,
                    row.get(17)?,
                    row.get(18)?,
                    row.get(19)?,
                    row.get(20)?,
                    row.get(21)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    let (
        resolution_state,
        operation_state,
        retry_operation_id,
        account_id,
        calendar_id,
        resource_id,
        series_id,
        current_etag,
        current_raw_ics,
        parse_status,
        lifecycle,
        enabled,
        removed_at_ms,
        available,
        privileges,
        collection_href,
        exact_href,
        series_uid,
        base_etag,
        intent_json,
        conflict_etag,
        conflict_raw_ics,
    ) = row;
    let resolution_state = resolution_state.ok_or(PersistenceError::ConflictAlreadyResolved)?;
    if resolution_state != "unresolved" {
        if resolution_state != choice.as_str() {
            return Err(PersistenceError::ConflictAlreadyResolved);
        }
        let retry_operation = retry_operation_id
            .as_deref()
            .map(|id| remote_deletion_summary(&transaction, id))
            .transpose()?;
        transaction.commit()?;
        return Ok(ConflictResolutionResult {
            operation_id: operation_id.to_owned(),
            account_id,
            choice,
            retry_operation,
        });
    }
    if operation_state != "conflict" || intent_json != r#"{"delete":true}"# {
        return Err(PersistenceError::ConflictAlreadyResolved);
    }

    let retry_operation = if choice == ConflictResolutionChoice::KeepRemote {
        transaction.execute(
            "UPDATE remote_delete_operations SET state='cancelled',updated_at_ms=?2
             WHERE operation_id=?1 AND state='conflict' AND resolution_state='unresolved'",
            params![operation_id, now_ms],
        )?;
        None
    } else {
        if !enabled || removed_at_ms.is_some() || !available {
            return Err(PersistenceError::AccountDisabled);
        }
        let conflict_remote = match (conflict_etag, conflict_raw_ics) {
            (Some(etag), Some(raw_ics)) => RemoteReadBack { etag, raw_ics },
            _ => return Err(PersistenceError::RemoteReadBackMismatch),
        };
        let pending_other = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM outbox_operations WHERE resource_id=?1 AND state='pending'
                UNION ALL
                SELECT 1 FROM remote_delete_operations WHERE resource_id=?1 AND state='pending'
             )",
            [&resource_id],
            |row| row.get::<_, bool>(0),
        )?;
        if pending_other
            || parse_status != "parsed"
            || lifecycle != "active"
            || !collection_content_writable(&privileges)
            || !resource_belongs_to_collection(&collection_href, &exact_href)
            || !strong_mutation_etag(&current_etag)
            || current_etag == base_etag
            || !conflict_remote.has_bounded_strong_revision()
            || !single_vevent_uid(&conflict_remote.raw_ics).is_ok_and(|uid| uid == series_uid)
            || contains_invitation_property(&current_raw_ics)
            || contains_recurrence_sensitive_property(&current_raw_ics)
            || !single_vevent_uid(&current_raw_ics).is_ok_and(|uid| uid == series_uid)
        {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
        if supported_crud {
            let eligible: bool = transaction.query_row(
                "SELECT provider_kind='caldav' AND remote_editing_enabled=1 AND credential_configured=1 AND region_profile='icloud-cn' FROM accounts WHERE account_id=?1",
                [&account_id], |r|r.get(0),
            )?;
            if !eligible {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
        }
        let new_operation_id = Uuid::new_v4().to_string();
        let new_idempotency_key = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO remote_delete_operations(
                operation_id,idempotency_key,account_id,calendar_id,resource_id,series_id,
                base_etag,base_raw_ics,intent_json,state,created_at_ms,updated_at_ms
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'{\"delete\":true}','pending',?9,?9)",
            params![
                new_operation_id,
                new_idempotency_key,
                account_id,
                calendar_id,
                resource_id,
                series_id,
                current_etag,
                current_raw_ics,
                now_ms
            ],
        )?;
        Some(RemoteMutationSummary {
            operation_id: new_operation_id,
            idempotency_key: new_idempotency_key,
            account_id: account_id.clone(),
            resource_id,
            state: RemoteMutationState::Pending,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        })
    };
    transaction.execute(
        "UPDATE remote_delete_operations SET resolution_state=?2,resolved_at_ms=?3,
             retry_operation_id=?4,updated_at_ms=?3
         WHERE operation_id=?1 AND resolution_state='unresolved'",
        params![
            operation_id,
            choice.as_str(),
            now_ms,
            retry_operation.as_ref().map(|item| &item.operation_id)
        ],
    )?;
    transaction.commit()?;
    Ok(ConflictResolutionResult {
        operation_id: operation_id.to_owned(),
        account_id,
        choice,
        retry_operation,
    })
}

fn remote_mutation_summary(
    connection: &Connection,
    operation_id: &str,
) -> Result<RemoteMutationSummary, PersistenceError> {
    connection
        .query_row(
            "SELECT operation_id,idempotency_key,account_id,resource_id,state,created_at_ms,updated_at_ms
         FROM outbox_operations WHERE operation_kind='update_title' AND operation_id=?1",
            [operation_id],
            |row| {
                let state = row.get::<_, String>(4)?;
                Ok(RemoteMutationSummary {
                    operation_id: row.get(0)?,
                    idempotency_key: row.get(1)?,
                    account_id: row.get(2)?,
                    resource_id: row.get(3)?,
                    state: RemoteMutationState::from_storage(&state).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    created_at_ms: row.get(5)?,
                    updated_at_ms: row.get(6)?,
                })
            },
        )
        .map_err(PersistenceError::from)
}

fn remote_deletion_summary(
    connection: &Connection,
    operation_id: &str,
) -> Result<RemoteMutationSummary, PersistenceError> {
    connection
        .query_row(
            "SELECT operation_id,idempotency_key,account_id,resource_id,state,created_at_ms,updated_at_ms
             FROM remote_delete_operations WHERE operation_id=?1",
            [operation_id],
            |row| {
                let state = row.get::<_, String>(4)?;
                Ok(RemoteMutationSummary {
                    operation_id: row.get(0)?,
                    idempotency_key: row.get(1)?,
                    account_id: row.get(2)?,
                    resource_id: row.get(3)?,
                    state: RemoteMutationState::from_storage(&state).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    created_at_ms: row.get(5)?,
                    updated_at_ms: row.get(6)?,
                })
            },
        )
        .map_err(PersistenceError::from)
}

fn record_remote_change_conflict(
    transaction: &Transaction<'_>,
    resource_id: &str,
    remote_etag: &str,
    remote_raw_ics: &[u8],
    observed_at_ms: i64,
) -> Result<(), PersistenceError> {
    // A terminal (applied) local write records the read-back it observed at
    // completion time. That evidence is history, not the resource's current
    // revision: the provider may re-serialize a stored resource, or relabel it
    // between reads, while keeping the same label. Rejecting the whole account
    // stage on that difference blocked all synchronization permanently once a
    // single app-created resource was re-serialized, so a fresh read of the
    // *current* revision is committed as the cache content. It authorizes no
    // write, and every pending/claimed intent below still fails closed.
    //
    // Two cases stay fail-closed:
    //  * a stage whose label matches the *base* of an applied or conflicted
    //    local operation was collected before that operation's own write
    //    landed, so committing it would regress the cache past our terminal
    //    evidence;
    //  * an applied create whose recorded label reappears with different bytes
    //    while the committed cache already carries a *different* label, which
    //    means the stage re-delivers an older revision of our own write.
    let committed: Option<(String, Vec<u8>, String)> = transaction
        .query_row(
            "SELECT r.etag,r.raw_ics,s.uid FROM remote_resources r
             JOIN event_series s ON s.series_id=r.series_id
             WHERE r.resource_id=?1",
            [resource_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let committed_label_matches = committed
        .as_ref()
        .is_some_and(|(etag, _, _)| etag == remote_etag);
    let applied_create_contradicts_committed_revision = !committed_label_matches
        && transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_create_operations WHERE resource_id=?1
             AND state='applied' AND applied_etag=?2 AND applied_raw_ics<>?3)",
            params![resource_id, remote_etag, remote_raw_ics],
            |r| r.get::<_, bool>(0),
        )?;
    if applied_create_contradicts_committed_revision {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    let terminal_base_reappeared = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM outbox_operations
            WHERE resource_id=?1 AND base_etag=?2
              AND (state='applied' OR EXISTS(
                  SELECT 1 FROM conflicts WHERE operation_id=outbox_operations.operation_id
              ))
            UNION ALL
            SELECT 1 FROM remote_delete_operations
            WHERE resource_id=?1 AND base_etag=?2 AND state IN ('applied','conflict')
         )",
        params![resource_id, remote_etag],
        |row| row.get::<_, bool>(0),
    )?;
    if terminal_base_reappeared {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    let conflict_revision_disagrees = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM outbox_operations o
            JOIN conflicts c ON c.operation_id=o.operation_id
            WHERE o.resource_id=?1 AND c.remote_etag=?2 AND c.remote_raw_ics<>?3
            UNION ALL
            SELECT 1 FROM remote_delete_operations d
            WHERE d.resource_id=?1 AND d.state='conflict'
              AND d.remote_etag=?2 AND d.remote_raw_ics<>?3
         )",
        params![resource_id, remote_etag, remote_raw_ics],
        |row| row.get::<_, bool>(0),
    )?;
    if conflict_revision_disagrees {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    let pending_delete = transaction
        .query_row(
            "SELECT base_etag,base_raw_ics FROM remote_delete_operations
             WHERE resource_id=?1 AND state='pending'",
            [resource_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    if let Some((base_etag, base_raw_ics)) = pending_delete {
        return if base_etag == remote_etag && base_raw_ics == remote_raw_ics {
            Ok(())
        } else {
            Err(PersistenceError::UnsafeSyncStage)
        };
    }
    let pending = transaction
        .query_row(
            "SELECT operation_id,base_etag,base_raw_ics,intent_json,
                    claim_token,execution_phase,operation_kind
             FROM outbox_operations WHERE resource_id=?1 AND state='pending'",
            [resource_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((operation_id, base_etag, base_raw_ics, local_intent_json, claim_token, phase, kind)) =
        pending
    else {
        return Ok(());
    };
    if kind == "update_event" {
        remote_update_fields::validate_saved_event_base(transaction, &operation_id)?;
    }
    if base_etag == remote_etag {
        return if base_raw_ics == remote_raw_ics {
            Ok(())
        } else {
            Err(PersistenceError::UnsafeSyncStage)
        };
    }
    // A changed revision might be this operation's own conditional write.
    // Only exact-target GET-first recovery can classify a leased or ambiguous
    // attempt; an inventory response must not consume its claim as conflict.
    if kind == "update_event" {
        return remote_update_event::sync_conflict(
            transaction,
            resource_id,
            &RemoteReadBack {
                etag: remote_etag.into(),
                raw_ics: remote_raw_ics.to_vec(),
            },
            observed_at_ms,
        );
    }
    if kind == "update_fields" {
        return remote_update_fields::sync_conflict(
            transaction,
            resource_id,
            &RemoteReadBack {
                etag: remote_etag.into(),
                raw_ics: remote_raw_ics.to_vec(),
            },
            observed_at_ms,
        );
    }
    if kind != "update_title" || claim_token.is_some() || phase != "ready" {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    let intent = parse_stored_title_intent(&local_intent_json)
        .map_err(|_| PersistenceError::UnsafeSyncStage)?;
    let series_uid = committed
        .as_ref()
        .map(|(_, _, uid)| uid.as_str())
        .ok_or(PersistenceError::UnsafeSyncStage)?;
    if !strong_mutation_etag(&base_etag)
        || !single_vevent_uid(&base_raw_ics).is_ok_and(|uid| uid == series_uid)
        || contains_invitation_property(&base_raw_ics)
        || patch_single_vevent_title(&base_raw_ics, &intent.title).is_err()
    {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    transaction.execute(
        "INSERT INTO conflicts(
            conflict_id,operation_id,reason,base_etag,base_raw_ics,local_intent_json,
            remote_etag,remote_raw_ics,resolution_state,created_at_ms
         ) VALUES(?1,?2,'concurrent_remote_change',?3,?4,?5,?6,?7,'unresolved',?8)",
        params![
            Uuid::new_v4().to_string(),
            operation_id,
            base_etag,
            base_raw_ics,
            local_intent_json,
            remote_etag,
            remote_raw_ics,
            observed_at_ms
        ],
    )?;
    transaction.execute(
        "UPDATE outbox_operations SET state='conflict',updated_at_ms=?2
         WHERE operation_id=?1 AND state='pending'",
        params![operation_id, observed_at_ms],
    )?;
    Ok(())
}

fn resource_has_inflight_mutation(
    connection: &Connection,
    resource_id: &str,
) -> Result<bool, PersistenceError> {
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM outbox_operations
                WHERE resource_id=?1 AND state='pending'
                  AND (claim_token IS NOT NULL OR execution_phase<>'ready')
                UNION ALL
                SELECT 1 FROM remote_delete_operations
                WHERE resource_id=?1 AND state='pending'
                UNION ALL
                SELECT 1 FROM remote_create_operations o JOIN remote_resources r
                  ON r.calendar_id=o.calendar_id AND r.exact_href=o.exact_href
                WHERE r.resource_id=?1 AND o.state='pending'
                  AND (o.claim_token IS NOT NULL OR o.claimed_at_ms IS NOT NULL OR o.execution_phase<>'ready')
             )",
            [resource_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn calendar_has_pending_mutation(
    connection: &Connection,
    calendar_id: &str,
) -> Result<bool, PersistenceError> {
    connection
        .query_row(
            "SELECT EXISTS(
            SELECT 1 FROM outbox_operations WHERE calendar_id=?1 AND state='pending'
            UNION ALL
            SELECT 1 FROM remote_delete_operations WHERE calendar_id=?1 AND state='pending'
            UNION ALL
            SELECT 1 FROM remote_create_operations WHERE calendar_id=?1 AND state='pending'
         )",
            [calendar_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn calendar_has_inflight_mutation(
    connection: &Connection,
    calendar_id: &str,
) -> Result<bool, PersistenceError> {
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM outbox_operations
                WHERE calendar_id=?1 AND state='pending'
                  AND (claim_token IS NOT NULL OR execution_phase<>'ready')
                UNION ALL
                SELECT 1 FROM remote_delete_operations
                WHERE calendar_id=?1 AND state='pending'
                UNION ALL
                SELECT 1 FROM remote_create_operations
                WHERE calendar_id=?1 AND state='pending'
                  AND (claim_token IS NOT NULL OR claimed_at_ms IS NOT NULL OR execution_phase<>'ready')
             )",
            [calendar_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

pub(crate) fn contains_invitation_property(raw_ics: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(raw_ics) else {
        return true;
    };
    text.replace("\r\n ", "")
        .replace("\r\n\t", "")
        .lines()
        .map(str::trim)
        .any(|line| {
            line.split_once([':', ';']).is_some_and(|(name, _)| {
                name.eq_ignore_ascii_case("ORGANIZER") || name.eq_ignore_ascii_case("ATTENDEE")
            })
        })
}

fn contains_recurrence_sensitive_property(raw_ics: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(raw_ics) else {
        return true;
    };
    let unfolded = text.replace("\r\n ", "").replace("\r\n\t", "");
    let mut components = Vec::new();
    for line in unfolded.lines().map(str::trim) {
        let Some((head, value)) = line.split_once(':') else {
            continue;
        };
        let name = head.split(';').next().unwrap_or_default();
        if name.eq_ignore_ascii_case("BEGIN") {
            components.push(value.trim().to_ascii_uppercase());
            continue;
        }
        if name.eq_ignore_ascii_case("END") {
            if components.pop().as_deref() != Some(value.trim().to_ascii_uppercase().as_str()) {
                return true;
            }
            continue;
        }
        if components
            .last()
            .is_some_and(|component| component == "VEVENT")
            && matches!(
                name.to_ascii_uppercase().as_str(),
                "RRULE" | "RDATE" | "EXDATE" | "RECURRENCE-ID"
            )
        {
            return true;
        }
    }
    !components.is_empty()
}

fn list_cached_calendar_resources(
    connection: &Connection,
) -> Result<Vec<CachedCalendarResource>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT r.resource_id,r.series_id,r.calendar_id,r.exact_href,r.etag,r.content_type,r.raw_ics,s.uid,r.parse_status,r.lifecycle,r.remote_deleted_at_ms
         FROM remote_resources r JOIN event_series s ON s.series_id=r.series_id
         ORDER BY r.calendar_id,r.exact_href,r.resource_id",
    )?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Vec<u8>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<i64>>(10)?,
            ))
        })?
        .map(|row| {
            let (
                resource_id,
                series_id,
                calendar_id,
                href,
                etag,
                content_type,
                raw_ics,
                uid,
                parse_status,
                lifecycle,
                remote_deleted_at_ms,
            ) = row?;
            Ok(CachedCalendarResource {
                resource_id,
                series_id,
                calendar_id,
                href,
                etag,
                content_type,
                raw_ics,
                uid,
                parse_status: ParseStatus::from_storage(&parse_status)?,
                lifecycle: ResourceLifecycle::from_storage(&lifecycle)?,
                remote_deleted_at_ms,
            })
        })
        .collect::<Result<Vec<_>, PersistenceError>>()
}

fn overlay_pending_remote_updates(
    connection: &Connection,
    items: &mut [CalendarAgendaItem],
) -> Result<(), PersistenceError> {
    use crate::domain::remote_mutation::RemoteOptionalTextUpdateV2;
    let mut statement = connection.prepare(
        "SELECT o.operation_id,o.account_id,o.resource_id,o.intent_json,o.state,o.execution_phase,o.last_outcome
         FROM outbox_operations o
         JOIN calendars c ON c.calendar_id=o.calendar_id
         JOIN accounts a ON a.account_id=o.account_id
         WHERE o.operation_kind='update_event' AND o.state IN ('pending','conflict')
           AND c.selected=1 AND a.removed_at_ms IS NULL
         ORDER BY o.created_at_ms,o.operation_id",
    )?;
    let mut rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut successors=connection.prepare("SELECT u.operation_id,u.account_id,u.resource_id,u.intent_json,CASE WHEN u.state='blocked_conflict' THEN 'conflict' ELSE 'pending' END,'ready',NULL FROM remote_update_successors u JOIN calendars c ON c.calendar_id=u.calendar_id JOIN accounts a ON a.account_id=u.account_id WHERE c.selected=1 AND a.removed_at_ms IS NULL AND u.state<>'cancelled' ORDER BY u.updated_at_ms,u.operation_id")?;
    rows.extend(
        successors
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?,
    );
    for (operation_id, account_id, resource_id, intent, state, phase, outcome) in rows {
        let fields = crate::domain::remote_event_update::decode_event_intent_v3(intent.as_bytes())?;
        for item in items
            .iter_mut()
            .filter(|item| item.resource_id == resource_id)
        {
            if let Some(title) = &fields.title {
                item.title = title.clone();
            }
            if let Some(description) = &fields.description {
                item.notes = match description {
                    RemoteOptionalTextUpdateV2::Set(value) => Some(value.clone()),
                    RemoteOptionalTextUpdateV2::Clear => None,
                };
            }
            if let Some(location) = &fields.location {
                item.location = match location {
                    RemoteOptionalTextUpdateV2::Set(value) => Some(value.clone()),
                    RemoteOptionalTextUpdateV2::Clear => None,
                };
            }
            if let Some(time) = fields.time {
                item.start_at_utc_ms = Some(time.start_at_utc_ms);
                item.end_at_utc_ms = Some(time.end_at_utc_ms);
            }
            item.remote_edit_eligible = false;
            item.remote_edit_blocked_reason = Some("此修改仍待同步".into());
            item.pending_remote = Some(PendingRemoteAgendaState {
                operation_id: operation_id.clone(),
                account_id: account_id.clone(),
                kind: PendingRemoteAgendaKind::UpdateEventV3,
                status: pending_agenda_status(&state, &phase, outcome.as_deref()),
            });
        }
    }
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingCreationIntent {
    version: String,
    title: String,
    description: Option<String>,
    location: Option<String>,
    start_at_utc_ms: i64,
    end_at_utc_ms: i64,
}

fn overlay_pending_remote_deletions(
    connection: &Connection,
    items: &mut [CalendarAgendaItem],
) -> Result<(), PersistenceError> {
    let mut statement = connection.prepare("SELECT d.operation_id,d.account_id,d.resource_id,d.state,d.execution_phase,d.last_outcome FROM remote_delete_operations d JOIN calendars c ON c.calendar_id=d.calendar_id JOIN accounts a ON a.account_id=d.account_id WHERE d.state IN ('pending','conflict') AND c.selected=1 AND a.removed_at_ms IS NULL ORDER BY d.created_at_ms,d.operation_id")?;
    let rows = statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    for row in rows {
        let (operation, account, resource, state, phase, outcome) = row?;
        for item in items.iter_mut().filter(|item| item.resource_id == resource) {
            item.remote_edit_eligible = false;
            item.remote_time_edit_eligible = false;
            item.remote_edit_blocked_reason = Some("此事件有待完成的同步操作".into());
            item.pending_remote = Some(PendingRemoteAgendaState {
                operation_id: operation.clone(),
                account_id: account.clone(),
                kind: PendingRemoteAgendaKind::Delete,
                status: if state == "conflict" {
                    PendingRemoteAgendaStatus::Conflict
                } else {
                    pending_agenda_status(&state, &phase, outcome.as_deref())
                },
            });
        }
    }
    Ok(())
}

fn append_pending_remote_creations(
    connection: &Connection,
    display_now_utc_ms: Option<i64>,
    items: &mut Vec<CalendarAgendaItem>,
) -> Result<(), PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT o.operation_id,o.idempotency_key,o.account_id,o.calendar_id,o.resource_id,o.intent_json,o.state,o.execution_phase,o.attempts,
                c.display_name,COALESCE(c.color_override,c.server_color),c.display_past_days,c.display_future_days
         FROM remote_create_operations o
         JOIN calendars c ON c.calendar_id=o.calendar_id
         JOIN accounts a ON a.account_id=o.account_id
         WHERE o.intent_version='utc-v2' AND o.state='pending' AND c.selected=1 AND a.removed_at_ms IS NULL
         ORDER BY o.created_at_ms,o.operation_id",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (
        operation_id,
        idempotency_key,
        account_id,
        calendar_id,
        resource_id,
        json,
        state,
        phase,
        attempts,
        calendar_name,
        calendar_color,
        past_days,
        future_days,
    ) in rows
    {
        let intent: PendingCreationIntent =
            serde_json::from_str(&json).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
        if intent.version != "utc-v2" {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
        let candidate = QueueRemoteEventCreateV2 {
            idempotency_key,
            calendar_id: calendar_id.clone(),
            title: intent.title.clone(),
            description: intent.description.clone(),
            location: intent.location.clone(),
            start_at_utc_ms: intent.start_at_utc_ms,
            end_at_utc_ms: intent.end_at_utc_ms,
        };
        if candidate.clone().validate()? != candidate {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
        if display_now_utc_ms.is_some_and(|now| {
            intent.start_at_utc_ms >= now + future_days * 86_400_000
                || (intent.start_at_utc_ms < now - past_days * 86_400_000
                    && intent.end_at_utc_ms <= now - past_days * 86_400_000)
        }) {
            continue;
        }
        items.push(CalendarAgendaItem {
            series_id: resource_id.clone(),
            resource_id,
            recurrence_key: "SINGLETON".into(),
            calendar_id,
            calendar_name,
            calendar_color,
            title: intent.title,
            notes: intent.description,
            location: intent.location,
            start_at_utc_ms: Some(intent.start_at_utc_ms),
            end_at_utc_ms: Some(intent.end_at_utc_ms),
            all_day: false,
            status: ProjectionStatus::Ready,
            unsupported_reason: None,
            work_item_id: None,
            completed: false,
            completed_at_ms: None,
            important: false,
            tags: Vec::new(),
            recurrence: None,
            subtasks: Vec::new(),
            remote_edit_eligible: false,
            remote_edit_blocked_reason: Some("此日程仍待同步".into()),
            remote_time_edit_eligible: false,
            pending_remote: Some(PendingRemoteAgendaState {
                operation_id,
                account_id,
                kind: PendingRemoteAgendaKind::CreateUtcV2,
                status: if attempts > 0 && phase == "verify_first" {
                    PendingRemoteAgendaStatus::RetryNeeded
                } else {
                    pending_agenda_status(&state, &phase, None)
                },
            }),
        });
    }
    Ok(())
}

fn pending_agenda_status(
    state: &str,
    phase: &str,
    outcome: Option<&str>,
) -> PendingRemoteAgendaStatus {
    if state == "conflict" {
        PendingRemoteAgendaStatus::Conflict
    } else if phase == "verify_first" || outcome.is_some() {
        PendingRemoteAgendaStatus::RetryNeeded
    } else {
        PendingRemoteAgendaStatus::Pending
    }
}

#[derive(Clone, Copy)]
struct ProjectionSource<'a> {
    series_id: &'a str,
    resource_id: &'a str,
    calendar_id: &'a str,
    etag: &'a str,
    raw_ics: &'a [u8],
    uid: &'a str,
}

/// Counts written by one series projection. Both outcomes are bounded numbers,
/// never content, so they can cross the refresh boundary safely.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProjectionCounts {
    occurrences: u32,
    unsupported_series: u32,
}

fn restore_active_alias_projection(
    transaction: &Transaction<'_>,
    series_id: &str,
    preferred_resource_id: Option<&str>,
    observed_at_ms: i64,
    window: ExpansionWindow,
) -> Result<(), PersistenceError> {
    type Alias = (String, String, String, Vec<u8>, String);
    let alias: Option<Alias> = transaction
        .query_row(
            "SELECT r.resource_id,r.calendar_id,r.etag,r.raw_ics,s.uid
             FROM remote_resources r
             JOIN event_series s ON s.series_id=r.series_id
             JOIN calendars c ON c.calendar_id=r.calendar_id AND c.account_id=s.account_id
             WHERE r.series_id=?1 AND r.lifecycle='active' AND c.available=1
             ORDER BY CASE WHEN r.resource_id=?2 THEN 0 ELSE 1 END,
                      r.exact_href,r.resource_id LIMIT 1",
            params![series_id, preferred_resource_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    if let Some((resource_id, calendar_id, etag, raw_ics, uid)) = alias {
        upsert_calendar_event_projection(
            transaction,
            ProjectionSource {
                series_id,
                resource_id: &resource_id,
                calendar_id: &calendar_id,
                etag: &etag,
                raw_ics: &raw_ics,
                uid: &uid,
            },
            observed_at_ms,
            window,
        )?;
    }
    Ok(())
}

fn upsert_calendar_event_projection(
    transaction: &Transaction<'_>,
    source: ProjectionSource<'_>,
    updated_at_ms: i64,
    window: ExpansionWindow,
) -> Result<ProjectionCounts, PersistenceError> {
    upsert_calendar_event_projection_inner(transaction, source, updated_at_ms, window, true)
}

fn upsert_calendar_event_projection_inner(
    transaction: &Transaction<'_>,
    source: ProjectionSource<'_>,
    updated_at_ms: i64,
    window: ExpansionWindow,
    touch_work_item: bool,
) -> Result<ProjectionCounts, PersistenceError> {
    let event = normalize_vevent(source.raw_ics, source.uid);
    transaction.execute(
        "DELETE FROM calendar_event_projections
         WHERE series_id=?1
           AND NOT (
               recurrence_key LIKE 'LOCAL:%'
               AND EXISTS(
                   SELECT 1 FROM work_item_state w
                   WHERE w.source_kind='calendar_occurrence'
                     AND w.calendar_series_id=calendar_event_projections.series_id
                     AND w.recurrence_key=calendar_event_projections.recurrence_key
                     AND w.completed=1
               )
           )",
        [source.series_id],
    )?;
    if event.unsupported_reason == Some(UnsupportedReason::UnsupportedRecurrence) {
        match expand_vevent_recurrence(source.raw_ics, source.uid, window) {
            Ok(expansion) => {
                let mut occurrences = 0_u32;
                for occurrence in expansion.occurrences {
                    insert_calendar_event_projection(
                        transaction,
                        source,
                        &occurrence.identity.canonical_key,
                        &occurrence.title,
                        occurrence.notes.as_deref(),
                        occurrence.location.as_deref(),
                        Some(occurrence.effective_start_at_utc_ms),
                        Some(occurrence.effective_end_at_utc_ms),
                        occurrence.all_day,
                        occurrence.cancelled,
                        ProjectionStatus::Ready,
                        None,
                        updated_at_ms,
                    )?;
                    occurrences = occurrences.saturating_add(1);
                    if touch_work_item && !occurrence.cancelled {
                        ensure_calendar_work_item(
                            transaction,
                            source.series_id,
                            &occurrence.identity.canonical_key,
                            updated_at_ms,
                        )?;
                    }
                }
                return Ok(ProjectionCounts {
                    occurrences,
                    unsupported_series: 0,
                });
            }
            Err(error) => {
                let reason = recurrence_unsupported_reason(error);
                insert_calendar_event_projection(
                    transaction,
                    source,
                    UNSUPPORTED_RECURRENCE_KEY,
                    "无法安全展开的重复日历事件",
                    None,
                    None,
                    None,
                    None,
                    false,
                    false,
                    ProjectionStatus::Unsupported,
                    Some(reason),
                    updated_at_ms,
                )?;
                return Ok(ProjectionCounts {
                    occurrences: 0,
                    unsupported_series: 1,
                });
            }
        }
    }
    insert_calendar_event_projection(
        transaction,
        source,
        SINGLETON_OCCURRENCE_KEY,
        &event.title,
        event.notes.as_deref(),
        event.location.as_deref(),
        event.start_at_utc_ms,
        event.end_at_utc_ms,
        event.all_day,
        event.cancelled,
        event.status,
        event.unsupported_reason,
        updated_at_ms,
    )?;
    if touch_work_item && event.status == ProjectionStatus::Ready && !event.cancelled {
        ensure_calendar_work_item(
            transaction,
            source.series_id,
            SINGLETON_OCCURRENCE_KEY,
            updated_at_ms,
        )?;
        materialize_local_calendar_recurrence(transaction, source, &event, updated_at_ms)?;
    }
    Ok(ProjectionCounts {
        occurrences: 1,
        unsupported_series: u32::from(event.status == ProjectionStatus::Unsupported),
    })
}

/// Local recurrence metadata is deliberately kept separate from the remote
/// ICS resource.  For an eligible singleton, materialize bounded local
/// projections so each occurrence has its own completion/work-item identity;
/// the remote resource itself remains a single event until a later sync step
/// advances its time.
fn materialize_local_calendar_recurrence(
    transaction: &Transaction<'_>,
    source: ProjectionSource<'_>,
    event: &NormalizedCalendarEvent,
    updated_at_ms: i64,
) -> Result<(), PersistenceError> {
    let recurrence = transaction
        .query_row(
            "SELECT frequency,interval_value,timezone,weekdays_mask,month_days_mask,year_months_mask,
                    end_kind,end_count,end_until_at_utc_ms,anchor_start_at_utc_ms,anchor_end_at_utc_ms
             FROM calendar_resource_recurrence WHERE resource_id=?1 AND enabled=1",
            [source.resource_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?, row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?, row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?, row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?, row.get::<_, Option<u16>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, i64>(9)?, row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()?;
    let Some((
        frequency,
        interval,
        timezone,
        weekdays_mask,
        month_days_mask,
        year_months_mask,
        end_kind,
        end_count,
        end_until_at_utc_ms,
        anchor_start_at_utc_ms,
        anchor_end_at_utc_ms,
    )) = recurrence
    else {
        return Ok(());
    };
    let frequency = RecurrenceFrequency::from_storage(&frequency)
        .map_err(|_| PersistenceError::RecurrenceRange)?;
    let end_kind = RecurrenceEndKind::from_storage(&end_kind)
        .map_err(|_| PersistenceError::RecurrenceRange)?;
    let options = RecurrenceOptions {
        timezone,
        weekdays: values_from_mask(weekdays_mask, 0, 6),
        month_days: values_from_mask(month_days_mask, 1, 31),
        year_months: values_from_mask(year_months_mask, 1, 12),
        end_kind,
        end_count,
        end_until_at_utc_ms,
    };
    let generated = crate::domain::local_recurrence::generate_occurrences(
        frequency,
        interval,
        &options,
        Some(anchor_start_at_utc_ms),
        anchor_end_at_utc_ms,
    )
    .map_err(|_| PersistenceError::RecurrenceRange)?;
    for occurrence in generated {
        let recurrence_key = format!("LOCAL:{}", occurrence.nominal_due_at_utc_ms);
        let cancelled = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM calendar_local_occurrence_tombstones WHERE resource_id=?1 AND nominal_end_at_utc_ms=?2)",
            params![source.resource_id, occurrence.nominal_due_at_utc_ms],
            |row| row.get::<_, bool>(0),
        )?;
        insert_calendar_event_projection(
            transaction,
            source,
            &recurrence_key,
            &event.title,
            event.notes.as_deref(),
            event.location.as_deref(),
            occurrence.nominal_start_at_utc_ms,
            Some(occurrence.nominal_due_at_utc_ms),
            event.all_day,
            cancelled,
            event.status,
            event.unsupported_reason,
            updated_at_ms,
        )?;
        ensure_calendar_work_item(
            transaction,
            source.series_id,
            &recurrence_key,
            updated_at_ms,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_calendar_event_projection(
    transaction: &Transaction<'_>,
    source: ProjectionSource<'_>,
    recurrence_key: &str,
    title: &str,
    notes: Option<&str>,
    location: Option<&str>,
    start_at_utc_ms: Option<i64>,
    end_at_utc_ms: Option<i64>,
    all_day: bool,
    cancelled: bool,
    status: ProjectionStatus,
    unsupported_reason: Option<UnsupportedReason>,
    updated_at_ms: i64,
) -> Result<(), PersistenceError> {
    transaction.execute(
        "INSERT INTO calendar_event_projections(
            series_id,recurrence_key,resource_id,calendar_id,title,notes,location,start_at_utc_ms,end_at_utc_ms,
            all_day,cancelled,projection_status,unsupported_reason,source_etag,updated_at_ms
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
         ON CONFLICT(series_id,recurrence_key) DO UPDATE SET
           resource_id=excluded.resource_id,calendar_id=excluded.calendar_id,title=excluded.title,
           notes=excluded.notes,location=excluded.location,start_at_utc_ms=excluded.start_at_utc_ms,
           end_at_utc_ms=excluded.end_at_utc_ms,all_day=excluded.all_day,cancelled=excluded.cancelled,
           projection_status=excluded.projection_status,unsupported_reason=excluded.unsupported_reason,
           source_etag=excluded.source_etag,updated_at_ms=excluded.updated_at_ms",
        params![
            source.series_id,
            recurrence_key,
            source.resource_id,
            source.calendar_id,
            title,
            notes,
            location,
            start_at_utc_ms,
            end_at_utc_ms,
            all_day,
            cancelled,
            status.as_str(),
            unsupported_reason.map(UnsupportedReason::as_str),
            source.etag,
            updated_at_ms,
        ],
    )?;
    Ok(())
}

fn recurrence_unsupported_reason(error: RecurrenceError) -> UnsupportedReason {
    match error {
        RecurrenceError::InvalidStructure => UnsupportedReason::InvalidStructure,
        RecurrenceError::UnsupportedTime => UnsupportedReason::UnsupportedTime,
        RecurrenceError::InvalidData | RecurrenceError::InvalidWindow => {
            UnsupportedReason::InvalidData
        }
        RecurrenceError::UnsupportedRule | RecurrenceError::ExpansionLimit => {
            UnsupportedReason::UnsupportedRecurrence
        }
    }
}

fn ensure_calendar_work_item(
    transaction: &Transaction<'_>,
    series_id: &str,
    recurrence_key: &str,
    now: i64,
) -> Result<String, PersistenceError> {
    if let Some(work_item_id) = transaction
        .query_row(
            "SELECT work_item_id FROM work_item_state
             WHERE source_kind='calendar_occurrence' AND calendar_series_id=?1 AND recurrence_key=?2",
            params![series_id, recurrence_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        transaction.execute(
            "UPDATE work_item_state SET last_seen_at_ms=?2,remote_deleted_at_ms=NULL WHERE work_item_id=?1",
            params![work_item_id, now],
        )?;
        return Ok(work_item_id);
    }
    let work_item_id = Uuid::new_v4().to_string();
    transaction.execute(
        "INSERT INTO work_item_state(work_item_id,source_kind,calendar_series_id,recurrence_key,last_seen_at_ms)
         VALUES(?1,'calendar_occurrence',?2,?3,?4)",
        params![work_item_id, series_id, recurrence_key, now],
    )?;
    Ok(work_item_id)
}

/// Rebuilds every active series projection from retained raw ICS over the window
/// derived from `now_ms`.
///
/// Only rebuildable `calendar_event_projections` rows are replaced. Occurrence
/// work items, completion, and subtasks are never removed, so an occurrence that
/// leaves the window keeps its local state and reconnects on reappearance.
fn refresh_calendar_event_projections(
    connection: &mut Connection,
    now_ms: i64,
    horizon: RecurrenceHorizon,
) -> Result<CalendarProjectionRefreshSummary, PersistenceError> {
    let window = horizon_window(now_ms, horizon)?;
    let transaction = connection.transaction()?;
    let resources = {
        let mut statement = transaction.prepare(
            "SELECT r.series_id,r.resource_id,r.calendar_id,r.etag,r.raw_ics,r.updated_at_ms,s.uid
             FROM remote_resources r JOIN event_series s ON s.series_id=r.series_id WHERE r.lifecycle='active'
             ORDER BY r.series_id,
                CASE WHEN EXISTS (
                    SELECT 1 FROM calendar_event_projections p
                    WHERE p.series_id=r.series_id AND p.resource_id=r.resource_id
                ) THEN 0 ELSE 1 END,
                r.exact_href,r.resource_id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    transaction.execute(
        "DELETE FROM calendar_event_projections
         WHERE NOT (
             recurrence_key LIKE 'LOCAL:%'
             AND EXISTS(
                 SELECT 1 FROM work_item_state w
                 WHERE w.source_kind='calendar_occurrence'
                   AND w.calendar_series_id=calendar_event_projections.series_id
                   AND w.recurrence_key=calendar_event_projections.recurrence_key
                   AND w.completed=1
             )
         )",
        [],
    )?;
    let mut series_count = 0_u32;
    let mut occurrence_count = 0_u32;
    let mut unsupported_series_count = 0_u32;
    let mut selected_series: Option<String> = None;
    for (series_id, resource_id, calendar_id, etag, raw_ics, updated_at_ms, uid) in resources {
        if selected_series.as_deref() == Some(series_id.as_str()) {
            continue;
        }
        selected_series = Some(series_id.clone());
        let counts = upsert_calendar_event_projection(
            &transaction,
            ProjectionSource {
                series_id: &series_id,
                resource_id: &resource_id,
                calendar_id: &calendar_id,
                etag: &etag,
                raw_ics: &raw_ics,
                uid: &uid,
            },
            updated_at_ms,
            window,
        )?;
        series_count = series_count.saturating_add(1);
        occurrence_count = occurrence_count.saturating_add(counts.occurrences);
        unsupported_series_count =
            unsupported_series_count.saturating_add(counts.unsupported_series);
    }
    // The horizon moves occurrences into the projection window. Doing this inside
    // the same transaction keeps an advancing window from silently introducing
    // overdue reminders that were never scheduled.
    let rebuilt = {
        let mut statement =
            transaction.prepare("SELECT DISTINCT series_id FROM calendar_event_projections")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    reconcile_calendar_notification_series(
        &transaction,
        &rebuilt,
        now_ms,
        NotificationReconcileContext::ScheduleMutation,
    )?;
    transaction.commit()?;
    Ok(CalendarProjectionRefreshSummary {
        window_start_at_utc_ms: window.start_at_utc_ms,
        window_end_at_utc_ms: window.end_at_utc_ms,
        series_count,
        occurrence_count,
        unsupported_series_count,
    })
}

fn list_calendar_settings(
    connection: &Connection,
) -> Result<Vec<CalendarSettingsView>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT c.calendar_id,c.account_id,c.display_name,c.server_color,c.color_override,COALESCE(c.color_override,c.server_color),c.available,c.selected,CASE WHEN c.display_past_unbounded=1 THEN NULL ELSE c.display_past_days END,CASE WHEN c.display_future_unbounded=1 THEN NULL ELSE c.display_future_days END FROM calendars c JOIN accounts a ON a.account_id=c.account_id WHERE a.removed_at_ms IS NULL ORDER BY a.user_label,c.display_name,c.calendar_id"
    )?;
    statement
        .query_map([], |row| {
            Ok(CalendarSettingsView {
                remote_session_ready: false,
                remote_create_eligible: calendar_write_blocked_reason(
                    connection,
                    &row.get::<_, String>(1)?,
                    &row.get::<_, String>(0)?,
                )
                .is_none(),
                remote_create_blocked_reason: calendar_write_blocked_reason(
                    connection,
                    &row.get::<_, String>(1)?,
                    &row.get::<_, String>(0)?,
                ),
                calendar_id: row.get(0)?,
                account_id: row.get(1)?,
                display_name: row.get(2)?,
                server_color: row.get(3)?,
                color_override: row.get(4)?,
                effective_color: row.get(5)?,
                available: row.get(6)?,
                visible: row.get(7)?,
                display_past_days: row.get(8)?,
                display_future_days: row.get(9)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn set_calendar_color_override(
    connection: &Connection,
    calendar_id: &str,
    color: Option<&str>,
) -> Result<(), PersistenceError> {
    if validate_account_id(calendar_id).is_err()
        || color.is_some_and(|value| {
            value.len() != 7
                || !value.starts_with('#')
                || !value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err(PersistenceError::InvalidCalendarColor);
    }
    let changed = connection.execute(
        "UPDATE calendars SET color_override=?2 WHERE calendar_id=?1 AND available=1 AND EXISTS(SELECT 1 FROM accounts a WHERE a.account_id=calendars.account_id AND a.removed_at_ms IS NULL)",
        params![calendar_id, color],
    )?;
    if changed == 0 {
        return Err(PersistenceError::NotFound);
    }
    Ok(())
}

fn list_calendar_agenda_items(
    connection: &Connection,
    display_now_utc_ms: Option<i64>,
) -> Result<Vec<CalendarAgendaItem>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT p.series_id,p.resource_id,p.recurrence_key,p.calendar_id,c.display_name,p.title,p.notes,p.location,
                p.start_at_utc_ms,p.end_at_utc_ms,p.all_day,p.projection_status,p.unsupported_reason,
                w.work_item_id,COALESCE(w.completed,0),w.completed_at_ms,COALESCE(w.important_override,0),COALESCE(c.color_override,c.server_color)
         FROM calendar_event_projections p
         JOIN calendars c ON c.calendar_id=p.calendar_id
         JOIN accounts a ON a.account_id=c.account_id
         JOIN remote_resources r ON r.resource_id=p.resource_id
         LEFT JOIN work_item_state w ON w.source_kind='calendar_occurrence'
              AND w.calendar_series_id=p.series_id AND w.recurrence_key=p.recurrence_key
         WHERE r.lifecycle='active' AND p.cancelled=0 AND a.removed_at_ms IS NULL AND c.selected=1
           AND NOT (p.recurrence_key=?2 AND EXISTS(
               SELECT 1 FROM calendar_resource_recurrence rr
               WHERE rr.resource_id=p.resource_id AND rr.enabled=1
           ))
           AND (EXISTS(SELECT 1 FROM remote_delete_operations d WHERE d.resource_id=p.resource_id AND d.state IN ('pending','conflict')) OR ?1 IS NULL OR p.start_at_utc_ms IS NULL OR p.end_at_utc_ms IS NULL OR
                ((c.display_future_unbounded=1 OR p.start_at_utc_ms < ?1 + c.display_future_days * 86400000) AND
                 (c.display_past_unbounded=1 OR p.start_at_utc_ms >= ?1 - c.display_past_days * 86400000 OR
                  p.end_at_utc_ms > ?1 - c.display_past_days * 86400000)))
         ORDER BY p.start_at_utc_ms IS NULL,p.start_at_utc_ms,p.all_day DESC,p.title,p.series_id,p.recurrence_key",
    )?;
    let mut items = statement
        .query_map(
            params![display_now_utc_ms, SINGLETON_OCCURRENCE_KEY],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, bool>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, bool>(14)?,
                    row.get::<_, Option<i64>>(15)?,
                    row.get::<_, bool>(16)?,
                    row.get::<_, Option<String>>(17)?,
                ))
            },
        )?
        .map(|row| {
            let (
                series_id,
                resource_id,
                recurrence_key,
                calendar_id,
                calendar_name,
                title,
                notes,
                location,
                start_at_utc_ms,
                end_at_utc_ms,
                all_day,
                status,
                unsupported_reason,
                work_item_id,
                completed,
                completed_at_ms,
                important,
                calendar_color,
            ) = row?;
            let status = ProjectionStatus::from_storage(&status).ok_or_else(|| {
                PersistenceError::CalendarSync(
                    crate::domain::calendar_sync::CalendarSyncError::CorruptStoredValue,
                )
            })?;
            let unsupported_reason = unsupported_reason
                .map(|value| {
                    UnsupportedReason::from_storage(&value).ok_or_else(|| {
                        PersistenceError::CalendarSync(
                            crate::domain::calendar_sync::CalendarSyncError::CorruptStoredValue,
                        )
                    })
                })
                .transpose()?;
            let subtasks = work_item_id
                .as_deref()
                .map(|id| load_subtasks(connection, id))
                .transpose()?
                .unwrap_or_default();
            let tags = load_calendar_tags(connection, &resource_id)?;
            let recurrence = load_calendar_recurrence(connection, &resource_id)?;
            let local_recurrence_occurrence = recurrence_key.starts_with("LOCAL:");
            Ok(CalendarAgendaItem {
                remote_edit_eligible: !local_recurrence_occurrence
                    && resource_write_blocked_reason(connection, &resource_id).is_none(),
                remote_edit_blocked_reason: if local_recurrence_occurrence {
                    Some("本机重复实例仅支持本机状态".into())
                } else {
                    resource_write_blocked_reason(connection, &resource_id)
                },
                remote_time_edit_eligible: !local_recurrence_occurrence
                    && resource_time_edit_eligible(connection, &resource_id),
                pending_remote: None,
                series_id,
                resource_id,
                recurrence_key,
                calendar_id,
                calendar_name,
                calendar_color,
                title,
                notes,
                location,
                start_at_utc_ms,
                end_at_utc_ms,
                all_day,
                status,
                unsupported_reason,
                work_item_id,
                completed,
                completed_at_ms,
                important,
                tags,
                recurrence,
                subtasks,
            })
        })
        .collect::<Result<Vec<_>, PersistenceError>>()?;
    overlay_pending_remote_updates(connection, &mut items)?;
    overlay_pending_remote_deletions(connection, &mut items)?;
    append_pending_remote_creations(connection, display_now_utc_ms, &mut items)?;
    items.sort_by(|left, right| {
        left.start_at_utc_ms
            .is_none()
            .cmp(&right.start_at_utc_ms.is_none())
            .then(left.start_at_utc_ms.cmp(&right.start_at_utc_ms))
            .then(right.all_day.cmp(&left.all_day))
            .then(left.title.cmp(&right.title))
            .then(left.series_id.cmp(&right.series_id))
            .then(left.recurrence_key.cmp(&right.recurrence_key))
    });
    Ok(items)
}

fn calendar_write_blocked_reason(c: &Connection, account: &str, calendar: &str) -> Option<String> {
    let flags=c.query_row("SELECT a.enabled,a.credential_configured,a.remote_editing_enabled,c.selected,c.available,c.privileges FROM calendars c JOIN accounts a ON a.account_id=c.account_id WHERE c.calendar_id=?1 AND c.account_id=?2",params![calendar,account],|r|Ok((r.get::<_,bool>(0)?,r.get::<_,bool>(1)?,r.get::<_,bool>(2)?,r.get::<_,bool>(3)?,r.get::<_,bool>(4)?,r.get::<_,String>(5)?))).ok();
    let reason = match flags {
        None => Some("日历当前不可用"),
        Some((false, ..)) => Some("请先启用账户"),
        Some((_, false, ..)) => Some("请先配置账户凭据"),
        Some((_, _, false, ..)) => Some("请在设置中允许远程日历编辑"),
        Some((_, _, _, false, ..)) => Some("请先在设置中显示此日历"),
        Some((_, _, _, _, false, _)) => Some("日历当前不可用"),
        Some((_, _, _, _, _, ref privileges)) if !collection_content_writable(privileges) => {
            Some("此日历没有写入权限")
        }
        _ => None,
    };
    if let Some(reason) = reason {
        return Some(reason.into());
    }
    let route = match scoped_event::selected_calendar_route(c, account, calendar) {
        Ok(route) => route,
        Err(_) => return Some("此账户或日历不支持安全同步".into()),
    };
    let binding = match icloud_binding::load(c, account) {
        Ok(Some(binding)) => binding,
        _ => return Some("请先连接此 iCloud 账户".into()),
    };
    let policy = crate::adapters::caldav_transport::DavTransportPolicy::icloud_bound(
        &binding,
        [route.clone()],
    );
    if !policy.is_ok_and(|p| {
        url::Url::parse(&binding.home_url)
            .is_ok_and(|origin| p.probe_collection_matches(&origin, &route))
    }) {
        return Some("日历地址不符合当前 iCloud 来源策略".into());
    }
    None
}
fn resource_write_blocked_reason(c: &Connection, resource: &str) -> Option<String> {
    let row=c.query_row("SELECT c.account_id,r.calendar_id,r.etag,r.raw_ics,r.exact_href,c.collection_url,s.uid FROM remote_resources r JOIN calendars c ON c.calendar_id=r.calendar_id JOIN event_series s ON s.series_id=r.series_id WHERE r.resource_id=?1 AND r.lifecycle='active' AND length(r.raw_ics) BETWEEN 1 AND 4194304",[resource],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Vec<u8>>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?))).ok();
    let Some((account, calendar, etag, raw, href, route, uid)) = row else {
        return Some("此事件仅可查看".into());
    };
    if let Some(reason) = calendar_write_blocked_reason(c, &account, &calendar) {
        return Some(reason);
    }
    if !strong_mutation_etag(&etag)
        || !resource_belongs_to_collection(&route, &href)
        || crate::domain::remote_event_update::validate_event_base_v3(&raw, &uid, false).is_err()
    {
        return Some("邀请、重复、全天或不支持的事件仅可查看".into());
    }
    let pending=c.query_row("SELECT EXISTS(SELECT 1 FROM outbox_operations WHERE resource_id=?1 AND state='pending') OR EXISTS(SELECT 1 FROM remote_delete_operations WHERE resource_id=?1 AND state='pending') OR EXISTS(SELECT 1 FROM remote_create_operations WHERE resource_id=?1 AND state='pending')",[resource],|r|r.get::<_,bool>(0)).unwrap_or(true);
    if pending {
        return Some("此事件有待完成的同步操作".into());
    }
    None
}

fn resource_time_edit_eligible(c: &Connection, resource: &str) -> bool {
    c.query_row(
        "SELECT r.raw_ics,s.uid FROM remote_resources r JOIN event_series s ON s.series_id=r.series_id WHERE r.resource_id=?1 AND r.lifecycle='active'",
        [resource],
        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
    )
    .is_ok_and(|(raw, uid)| {
        crate::domain::remote_event_update::validate_event_base_v3(&raw, &uid, true).is_ok()
    })
}
fn get_active_calendar_item(
    connection: &Connection,
    series_id: &str,
    recurrence_key: &str,
) -> Result<CalendarAgendaItem, PersistenceError> {
    list_calendar_agenda_items(connection, None)?
        .into_iter()
        .find(|item| item.series_id == series_id && item.recurrence_key == recurrence_key)
        .ok_or(PersistenceError::NotFound)
}

fn active_calendar_work_item(
    transaction: &Transaction<'_>,
    series_id: &str,
    recurrence_key: &str,
) -> Result<String, PersistenceError> {
    transaction
        .query_row(
            "SELECT w.work_item_id FROM work_item_state w
             JOIN calendar_event_projections p ON p.series_id=w.calendar_series_id AND p.recurrence_key=w.recurrence_key
             JOIN remote_resources r ON r.resource_id=p.resource_id
             WHERE w.source_kind='calendar_occurrence' AND w.calendar_series_id=?1
               AND w.recurrence_key=?2 AND p.projection_status='ready'
               AND p.cancelled=0 AND r.lifecycle='active'",
            params![series_id, recurrence_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)
}

fn active_calendar_metadata_work_item(
    transaction: &Transaction<'_>,
    series_id: &str,
    recurrence_key: &str,
) -> Result<String, PersistenceError> {
    transaction
        .query_row(
            "SELECT w.work_item_id FROM work_item_state w
             JOIN calendar_event_projections p ON p.series_id=w.calendar_series_id AND p.recurrence_key=w.recurrence_key
             JOIN remote_resources r ON r.resource_id=p.resource_id
             WHERE w.source_kind='calendar_occurrence' AND w.calendar_series_id=?1
               AND w.recurrence_key=?2 AND p.cancelled=0 AND r.lifecycle='active'",
            params![series_id, recurrence_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)
}

fn calendar_subtask_work_item(
    transaction: &Transaction<'_>,
    series_id: &str,
    recurrence_key: &str,
    subtask_id: &str,
) -> Result<String, PersistenceError> {
    let work_item_id = active_calendar_work_item(transaction, series_id, recurrence_key)?;
    transaction
        .query_row(
            "SELECT work_item_id FROM subtasks WHERE subtask_id=?1 AND work_item_id=?2",
            params![subtask_id, work_item_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)
}

fn set_calendar_completed(
    connection: &mut Connection,
    series_id: &str,
    recurrence_key: &str,
    completed: bool,
) -> Result<CalendarAgendaItem, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let work_item_id = active_calendar_work_item(&transaction, series_id, recurrence_key)?;
    transaction.execute(
        "UPDATE work_item_state SET completed=?1,completed_at_ms=CASE WHEN ?1 THEN ?2 ELSE NULL END,last_seen_at_ms=?2
         WHERE work_item_id=?3",
        params![completed, now, work_item_id],
    )?;
    transaction.commit()?;
    get_active_calendar_item(connection, series_id, recurrence_key)
}

fn set_calendar_important(
    connection: &mut Connection,
    series_id: &str,
    recurrence_key: &str,
    important: bool,
) -> Result<CalendarAgendaItem, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let work_item_id = active_calendar_metadata_work_item(&transaction, series_id, recurrence_key)?;
    transaction.execute(
        "UPDATE work_item_state SET important_override=?1,last_seen_at_ms=?2 WHERE work_item_id=?3",
        params![important, now, work_item_id],
    )?;
    transaction.commit()?;
    get_active_calendar_item(connection, series_id, recurrence_key)
}

fn update_calendar_local_metadata(
    connection: &mut Connection,
    input: UpdateCalendarLocalMetadata,
) -> Result<(), PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    update_calendar_local_metadata_in(&transaction, input, now)?;
    transaction.commit()?;
    Ok(())
}

fn update_calendar_local_metadata_in(
    transaction: &Transaction<'_>,
    input: UpdateCalendarLocalMetadata,
    now: i64,
) -> Result<(), PersistenceError> {
    let input = input
        .validate()
        .map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    let identities = {
        let mut statement = transaction.prepare(
            "SELECT p.series_id,p.recurrence_key FROM calendar_event_projections p JOIN remote_resources r ON r.resource_id=p.resource_id WHERE p.resource_id=?1 AND p.cancelled=0 AND r.lifecycle='active' LIMIT 2",
        )?;
        statement
            .query_map([&input.resource_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<(String, String)>, _>>()?
    };
    if identities.is_empty()
        && !transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_create_operations WHERE resource_id=?1 AND state IN ('pending','applied'))",
            [&input.resource_id],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Err(PersistenceError::NotFound);
    }
    let mut important_target = if input.important.is_some() {
        if identities.len() != 1 {
            return Err(PersistenceError::NotFound);
        }
        Some(identities[0].clone())
    } else {
        None
    };
    // Local recurrence metadata adds, changes and removes `LOCAL:*` projections
    // for this resource, so its reminders are rebuilt in the same transaction.
    let mut affected_series = identities
        .iter()
        .map(|(series_id, _)| series_id.clone())
        .collect::<std::collections::BTreeSet<_>>();

    transaction.execute(
        "DELETE FROM calendar_resource_tags WHERE resource_id=?1",
        [&input.resource_id],
    )?;
    for (sort_order, name) in input.tag_names.iter().enumerate() {
        let normalized = normalize_tag_name(name);
        let tag_id = transaction
            .query_row(
                "SELECT tag_id FROM tags WHERE normalized_name=?1",
                [&normalized],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        transaction.execute(
            "INSERT INTO tags(tag_id,name,normalized_name,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?4) ON CONFLICT(normalized_name) DO UPDATE SET name=excluded.name,updated_at_ms=excluded.updated_at_ms",
            params![tag_id, name, normalized, now],
        )?;
        let stable_tag_id: String = transaction.query_row(
            "SELECT tag_id FROM tags WHERE normalized_name=?1",
            [&normalized],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO calendar_resource_tags(resource_id,tag_id,sort_order) VALUES(?1,?2,?3)",
            params![input.resource_id, stable_tag_id, sort_order as i64],
        )?;
    }
    if transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM calendar_resource_recurrence WHERE resource_id=?1 AND pending_operation_id IS NOT NULL)",
        [&input.resource_id],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::PendingMutationExists);
    }
    if let Some(recurrence) = input.recurrence {
        let binding = transaction
            .query_row(
                "SELECT a.account_id,p.calendar_id,p.series_id,s.uid,p.start_at_utc_ms,p.end_at_utc_ms
                 FROM calendar_event_projections p
                 JOIN remote_resources r ON r.resource_id=p.resource_id AND r.lifecycle='active' AND r.parse_status='parsed'
                 JOIN event_series s ON s.series_id=p.series_id
                 JOIN calendars c ON c.calendar_id=p.calendar_id
                 JOIN accounts a ON a.account_id=c.account_id AND a.account_id=s.account_id
                 WHERE p.resource_id=?1 AND p.recurrence_key=?2 AND p.cancelled=0
                   AND p.projection_status='ready' AND p.start_at_utc_ms IS NOT NULL AND p.end_at_utc_ms IS NOT NULL",
                params![input.resource_id, SINGLETON_OCCURRENCE_KEY],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or(PersistenceError::RemoteMutationReadOnly)?;
        if binding.4 >= binding.5 {
            return Err(PersistenceError::RecurrenceRange);
        }
        if important_target.is_some() {
            important_target = Some((binding.2.clone(), format!("LOCAL:{}", binding.5)));
        }
        affected_series.insert(binding.2.clone());
        transaction.execute(
            "INSERT INTO calendar_resource_recurrence(
                resource_id,frequency,interval_value,timezone,weekdays_mask,month_days_mask,year_months_mask,
                end_kind,end_count,end_until_at_utc_ms,account_id,calendar_id,series_id,uid,generation,enabled,
                anchor_start_at_utc_ms,anchor_end_at_utc_ms,cursor_start_at_utc_ms,cursor_end_at_utc_ms,
                created_at_ms,updated_at_ms
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,1,1,?15,?16,?15,?16,?17,?17)
             ON CONFLICT(resource_id) DO UPDATE SET
                frequency=excluded.frequency,interval_value=excluded.interval_value,timezone=excluded.timezone,
                weekdays_mask=excluded.weekdays_mask,month_days_mask=excluded.month_days_mask,
                year_months_mask=excluded.year_months_mask,end_kind=excluded.end_kind,end_count=excluded.end_count,
                end_until_at_utc_ms=excluded.end_until_at_utc_ms,account_id=excluded.account_id,
                calendar_id=excluded.calendar_id,series_id=excluded.series_id,uid=excluded.uid,
                generation=calendar_resource_recurrence.generation+1,enabled=1,
                anchor_start_at_utc_ms=calendar_resource_recurrence.anchor_start_at_utc_ms,
                anchor_end_at_utc_ms=calendar_resource_recurrence.anchor_end_at_utc_ms,
                cursor_start_at_utc_ms=calendar_resource_recurrence.cursor_start_at_utc_ms,
                cursor_end_at_utc_ms=calendar_resource_recurrence.cursor_end_at_utc_ms,
                pending_operation_id=NULL,pending_idempotency_key=NULL,
                pending_expected_start_at_utc_ms=NULL,pending_expected_end_at_utc_ms=NULL,
                pending_next_start_at_utc_ms=NULL,pending_next_end_at_utc_ms=NULL,
                updated_at_ms=excluded.updated_at_ms",
            params![
                input.resource_id,
                recurrence.frequency.as_str(),
                recurrence.interval,
                recurrence.options.timezone,
                values_mask(&recurrence.options.weekdays, 0),
                values_mask(&recurrence.options.month_days, 1),
                values_mask(&recurrence.options.year_months, 1),
                recurrence.options.end_kind.as_str(),
                recurrence.options.end_count,
                recurrence.options.end_until_at_utc_ms,
                binding.0,
                binding.1,
                binding.2,
                binding.3,
                binding.4,
                binding.5,
                now
            ],
        )?;
    } else {
        transaction.execute(
            "DELETE FROM calendar_resource_recurrence WHERE resource_id=?1",
            [&input.resource_id],
        )?;
    }
    // Keep the visible agenda in sync with local recurrence metadata without
    // waiting for the next network refresh. Existing completed work items are
    // retained; unfinished projections are tombstoned before regeneration.
    transaction.execute(
        "UPDATE calendar_event_projections SET cancelled=1,updated_at_ms=?2 WHERE resource_id=?1 AND recurrence_key<>?3 AND recurrence_key LIKE 'LOCAL:%' AND cancelled=0 AND NOT EXISTS(SELECT 1 FROM work_item_state w WHERE w.source_kind='calendar_occurrence' AND w.calendar_series_id=calendar_event_projections.series_id AND w.recurrence_key=calendar_event_projections.recurrence_key AND w.completed=1)",
        params![input.resource_id, now, SINGLETON_OCCURRENCE_KEY],
    )?;
    materialize_resource_local_recurrence(&transaction, &input.resource_id, now)?;
    if let (Some(important), Some((series_id, recurrence_key))) =
        (input.important, important_target)
    {
        let work_item_id =
            active_calendar_metadata_work_item(&transaction, &series_id, &recurrence_key)?;
        transaction.execute(
            "UPDATE work_item_state SET important_override=?1,last_seen_at_ms=?2 WHERE work_item_id=?3",
            params![important, now, work_item_id],
        )?;
    }
    for series_id in &affected_series {
        reconcile_calendar_notification_jobs(
            transaction,
            series_id,
            now,
            NotificationReconcileContext::ScheduleMutation,
        )?;
    }
    Ok(())
}

fn remote_recurrence_targets(
    connection: &Connection,
) -> Result<Vec<RemoteRecurrenceTarget>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT rr.resource_id,rr.account_id,c.collection_url,rr.cursor_end_at_utc_ms,
                rr.pending_operation_id IS NOT NULL
         FROM calendar_resource_recurrence rr
         JOIN calendars c ON c.calendar_id=rr.calendar_id AND c.account_id=rr.account_id
         JOIN accounts a ON a.account_id=rr.account_id
         JOIN remote_resources r ON r.resource_id=rr.resource_id AND r.lifecycle='active'
         WHERE rr.enabled=1
           AND a.enabled=1 AND a.removed_at_ms IS NULL AND a.remote_editing_enabled=1
           AND c.available=1 AND c.selected=1
           AND (rr.pending_operation_id IS NULL OR EXISTS(
                SELECT 1 FROM outbox_operations o
                WHERE o.operation_id=rr.pending_operation_id
                  AND o.operation_kind='update_event' AND o.state='pending'))
         ORDER BY rr.cursor_end_at_utc_ms,rr.resource_id",
    )?;
    statement
        .query_map([], |row| {
            Ok(RemoteRecurrenceTarget {
                resource_id: row.get(0)?,
                account_id: row.get(1)?,
                collection_route: row.get(2)?,
                deadline_at_utc_ms: row.get(3)?,
                pending: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(PersistenceError::from)
}

fn admit_remote_recurrence_advance(
    connection: &mut Connection,
    resource_id: &str,
    actor: std::sync::Weak<()>,
    now: i64,
    connected: &scoped_event::ConnectedOwnerCheck,
) -> Result<scoped_event::ScopedEventSnapshot, PersistenceError> {
    // Resource identities are opaque provider-bound keys. They are not
    // account UUIDs and must be validated by the exact binding query below.
    if resource_id.trim().is_empty() {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    if actor.upgrade().is_none() {
        return Err(PersistenceError::ActorUnavailable);
    }
    let transaction = connection.transaction()?;
    type Binding = (
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        Option<String>,
    );
    let binding: Binding = transaction
        .query_row(
            "SELECT rr.account_id,rr.calendar_id,rr.series_id,rr.uid,rr.generation,
                    rr.cursor_start_at_utc_ms,rr.cursor_end_at_utc_ms,rr.pending_operation_id
             FROM calendar_resource_recurrence rr
             JOIN remote_resources r ON r.resource_id=rr.resource_id AND r.series_id=rr.series_id
                 AND r.calendar_id=rr.calendar_id AND r.lifecycle='active'
             JOIN event_series s ON s.series_id=rr.series_id AND s.account_id=rr.account_id AND s.uid=rr.uid
             JOIN accounts a ON a.account_id=rr.account_id
             JOIN calendars c ON c.calendar_id=rr.calendar_id AND c.account_id=rr.account_id
             WHERE rr.resource_id=?1 AND rr.enabled=1
               AND a.enabled=1 AND a.removed_at_ms IS NULL AND a.remote_editing_enabled=1
               AND c.available=1 AND c.selected=1",
            [resource_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::RemoteMutationReadOnly)?;
    let route: String = transaction.query_row(
        "SELECT collection_url FROM calendars WHERE calendar_id=?1 AND account_id=?2",
        params![binding.1, binding.0],
        |row| row.get(0),
    )?;
    if !connected(&binding.0, &route) {
        return Err(PersistenceError::RemoteAccountConnectionRequired);
    }
    if let Some(operation_id) = binding.7 {
        transaction.commit()?;
        return scoped_event::restore(
            connection,
            (
                crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3,
                &operation_id,
                &binding.0,
                &binding.1,
                resource_id,
            ),
            actor,
            Some(connected),
        );
    }
    let cursor_key = format!("LOCAL:{}", binding.6);
    let completed = transaction.query_row(
        "SELECT COALESCE((SELECT completed FROM work_item_state
                         WHERE source_kind='calendar_occurrence' AND calendar_series_id=?1 AND recurrence_key=?2),0)",
        params![binding.2, cursor_key],
        |row| row.get::<_, bool>(0),
    )?;
    // The cursor advances to the nearest future generated instance. Completion
    // of the current cursor never relaxes that bound; a past instance may not
    // be written back to the remote event.
    if !completed && binding.6 > now {
        return Err(PersistenceError::NotFound);
    }
    let current: (i64, i64) = transaction
        .query_row(
            "SELECT start_at_utc_ms,end_at_utc_ms FROM calendar_event_projections
             WHERE resource_id=?1 AND series_id=?2 AND recurrence_key=?3
               AND cancelled=0 AND projection_status='ready'",
            params![resource_id, binding.2, SINGLETON_OCCURRENCE_KEY],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or(PersistenceError::RemoteMutationReadOnly)?;
    if current != (binding.5, binding.6) {
        return Err(PersistenceError::StaleSyncCapture);
    }
    let next: (i64, i64) = transaction
        .query_row(
            "SELECT start_at_utc_ms,end_at_utc_ms FROM calendar_event_projections
             WHERE resource_id=?1 AND series_id=?2 AND recurrence_key LIKE 'LOCAL:%'
               AND cancelled=0 AND projection_status='ready' AND end_at_utc_ms>?3
               AND start_at_utc_ms>=?4
             ORDER BY start_at_utc_ms,end_at_utc_ms,recurrence_key LIMIT 1",
            params![resource_id, binding.2, binding.6, now],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    let fields = crate::domain::remote_event_update::RemoteEventUpdateFieldsV3 {
        title: None,
        description: None,
        location: None,
        time: Some(crate::domain::remote_event_update::UtcEventTimePair {
            start_at_utc_ms: next.0,
            end_at_utc_ms: next.1,
        }),
    };
    let intent = crate::domain::remote_event_update::encode_event_intent_v3(&fields)
        .map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    let intent = String::from_utf8(intent).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    let idempotency_key = Uuid::new_v4().to_string();
    let operation = remote_update_fields::queue_encoded_in(
        &transaction,
        idempotency_key.clone(),
        resource_id.to_owned(),
        intent,
        remote_update_fields::UpdateKind::EventV3,
        now,
    )?;
    let changed = transaction.execute(
        "UPDATE calendar_resource_recurrence
         SET pending_operation_id=?2,pending_idempotency_key=?3,
             pending_expected_start_at_utc_ms=?4,pending_expected_end_at_utc_ms=?5,
             pending_next_start_at_utc_ms=?6,pending_next_end_at_utc_ms=?7,updated_at_ms=?8
         WHERE resource_id=?1 AND enabled=1 AND generation=?9 AND pending_operation_id IS NULL",
        params![
            resource_id,
            operation.operation_id,
            idempotency_key,
            binding.5,
            binding.6,
            next.0,
            next.1,
            now,
            binding.4
        ],
    )?;
    if changed != 1 {
        return Err(PersistenceError::StaleSyncCapture);
    }
    transaction.commit()?;
    scoped_event::restore(
        connection,
        (
            crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3,
            &operation.operation_id,
            &binding.0,
            &binding.1,
            resource_id,
        ),
        actor,
        Some(connected),
    )
}

fn materialize_resource_local_recurrence(
    transaction: &Transaction<'_>,
    resource_id: &str,
    updated_at_ms: i64,
) -> Result<(), PersistenceError> {
    let row = transaction
        .query_row(
            "SELECT p.series_id,p.calendar_id,p.title,p.notes,p.location,p.start_at_utc_ms,p.end_at_utc_ms,p.all_day,p.projection_status,p.unsupported_reason,r.etag,r.raw_ics,s.uid FROM calendar_event_projections p JOIN remote_resources r ON r.resource_id=p.resource_id JOIN event_series s ON s.series_id=p.series_id WHERE p.resource_id=?1 AND p.recurrence_key=?2 AND p.cancelled=0 AND r.lifecycle='active'",
            params![resource_id, SINGLETON_OCCURRENCE_KEY],
            |row| Ok((
                row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?, row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?, row.get::<_, bool>(7)?,
                row.get::<_, String>(8)?, row.get::<_, Option<String>>(9)?,
                row.get::<_, String>(10)?, row.get::<_, Vec<u8>>(11)?,
                row.get::<_, String>(12)?,
            )),
        )
        .optional()?;
    let Some((
        series_id,
        calendar_id,
        title,
        notes,
        location,
        start_at_utc_ms,
        end_at_utc_ms,
        all_day,
        status,
        unsupported_reason,
        etag,
        raw_ics,
        uid,
    )) = row
    else {
        return Ok(());
    };
    if status != ProjectionStatus::Ready.as_str() || unsupported_reason.is_some() {
        return Ok(());
    }
    let (Some(anchor_start), Some(anchor_due)) = (start_at_utc_ms, end_at_utc_ms) else {
        return Ok(());
    };
    let source = ProjectionSource {
        series_id: &series_id,
        resource_id,
        calendar_id: &calendar_id,
        etag: &etag,
        raw_ics: &raw_ics,
        uid: &uid,
    };
    let event = NormalizedCalendarEvent {
        title,
        notes,
        location,
        start_at_utc_ms: Some(anchor_start),
        end_at_utc_ms: Some(anchor_due),
        all_day,
        cancelled: false,
        status: ProjectionStatus::Ready,
        unsupported_reason: None,
    };
    materialize_local_calendar_recurrence(transaction, source, &event, updated_at_ms)
}

fn create_calendar_subtask(
    connection: &mut Connection,
    input: CreateCalendarSubtask,
) -> Result<CalendarAgendaItem, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let work_item_id =
        active_calendar_work_item(&transaction, &input.series_id, &input.recurrence_key)?;
    let sort_order: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(sort_order)+1,0) FROM subtasks WHERE work_item_id=?1",
        [&work_item_id],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO subtasks(subtask_id,work_item_id,text,sort_order,created_at_ms,updated_at_ms)
         VALUES(?1,?2,?3,?4,?5,?5)",
        params![
            Uuid::new_v4().to_string(),
            work_item_id,
            input.text,
            sort_order,
            now
        ],
    )?;
    transaction.execute(
        "UPDATE work_item_state SET last_seen_at_ms=?2 WHERE work_item_id=?1",
        params![work_item_id, now],
    )?;
    transaction.commit()?;
    get_active_calendar_item(connection, &input.series_id, &input.recurrence_key)
}

fn update_calendar_subtask(
    connection: &mut Connection,
    input: UpdateCalendarSubtask,
) -> Result<CalendarAgendaItem, PersistenceError> {
    mutate_calendar_subtask(
        connection,
        &input.series_id,
        &input.recurrence_key,
        &input.subtask_id,
        |transaction, work_item_id, now| {
            transaction.execute(
                "UPDATE subtasks SET text=?1,updated_at_ms=?2 WHERE subtask_id=?3 AND work_item_id=?4",
                params![input.text, now, input.subtask_id, work_item_id],
            )
        },
    )
}

fn set_calendar_subtask_completed(
    connection: &mut Connection,
    series_id: &str,
    recurrence_key: &str,
    subtask_id: &str,
    completed: bool,
) -> Result<CalendarAgendaItem, PersistenceError> {
    mutate_calendar_subtask(
        connection,
        series_id,
        recurrence_key,
        subtask_id,
        |transaction, work_item_id, now| {
            transaction.execute(
                "UPDATE subtasks SET completed=?1,updated_at_ms=?2 WHERE subtask_id=?3 AND work_item_id=?4",
                params![completed, now, subtask_id, work_item_id],
            )
        },
    )
}

fn mutate_calendar_subtask(
    connection: &mut Connection,
    series_id: &str,
    recurrence_key: &str,
    subtask_id: &str,
    mutate: impl FnOnce(&Transaction<'_>, &str, i64) -> rusqlite::Result<usize>,
) -> Result<CalendarAgendaItem, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let work_item_id =
        calendar_subtask_work_item(&transaction, series_id, recurrence_key, subtask_id)?;
    if mutate(&transaction, &work_item_id, now)? != 1 {
        return Err(PersistenceError::NotFound);
    }
    transaction.execute(
        "UPDATE work_item_state SET last_seen_at_ms=?2 WHERE work_item_id=?1",
        params![work_item_id, now],
    )?;
    transaction.commit()?;
    get_active_calendar_item(connection, series_id, recurrence_key)
}

fn delete_calendar_subtask(
    connection: &mut Connection,
    series_id: &str,
    recurrence_key: &str,
    subtask_id: &str,
) -> Result<CalendarAgendaItem, PersistenceError> {
    let now = unix_time_ms()?;
    let transaction = connection.transaction()?;
    let work_item_id =
        calendar_subtask_work_item(&transaction, series_id, recurrence_key, subtask_id)?;
    let sort_order = transaction.query_row(
        "SELECT sort_order FROM subtasks WHERE subtask_id=?1 AND work_item_id=?2",
        params![subtask_id, work_item_id],
        |row| row.get::<_, i64>(0),
    )?;
    transaction.execute(
        "DELETE FROM subtasks WHERE subtask_id=?1 AND work_item_id=?2",
        params![subtask_id, work_item_id],
    )?;
    transaction.execute(
        "UPDATE subtasks SET sort_order=sort_order-1 WHERE work_item_id=?1 AND sort_order>?2",
        params![work_item_id, sort_order],
    )?;
    transaction.execute(
        "UPDATE work_item_state SET last_seen_at_ms=?2 WHERE work_item_id=?1",
        params![work_item_id, now],
    )?;
    transaction.commit()?;
    get_active_calendar_item(connection, series_id, recurrence_key)
}

fn reconcile_deletion_evidence(
    connection: &mut Connection,
    reconciliation: AccountDeletionReconciliation,
) -> Result<(), PersistenceError> {
    let reconciliation = reconciliation.validate()?;
    let transaction = connection.transaction()?;
    let account_usable = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=?1 AND enabled=1 AND removed_at_ms IS NULL)",
        [&reconciliation.account_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !account_usable {
        return Err(PersistenceError::AccountDisabled);
    }
    let mut statement = transaction.prepare(
        "SELECT r.resource_id FROM remote_resources r
         JOIN calendars c ON c.calendar_id=r.calendar_id
         WHERE c.account_id=?1 AND r.lifecycle='missing_candidate'
         ORDER BY r.resource_id",
    )?;
    let candidates = statement
        .query_map([&reconciliation.account_id], |row| row.get::<_, String>(0))?
        .collect::<Result<std::collections::HashSet<_>, _>>()?;
    drop(statement);
    let probed = reconciliation
        .probes
        .iter()
        .map(|probe| probe.resource_id.clone())
        .collect::<std::collections::HashSet<_>>();
    if candidates.is_empty()
        || candidates != probed
        || candidates.len() != reconciliation.probes.len()
    {
        return Err(PersistenceError::UnsafeDeletionBarrier);
    }

    let mut confirmed_series = HashMap::<String, i64>::new();
    for probe in reconciliation.probes {
        let (calendar_id, series_id, calendar_available, candidate_evidence_valid, collection_href, resource_href) = transaction
            .query_row(
                "SELECT r.calendar_id,r.series_id,c.available,r.candidate_evidence_valid,c.collection_url,r.exact_href FROM remote_resources r
                 JOIN calendars c ON c.calendar_id=r.calendar_id
                 WHERE r.resource_id=?1 AND c.account_id=?2 AND r.lifecycle='missing_candidate'",
                params![probe.resource_id, reconciliation.account_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or(PersistenceError::UnsafeDeletionBarrier)?;
        if probe.status.is_positive_absence()
            && (!calendar_available
                || !candidate_evidence_valid
                || !resource_belongs_to_collection(&collection_href, &resource_href))
        {
            return Err(PersistenceError::UnsafeDeletionBarrier);
        }
        if resource_has_inflight_mutation(&transaction, &probe.resource_id)? {
            return Err(PersistenceError::UnsafeDeletionBarrier);
        }
        let replacement_found = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM remote_resources replacement
                JOIN calendars c ON c.calendar_id=replacement.calendar_id
                WHERE replacement.series_id=?1 AND replacement.resource_id<>?2
                  AND replacement.lifecycle='active' AND c.account_id=?3
             )",
            params![series_id, probe.resource_id, reconciliation.account_id],
            |row| row.get::<_, bool>(0),
        )?;
        let confirmed = probe.status.is_positive_absence() && !replacement_found;
        if confirmed {
            transaction.execute(
                "UPDATE remote_resources SET lifecycle='remote_deleted',remote_deleted_at_ms=?2,updated_at_ms=?2
                 WHERE resource_id=?1 AND lifecycle='missing_candidate'",
                params![probe.resource_id, probe.observed_at_ms],
            )?;
            transaction.execute(
                "DELETE FROM calendar_event_projections WHERE resource_id=?1",
                [&probe.resource_id],
            )?;
            confirmed_series
                .entry(series_id)
                .and_modify(|observed_at_ms| {
                    *observed_at_ms = (*observed_at_ms).max(probe.observed_at_ms);
                })
                .or_insert(probe.observed_at_ms);
        }
        transaction.execute(
            "INSERT INTO deletion_evidence(
                evidence_id,resource_id,account_id,calendar_id,probe_status,
                replacement_search_complete,replacement_found,account_barrier_complete,
                decision,observed_at_ms,created_at_ms
             ) VALUES(?1,?2,?3,?4,?5,1,?6,1,?7,?8,?8)",
            params![
                Uuid::new_v4().to_string(),
                probe.resource_id,
                reconciliation.account_id,
                calendar_id,
                probe.status.as_str(),
                replacement_found,
                if confirmed {
                    "confirmed_deleted"
                } else {
                    "retained"
                },
                probe.observed_at_ms,
            ],
        )?;
    }
    for (series_id, observed_at_ms) in &confirmed_series {
        let unresolved = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM remote_resources r
                JOIN calendars c ON c.calendar_id=r.calendar_id
                WHERE r.series_id=?1 AND c.account_id=?2
                  AND r.lifecycle IN ('active','missing_candidate')
            )",
            params![series_id, reconciliation.account_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !unresolved {
            transaction.execute(
                "UPDATE work_item_state SET remote_deleted_at_ms=?2,last_seen_at_ms=?2
                 WHERE source_kind='calendar_occurrence' AND calendar_series_id=?1",
                params![series_id, observed_at_ms],
            )?;
        }
    }
    // Confirmed deletion is a schedule mutation: the projections were removed
    // above, so reminders that no longer have a surviving alias must not stay
    // claimable while the series keeps its durable identity.
    for (series_id, observed_at_ms) in &confirmed_series {
        reconcile_calendar_notification_jobs(
            &transaction,
            series_id,
            *observed_at_ms,
            NotificationReconcileContext::ScheduleMutation,
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// Physically reclaim the raw cache of resources whose remote deletion was
/// confirmed at least `DELETED_RAW_CACHE_RETENTION_MS` before `now_ms`.
///
/// Only `remote_deleted` rows are eligible, and never `active` or
/// `missing_candidate` ones, so absence from a failed or bounded inventory
/// still cannot cause data loss. Series identity, href aliases, completion, and
/// subtasks are not touched: a later reappearance of the same series reconnects
/// to the same local state. Evidence rows cascade with their resource because
/// they describe a resource that no longer exists locally. Mutation history
/// holds a restrictive resource reference and must remain outside the batch.
fn purge_expired_deleted_raw_cache(
    connection: &mut Connection,
    now_ms: i64,
) -> Result<u32, PersistenceError> {
    let cutoff = admitted_now_ms(now_ms)?.saturating_sub(DELETED_RAW_CACHE_RETENTION_MS);
    let transaction = connection.transaction()?;
    let purged = transaction.execute(
        "DELETE FROM remote_resources
         WHERE resource_id IN (
             SELECT resource_id FROM remote_resources
             WHERE lifecycle='remote_deleted'
               AND remote_deleted_at_ms IS NOT NULL
               AND remote_deleted_at_ms <= ?1
               AND NOT EXISTS (
                   SELECT 1 FROM outbox_operations o
                   WHERE o.resource_id=remote_resources.resource_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM remote_delete_operations d
                   WHERE d.resource_id=remote_resources.resource_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM remote_create_operations c
                   WHERE c.resource_id=remote_resources.resource_id
                      OR (c.calendar_id=remote_resources.calendar_id AND c.exact_href=remote_resources.exact_href)
               )
             ORDER BY remote_deleted_at_ms, resource_id
             LIMIT ?2
         )",
        params![cutoff, DELETED_RAW_CACHE_PURGE_LIMIT],
    )?;
    transaction.commit()?;
    Ok(u32::try_from(purged).unwrap_or(u32::MAX))
}

fn calendar_sync_token(
    connection: &Connection,
    calendar_id: &str,
) -> Result<Option<String>, PersistenceError> {
    connection
        .query_row(
            "SELECT committed_sync_token FROM calendar_sync_state WHERE calendar_id=?1",
            [calendar_id],
            |row| row.get(0),
        )
        .optional()
        .map(|value| value.flatten())
        .map_err(Into::into)
}

fn record_sync_run(
    connection: &mut Connection,
    account_id: String,
    trigger: SyncTrigger,
    outcome: SyncOutcome,
    failure_class: SyncFailureClass,
    calendar_count: u32,
    resource_count: u32,
) -> Result<SyncRunSummary, PersistenceError> {
    let now = unix_time_ms()?;
    let summary = SyncRunSummary {
        run_id: Uuid::new_v4().to_string(),
        account_id,
        trigger,
        outcome,
        failure_class,
        calendar_count,
        resource_count,
        started_at_ms: now,
        finished_at_ms: now,
    };
    connection.execute(
        "INSERT INTO sync_runs(run_id,account_id,trigger_kind,outcome,failure_class,calendar_count,resource_count,started_at_ms,finished_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![summary.run_id, summary.account_id, summary.trigger.as_str(), summary.outcome.as_str(), summary.failure_class.as_str(), summary.calendar_count, summary.resource_count, summary.started_at_ms, summary.finished_at_ms],
    )?;
    Ok(summary)
}

fn list_sync_runs(connection: &Connection) -> Result<Vec<SyncRunSummary>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT run_id,account_id,trigger_kind,outcome,failure_class,calendar_count,resource_count,started_at_ms,finished_at_ms FROM sync_runs ORDER BY started_at_ms,run_id",
    )?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, u32>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })?
        .map(|row| {
            let (
                run_id,
                account_id,
                trigger,
                outcome,
                failure_class,
                calendar_count,
                resource_count,
                started_at_ms,
                finished_at_ms,
            ) = row?;
            Ok(SyncRunSummary {
                run_id,
                account_id,
                trigger: SyncTrigger::from_storage(&trigger)?,
                outcome: SyncOutcome::from_storage(&outcome)?,
                failure_class: SyncFailureClass::from_storage(&failure_class)?,
                calendar_count,
                resource_count,
                started_at_ms,
                finished_at_ms,
            })
        })
        .collect()
}

fn backup(connection: &Connection, destination: &Path) -> Result<(), PersistenceError> {
    if destination.exists() {
        return Err(PersistenceError::BackupTargetExists);
    }
    let owner_guard = db_owner::DatabaseOwner::acquire_vacant(destination)
        .map_err(PersistenceError::DatabaseOwnership)?;
    connection.backup(MAIN_DB, owner_guard.path(), None)?;
    Ok(())
}

fn unix_time_ms() -> Result<i64, PersistenceError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PersistenceError::Clock)?
        .as_millis();
    i64::try_from(millis).map_err(|_| PersistenceError::Clock)
}

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[cfg(feature = "dev-test")]
    #[error("development Probe prerequisite failed")]
    DevProbePrerequisite(&'static str),
    #[error("connect the iCloud account before changing its remote calendar")]
    RemoteAccountConnectionRequired,
    #[error("scoped event retry requires a newer committed remote revision")]
    ScopedEventRetryBlocked,
    #[error("database ownership prerequisite failed")]
    DatabaseOwnership(#[source] DatabaseOwnerFailure),
    #[error("SQLite operation failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database migration failed")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("database actor is unavailable")]
    ActorUnavailable,
    #[error("local TODO was not found")]
    NotFound,
    #[error("local subtask move is unavailable")]
    SubtaskMoveUnavailable,
    #[error("local subtask neighbor changed")]
    StaleSubtaskOrder,
    #[error("local subtask order is invalid")]
    InvalidSubtaskOrder,
    #[error("desktop item order is invalid")]
    InvalidDesktopOrder,
    #[error("calendar color override is invalid")]
    InvalidCalendarColor,
    #[error("system clock is outside the supported range")]
    Clock,
    #[error("backup target already exists")]
    BackupTargetExists,
    #[error("fixed recurrence is outside the supported range")]
    RecurrenceRange,
    #[error("recurrence with occurrence history cannot be disabled")]
    RecurrenceHasHistory,
    #[error("stored Local TODO is invalid")]
    Domain(#[from] crate::domain::local_todo::LocalTodoError),
    #[error("stored application settings are invalid")]
    Settings(#[from] crate::domain::settings::SettingsError),
    #[error("stored window placement is invalid")]
    WindowPlacement(#[from] crate::domain::window_placement::WindowPlacementError),
    #[error("window label does not own a persisted placement")]
    InvalidWindowLabel,
    #[error("stored account profile is invalid")]
    Account(#[from] crate::domain::account::AccountError),
    #[error("account is disabled")]
    AccountDisabled,
    #[error("calendar sync stage is incomplete or unsafe")]
    UnsafeSyncStage,
    #[error("calendar sync stage predates a committed cache change")]
    StaleSyncCapture,
    #[error("stored calendar sync data is invalid")]
    CalendarSync(#[from] crate::domain::calendar_sync::CalendarSyncError),
    #[error("calendar sync identity conflicts with committed state")]
    IdentityConflict,
    #[error("calendar sync token does not match committed state")]
    SyncTokenConflict,
    #[error("remote mutation input or stored state is invalid")]
    RemoteMutation(#[from] crate::domain::remote_mutation::RemoteMutationError),
    #[error("remote resource is not safely writable")]
    RemoteMutationReadOnly,
    #[error("a pending remote mutation already owns this resource")]
    PendingMutationExists,
    #[error("idempotency key belongs to different remote intent")]
    IdempotencyConflict,
    #[error("remote mutation claim is no longer current")]
    OutboxClaimLost,
    #[error("remote mutation read-back does not prove the intended result")]
    RemoteReadBackMismatch,
    #[error("remote mutation conflict was already resolved differently")]
    ConflictAlreadyResolved,
    #[error("deletion reconciliation barrier is incomplete or unsafe")]
    UnsafeDeletionBarrier,
}

/// Actor-minted only after independently validated terminal transaction commit.
pub(crate) struct CommittedEventScheduleChange {
    actor: std::sync::Weak<()>,
    request: std::sync::Weak<()>,
    operation: String,
    account: String,
    changed: bool,
}
impl CommittedEventScheduleChange {
    pub(crate) fn matches_request(&self, request: &std::sync::Weak<()>) -> bool {
        !self.operation.is_empty()
            && self.request.upgrade().is_some()
            && std::sync::Weak::ptr_eq(&self.request, request)
    }
    pub(crate) fn matches_actor(&self, actor: &std::sync::Weak<()>) -> bool {
        self.actor.upgrade().is_some() && std::sync::Weak::ptr_eq(&self.actor, actor)
    }
    pub(crate) fn changed_for(&self, account: &str) -> bool {
        self.changed && self.account == account
    }
}

#[cfg(test)]
#[path = "persistence/tzid_projection_tests.rs"]
mod tzid_projection_tests;

#[cfg(test)]
mod reminder_admission_tests;
