//! Process-session presentation only. No actor, transport, timer or execution authority.
use crate::{
    application::remote_crud::ConfirmedEventView,
    domain::{
        calendar_event::CalendarAgendaItem,
        local_todo::{LocalTodo, SaveLocalTodo},
    },
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Mutex};
use uuid::Uuid;
pub const EVENT: &str = "calendar-todo://agenda-presentation-changed";
const MAX_UNRESOLVED: usize = 32;
#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CreateSource {
    Local,
    Remote,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Recovery {
    pub presentation_id: String,
    pub revision: u64,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BeginCreate {
    pub source: CreateSource,
    pub draft: SaveLocalTodo,
    pub calendar_name: String,
    pub calendar_color: Option<String>,
    pub recovery: Option<Recovery>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentedCreate {
    pub presentation_id: String,
    pub revision: u64,
    pub source: CreateSource,
    pub owner: String,
    pub draft: SaveLocalTodo,
    pub calendar_name: String,
    pub calendar_color: Option<String>,
    pub phase: &'static str,
    pub failure: Option<&'static str>,
    pub feedback: u64,
    pub reconciliation_ready: bool,
    pub local_item: Option<LocalTodo>,
    pub remote: Option<ConfirmedEventView>,
    #[serde(skip)]
    pub key: String,
    observed: BTreeSet<String>,
    #[serde(skip)]
    metadata_claimed: bool,
    #[serde(skip)]
    metadata_finished: bool,
    #[serde(skip)]
    metadata_retrying: bool,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentedDelete {
    pub deletion_id: String,
    pub owner: String,
    pub item: CalendarAgendaItem,
    pub expires_at_ms: i64,
    pub phase: &'static str,
    pub failure: Option<&'static str>,
    pub remote: Option<ConfirmedEventView>,
    pub feedback: u64,
    #[serde(skip)]
    pub key: String,
}
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub version: u64,
    pub creates: Vec<PresentedCreate>,
    pub remote_deletes: Vec<PresentedDelete>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteExpiry {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    pub remaining_ms: Option<i64>,
}
#[derive(Default)]
pub struct AgendaPresentationState {
    current: Mutex<Snapshot>,
}
fn exact(a: &ConfirmedEventView, b: &ConfirmedEventView) -> bool {
    a.receipt.kind == b.receipt.kind
        && a.receipt.account_id == b.receipt.account_id
        && a.receipt.calendar_id == b.receipt.calendar_id
        && a.receipt.resource_id == b.receipt.resource_id
        && a.receipt.operation_id == b.receipt.operation_id
}
pub fn applied(view: &ConfirmedEventView) -> bool {
    view.operation
        .as_ref()
        .is_some_and(|op| op.state == crate::domain::remote_mutation::RemoteMutationState::Applied)
}
pub fn failed(view: &ConfirmedEventView) -> bool {
    view.failure.is_some()
        || view.operation.as_ref().is_some_and(|op| {
            matches!(
                op.state,
                crate::domain::remote_mutation::RemoteMutationState::Conflict
                    | crate::domain::remote_mutation::RemoteMutationState::Cancelled
            )
        })
        || view.runtime_state.is_some_and(|s| {
            matches!(
                s,
                crate::application::remote_mutation_runtime::MutationRuntimeState::Deferred
                    | crate::application::remote_mutation_runtime::MutationRuntimeState::Failed
                    | crate::application::remote_mutation_runtime::MutationRuntimeState::Conflict
                    | crate::application::remote_mutation_runtime::MutationRuntimeState::Cancelled
            )
        })
}
fn delete_handoff(view: &ConfirmedEventView) -> bool {
    use crate::domain::remote_mutation::{ConflictResolutionState, RemoteMutationState};
    view.operation.as_ref().is_some_and(|op| {
        op.state == RemoteMutationState::Cancelled
            && op.resolution_state == Some(ConflictResolutionState::KeepRemote)
            || op.resolution_state == Some(ConflictResolutionState::RetryLocal)
                && op
                    .retry_operation_id
                    .as_ref()
                    .is_some_and(|id| !id.is_empty() && id != &op.operation_id)
    })
}
fn same_remote_request(a: &SaveLocalTodo, b: &SaveLocalTodo) -> bool {
    a.calendar_id == b.calendar_id
        && a.title == b.title
        && a.notes == b.notes
        && a.location == b.location
        && a.start_at_utc_ms == b.start_at_utc_ms
        && a.due_at_utc_ms == b.due_at_utc_ms
}
impl AgendaPresentationState {
    pub fn get(&self) -> Snapshot {
        self.current.lock().expect("agenda presentation").clone()
    }
    pub fn begin(
        &self,
        input: BeginCreate,
        owner: String,
        name: String,
        color: Option<String>,
    ) -> Result<(Snapshot, PresentedCreate), ()> {
        let mut state = self.current.lock().map_err(|_| ())?;
        let mut record = if let Some(recovery) = input.recovery {
            let old = state
                .creates
                .iter()
                .find(|c| {
                    c.presentation_id == recovery.presentation_id
                        && c.revision == recovery.revision
                        && c.phase == "failed"
                })
                .ok_or(())?;
            let mut next = old.clone();
            if old.source != input.source || !same_remote_request(&old.draft, &input.draft) {
                next.key = Uuid::new_v4().to_string();
            }
            next.revision += 1;
            next
        } else {
            if state.creates.len() >= MAX_UNRESOLVED {
                return Err(());
            }
            PresentedCreate {
                presentation_id: Uuid::new_v4().to_string(),
                revision: 1,
                source: input.source,
                owner: owner.clone(),
                draft: input.draft.clone(),
                calendar_name: name.clone(),
                calendar_color: color.clone(),
                phase: "submitting",
                failure: None,
                feedback: 0,
                reconciliation_ready: false,
                local_item: None,
                remote: None,
                key: Uuid::new_v4().to_string(),
                observed: BTreeSet::new(),
                metadata_claimed: false,
                metadata_finished: false,
                metadata_retrying: false,
            }
        };
        record.source = input.source;
        record.owner = owner;
        record.draft = input.draft;
        record.calendar_name = name;
        record.calendar_color = color;
        record.phase = "submitting";
        record.reconciliation_ready = false;
        record.failure = None;
        record.local_item = None;
        record.remote = None;
        record.observed.clear();
        record.metadata_claimed = false;
        record.metadata_finished = false;
        record.metadata_retrying = false;
        state
            .creates
            .retain(|c| c.presentation_id != record.presentation_id);
        state.creates.push(record.clone());
        state.version += 1;
        Ok((state.clone(), record))
    }
    pub fn local_result(&self, id: &str, revision: u64, item: Option<LocalTodo>) {
        let mut state = self.current.lock().expect("agenda presentation");
        if let Some(c) = state
            .creates
            .iter_mut()
            .find(|c| c.presentation_id == id && c.revision == revision && c.phase == "submitting")
        {
            c.phase = if item.is_some() {
                "committed"
            } else {
                "failed"
            };
            c.failure = if item.is_some() {
                None
            } else {
                Some("localPersistence")
            };
            c.reconciliation_ready = item.is_some();
            c.local_item = item;
            c.feedback += 1;
            state.version += 1;
        }
    }
    pub fn remote_result(&self, id: &str, revision: u64, view: Option<ConfirmedEventView>) {
        let mut state = self.current.lock().expect("agenda presentation");
        if let Some(c) = state
            .creates
            .iter_mut()
            .find(|c| c.presentation_id == id && c.revision == revision && c.phase == "submitting")
        {
            c.phase = if view.is_some() {
                "committed"
            } else {
                "failed"
            };
            c.failure = if view.is_some() {
                None
            } else {
                Some("remoteAdmission")
            };
            c.remote = view;
            c.feedback += 1;
            state.version += 1;
        }
    }
    /// Each view observes its replacement or deliberately excludes it after successful initialization.
    pub fn observe(&self, owner: &str, ids: &[Recovery]) {
        if !matches!(owner, "main" | "desktop") {
            return;
        }
        let mut state = self.current.lock().expect("agenda presentation");
        let mut changed = false;
        for c in &mut state.creates {
            if c.phase == "committed"
                && c.failure.is_none()
                && ids
                    .iter()
                    .any(|id| id.presentation_id == c.presentation_id && id.revision == c.revision)
                && (c.source == CreateSource::Local || c.metadata_finished)
            {
                changed |= c.observed.insert(owner.to_string());
            }
        }
        state.creates.retain(|c| c.observed.len() < 2);
        if changed {
            state.version += 1;
        }
    }
    pub fn stage_delete(
        &self,
        item: CalendarAgendaItem,
        owner: String,
        now: i64,
    ) -> Result<Snapshot, ()> {
        let mut state = self.current.lock().map_err(|_| ())?;
        if state
            .remote_deletes
            .iter()
            .any(|d| d.item.resource_id == item.resource_id && d.phase != "failed")
        {
            return Ok(state.clone());
        }
        if state.remote_deletes.len() >= MAX_UNRESOLVED
            && !state
                .remote_deletes
                .iter()
                .any(|d| d.item.resource_id == item.resource_id)
        {
            return Err(());
        }
        let key = state
            .remote_deletes
            .iter()
            .find(|d| d.item.resource_id == item.resource_id)
            .map(|d| d.key.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        state
            .remote_deletes
            .retain(|d| d.item.resource_id != item.resource_id);
        state.remote_deletes.push(PresentedDelete {
            deletion_id: Uuid::new_v4().to_string(),
            owner,
            item,
            expires_at_ms: now.checked_add(5000).ok_or(())?,
            phase: "undoable",
            failure: None,
            remote: None,
            feedback: 0,
            key,
        });
        state.version += 1;
        Ok(state.clone())
    }
    pub fn undo_delete(&self, id: &str, now: i64) -> Snapshot {
        let mut state = self.current.lock().expect("agenda presentation");
        let size = state.remote_deletes.len();
        state
            .remote_deletes
            .retain(|d| d.deletion_id != id || d.phase != "undoable" || now >= d.expires_at_ms);
        if size != state.remote_deletes.len() {
            state.version += 1;
        }
        state.clone()
    }
    pub fn expire_delete(&self, id: &str, now: i64) -> Option<PresentedDelete> {
        let mut state = self.current.lock().expect("agenda presentation");
        let d = state
            .remote_deletes
            .iter_mut()
            .find(|d| d.deletion_id == id && d.phase == "undoable" && now >= d.expires_at_ms)?;
        d.phase = "admitting";
        let result = d.clone();
        state.version += 1;
        Some(result)
    }
    pub fn delete_result(&self, id: &str, view: Option<ConfirmedEventView>) {
        let mut state = self.current.lock().expect("agenda presentation");
        if let Some(d) = state
            .remote_deletes
            .iter_mut()
            .find(|d| d.deletion_id == id && d.phase == "admitting")
        {
            d.phase = if view.is_some() { "admitted" } else { "failed" };
            d.failure = if view.is_some() {
                None
            } else {
                Some("remoteAdmission")
            };
            d.remote = view;
            if d.failure.is_some() {
                d.feedback += 1;
            }
            state.version += 1;
        }
    }
    /// Called outside actor/transport callbacks. Returns metadata work claimed exactly once.
    pub fn remote_changed(
        &self,
        view: &ConfirmedEventView,
    ) -> Vec<(String, u64, SaveLocalTodo, String)> {
        let mut state = self.current.lock().expect("agenda presentation");
        let mut jobs = Vec::new();
        let mut changed = false;
        for c in &mut state.creates {
            if c.remote
                .as_ref()
                .is_none_or(|old| !exact(old, view) || applied(old) && c.metadata_claimed)
            {
                continue;
            }
            if failed(view) && c.failure.is_none() {
                c.failure = Some("remoteTerminal");
                c.feedback += 1;
            }
            if applied(view) && c.failure == Some("remoteTerminal") {
                c.failure = None;
            }
            if applied(view) && !c.metadata_claimed {
                c.metadata_claimed = true;
                if let Some(resource) = &view.receipt.resource_id {
                    jobs.push((
                        c.presentation_id.clone(),
                        c.revision,
                        c.draft.clone(),
                        resource.clone(),
                    ));
                }
            }
            c.remote = Some(view.clone());
            changed = true;
        }
        for d in &mut state.remote_deletes {
            if d.remote
                .as_ref()
                .is_none_or(|old| !exact(old, view) || applied(old))
            {
                continue;
            }
            if failed(view) && !delete_handoff(view) && d.failure.is_none() {
                d.failure = Some("remoteTerminal");
                d.feedback += 1;
            }
            d.remote = Some(view.clone());
            changed = true;
        }
        // A retry successor may have outlived its transient DELETE ticket. Its typed
        // Applied view still proves deletion of this exact account/calendar/resource.
        let before = state.creates.len();
        let delete_proven = view.receipt.kind == crate::domain::remote_crud::RemoteCrudKind::Delete
            && applied(view)
            && view.operation.as_ref().is_some_and(|operation| {
                operation.operation_id == view.receipt.operation_id
                    && Some(&operation.calendar_id) == view.receipt.calendar_id.as_ref()
                    && Some(&operation.resource_id) == view.receipt.resource_id.as_ref()
            });
        if delete_proven {
            state.creates.retain(|c| {
                !c.remote.as_ref().is_some_and(|created| {
                    created.receipt.kind == crate::domain::remote_crud::RemoteCrudKind::CreateUtcV2
                        && created.receipt.account_id == view.receipt.account_id
                        && created.receipt.calendar_id == view.receipt.calendar_id
                        && created.receipt.resource_id == view.receipt.resource_id
                })
            });
        }
        changed |= state.creates.len() != before;
        // Retire on exact deletion proof or an explicit actor handoff, never list absence.
        let before = state.remote_deletes.len();
        state.remote_deletes.retain(|d| {
            !d.remote
                .as_ref()
                .is_some_and(|v| applied(v) || delete_handoff(v))
        });
        changed |= state.remote_deletes.len() != before;
        if changed {
            state.version += 1;
        }
        jobs
    }
    pub fn retire_local_deleted(&self, todo_id: &str, occurrence_key: Option<&str>) {
        let mut state = self.current.lock().expect("agenda presentation");
        let before = state.creates.len();
        state.creates.retain(|c| {
            !c.local_item.as_ref().is_some_and(|item| {
                item.todo_id == todo_id
                    && occurrence_key.is_none_or(|key| key == item.occurrence_key)
            })
        });
        if state.creates.len() != before {
            state.version += 1;
        }
    }
    pub fn retry_metadata(&self, id: &Recovery) -> Option<PresentedCreate> {
        let mut state = self.current.lock().expect("agenda presentation");
        let c = state.creates.iter_mut().find(|c| {
            c.presentation_id == id.presentation_id
                && c.revision == id.revision
                && c.failure == Some("localMetadata")
                && c.metadata_finished
                && c.remote
                    .as_ref()
                    .is_some_and(|v| applied(v) && v.receipt.resource_id.is_some())
        })?;
        c.failure = None;
        c.metadata_finished = false;
        c.reconciliation_ready = false;
        c.metadata_retrying = true;
        let result = c.clone();
        state.version += 1;
        Some(result)
    }
    pub fn metadata_result(&self, id: &str, revision: u64, succeeded: bool) {
        let mut state = self.current.lock().expect("agenda presentation");
        if let Some(c) = state
            .creates
            .iter_mut()
            .find(|c| c.presentation_id == id && c.revision == revision)
        {
            c.metadata_finished = true;
            c.reconciliation_ready = true;
            if !succeeded {
                c.failure = Some("localMetadata");
            }
            if !succeeded || c.metadata_retrying {
                c.feedback += 1;
            }
            c.metadata_retrying = false;
            state.version += 1;
        }
    }
}
