use super::*;
use crate::application::agenda_presentation::{
    self as presentation, BeginCreate, CreateSource, Recovery, Snapshot,
};

pub(super) fn unavailable() -> IpcError {
    IpcError {
        code: "presentationUnavailable",
        message: "未能保存操作，输入仍保留。",
    }
}
pub(super) fn publish(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        let _ = app.emit(presentation::EVENT, state.agenda_presentation.get());
    }
}
fn log_failure(app: &AppHandle, action: &str, code: &str, diagnostic: &str) {
    if let Ok(directory) = app.path().app_log_dir() {
        let _ = crate::adapters::error_log::append(&directory, action, code, diagnostic);
    }
}
fn raw_create(record: &presentation::PresentedCreate) -> String {
    let d = &record.draft;
    let mut input = serde_json::json!({"idempotencyKey":record.key,"calendarId":d.calendar_id,"title":d.title,"startAtUtcMs":d.start_at_utc_ms,"endAtUtcMs":d.due_at_utc_ms,"confirmation":"confirmed"});
    if let Some(notes) = &d.notes {
        input["description"] = notes.clone().into();
    }
    if let Some(location) = &d.location {
        input["location"] = location.clone().into();
    }
    serde_json::json!({"action":"confirm","kind":"createUtcV2","input":input}).to_string()
}
fn raw_delete(key: &str, resource: &str) -> String {
    serde_json::json!({"action":"confirm","kind":"delete","input":{"idempotencyKey":key,"resourceId":resource,"confirmation":"confirmed"}}).to_string()
}

fn validate_create(owner: &str, mut input: BeginCreate) -> Result<BeginCreate, IpcError> {
    if !matches!(owner, "main" | "desktop")
        || input.draft.todo_id.is_some()
        || input.draft.selected_occurrence_key.is_some()
    {
        return Err(unavailable());
    }
    input.draft = input.draft.validate().map_err(|_| unavailable())?;
    // Display labels are bounded presentation only; the background services derive
    // and revalidate the actual calendar. Staging never waits on the SQLite actor.
    if input.calendar_name.chars().count() > 512
        || input.calendar_color.as_ref().is_some_and(|c| c.len() > 64)
    {
        return Err(unavailable());
    }
    if input.source == CreateSource::Remote {
        let d = &input.draft;
        let validated = crate::domain::remote_creation::QueueRemoteEventCreateV2 {
            idempotency_key: Uuid::new_v4().to_string(),
            calendar_id: d.calendar_id.clone(),
            title: d.title.clone(),
            description: d.notes.clone(),
            location: d.location.clone(),
            start_at_utc_ms: d.start_at_utc_ms.ok_or_else(unavailable)?,
            end_at_utc_ms: d.due_at_utc_ms.ok_or_else(unavailable)?,
        }
        .validate()
        .map_err(|_| unavailable())?;
        input.draft.notes = validated.description;
    }
    Ok(input)
}

pub(super) fn begin(
    app: AppHandle,
    state: &AppState,
    owner: String,
    input: BeginCreate,
) -> Result<Snapshot, IpcError> {
    let input = validate_create(&owner, input)?;
    let name = input.calendar_name.clone();
    let color = input.calendar_color.clone();
    let (snapshot, record) = state
        .agenda_presentation
        .begin(input, owner, name, color)
        .map_err(|_| unavailable())?;
    let local = state.local_todos.clone();
    let continuation = ConfirmedEventContinuation::capture(state);
    publish(&app);
    tauri::async_runtime::spawn_blocking(move || {
        match record.source {
            CreateSource::Local => {
                let result = local.save(record.draft.clone());
                if let Some(state) = app.try_state::<AppState>() {
                    if result.is_err() {
                        log_failure(
                            &app,
                            "saveAction-event",
                            "localSave",
                            "presented local creation persistence failed",
                        );
                    }
                    state.agenda_presentation.local_result(
                        &record.presentation_id,
                        record.revision,
                        result.ok(),
                    );
                    let _ = state.refresh_reminders();
                }
                let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
            }
            CreateSource::Remote => {
                let result = continuation.run(&raw_create(&record));
                if let Some(state) = app.try_state::<AppState>() {
                    if let Err(error) = &result {
                        log_failure(
                            &app,
                            "saveAction-event",
                            "remoteAdmission",
                            &format!("stage={};reason={}", error.stage, error.reason),
                        );
                    }
                    state.agenda_presentation.remote_result(
                        &record.presentation_id,
                        record.revision,
                        result.as_ref().ok().cloned(),
                    );
                }
                if let Ok(view) = result {
                    settle_remote(&app, &continuation, view);
                }
                let _ = app.emit("calendar-todo://agenda-data-changed", ());
            }
        }
        publish(&app);
    });
    Ok(snapshot)
}

/// Bind first, then take one exact status read to close terminal-before-receipt races.
fn settle_remote(
    app: &AppHandle,
    continuation: &ConfirmedEventContinuation,
    view: crate::application::remote_crud::ConfirmedEventView,
) {
    remote_changed(app, &view);
    let scope = &view.receipt;
    if let Ok(latest) = continuation.run(&serde_json::json!({"action":"status","input":{"kind":scope.kind,"operationId":scope.operation_id,"accountId":scope.account_id,"calendarId":scope.calendar_id,"resourceId":scope.resource_id}}).to_string()) { remote_changed(app, &latest); }
}

pub(super) fn remote_changed(
    app: &AppHandle,
    view: &crate::application::remote_crud::ConfirmedEventView,
) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let before = state.agenda_presentation.get();
    let jobs = state.agenda_presentation.remote_changed(view);
    let after = state.agenda_presentation.get();
    for c in &after.creates {
        if c.failure == Some("remoteTerminal")
            && before
                .creates
                .iter()
                .any(|old| old.presentation_id == c.presentation_id && old.feedback < c.feedback)
        {
            log_failure(
                app,
                "saveAction-event",
                "remoteBackgroundSync",
                "exact presented create requires recovery",
            );
        }
    }
    for d in &after.remote_deletes {
        if d.failure == Some("remoteTerminal")
            && before
                .remote_deletes
                .iter()
                .any(|old| old.deletion_id == d.deletion_id && old.feedback < d.feedback)
        {
            log_failure(
                app,
                "deleteAction-event",
                "remoteBackgroundSync",
                "exact presented deletion requires recovery",
            );
        }
    }
    for (id, revision, draft, resource_id) in jobs {
        start_metadata(app.clone(), id, revision, draft, resource_id);
    }
    if before.version != after.version {
        publish(app);
    }
}

fn start_metadata(
    app: AppHandle,
    id: String,
    revision: u64,
    draft: crate::domain::local_todo::SaveLocalTodo,
    resource_id: String,
) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let agenda = state.agenda.clone();
    // Never call the SQLite actor from inside the mutation emitter's stack/locks.
    tauri::async_runtime::spawn_blocking(move || {
        let result = if draft.important || !draft.tag_names.is_empty() || draft.recurrence.is_some()
        {
            agenda.update_local_metadata(
                crate::domain::calendar_event::UpdateCalendarLocalMetadata {
                    resource_id,
                    tag_names: draft.tag_names,
                    recurrence: draft.recurrence.map(|r| {
                        crate::domain::calendar_event::CalendarFixedRecurrenceInput {
                            frequency: r.frequency,
                            interval: r.interval,
                            options: r.options,
                        }
                    }),
                    important: Some(draft.important),
                },
            )
        } else {
            Ok(())
        };
        if result.is_err() {
            log_failure(
                &app,
                "saveAction-event",
                "localMetadata",
                "presented create local metadata persistence failed",
            );
        }
        if let Some(state) = app.try_state::<AppState>() {
            state
                .agenda_presentation
                .metadata_result(&id, revision, result.is_ok());
            if result.is_ok() {
                let _ = state.refresh_reminders();
                state.trigger_remote_recurrence_change();
            }
        }
        let _ = app.emit("calendar-todo://agenda-data-changed", ());
        publish(&app);
    });
}

pub(super) fn retry_metadata(
    app: &AppHandle,
    state: &AppState,
    identity: Recovery,
) -> Result<Snapshot, IpcError> {
    let record = state
        .agenda_presentation
        .retry_metadata(&identity)
        .ok_or_else(unavailable)?;
    let resource = record
        .remote
        .as_ref()
        .and_then(|v| v.receipt.resource_id.clone())
        .expect("Applied create has an exact resource");
    publish(app);
    start_metadata(
        app.clone(),
        record.presentation_id,
        record.revision,
        record.draft,
        resource,
    );
    Ok(state.agenda_presentation.get())
}

pub(super) fn stage_delete(
    app: &AppHandle,
    state: &AppState,
    owner: String,
    resource: String,
) -> Result<Snapshot, IpcError> {
    if !matches!(owner.as_str(), "main" | "desktop") {
        return Err(unavailable());
    }
    let existing = state.agenda_presentation.get();
    if existing
        .remote_deletes
        .iter()
        .any(|d| d.item.resource_id == resource && d.phase == "undoable")
    {
        return Ok(existing);
    }
    // Read-only qualification; intent and connection remain deferred until expiry.
    state
        .remote_crud
        .confirmed_target(&raw_delete(&Uuid::new_v4().to_string(), &resource))
        .map_err(|_| unavailable())?;
    let mut candidates = state
        .desktop_order_db
        .list_calendar_agenda_items()
        .map_err(|_| unavailable())?
        .into_iter()
        .filter(|item| item.resource_id == resource && item.pending_remote.is_none());
    let item = candidates.next().ok_or_else(unavailable)?;
    if candidates.next().is_some() {
        return Err(unavailable());
    }
    let snapshot = state
        .agenda_presentation
        .stage_delete(
            item,
            owner,
            state.clock.now_utc_ms().map_err(|_| unavailable())?,
        )
        .map_err(|_| unavailable())?;
    publish(app);
    Ok(snapshot)
}
pub(super) fn expire(
    app: AppHandle,
    state: &AppState,
    id: String,
) -> Result<presentation::DeleteExpiry, IpcError> {
    let now = state.clock.now_utc_ms().map_err(|_| unavailable())?;
    if let Some(record) = state.agenda_presentation.expire_delete(&id, now) {
        let continuation = ConfirmedEventContinuation::capture(state);
        publish(&app);
        tauri::async_runtime::spawn_blocking(move || {
            let result = continuation.run(&raw_delete(&record.key, &record.item.resource_id));
            if let Some(state) = app.try_state::<AppState>() {
                if let Err(error) = &result {
                    log_failure(
                        &app,
                        "deleteAction-event",
                        "remoteAdmission",
                        &format!("stage={};reason={}", error.stage, error.reason),
                    );
                }
                state
                    .agenda_presentation
                    .delete_result(&record.deletion_id, result.as_ref().ok().cloned());
            }
            if let Ok(view) = result {
                settle_remote(&app, &continuation, view);
            }
            let _ = app.emit("calendar-todo://agenda-data-changed", ());
            publish(&app);
        });
    }
    let snapshot = state.agenda_presentation.get();
    let remaining_ms = snapshot
        .remote_deletes
        .iter()
        .find(|d| d.deletion_id == id && d.phase == "undoable")
        .map(|d| d.expires_at_ms.saturating_sub(now).max(0));
    Ok(presentation::DeleteExpiry {
        snapshot,
        remaining_ms,
    })
}
pub(super) fn observe(
    app: &AppHandle,
    state: &AppState,
    owner: &str,
    ids: &[Recovery],
) -> Snapshot {
    state.agenda_presentation.observe(owner, ids);
    publish(app);
    state.agenda_presentation.get()
}
