//! Regression tests for reminder admission.
//!
//! These exercise the durable `notification_jobs` decisions directly against an
//! in-memory database, so they cover the first-import, external-edit, restart and
//! catch-up boundaries a manual UI check cannot pin down.

use super::*;
use crate::domain::calendar_event::UpdateCalendarLocalMetadata;
use crate::domain::calendar_sync::{
    AccountDeletionReconciliation, CalendarIncrementalStage, CalendarInventoryStage, DeletionProbe,
    DirectProbeStatus, StagedResource,
};

const ACCOUNT: &str = "11111111-1111-4111-8111-111111111111";
const CALENDAR: &str = "22222222-2222-4222-8222-222222222222";
const COLLECTION: &str = "https://cal.example/22222222-2222-4222-8222-222222222222/";
const HREF: &str = "/22222222-2222-4222-8222-222222222222/series.ics";
const UID: &str = "series-uid@example";
const NOW: i64 = 1_700_000_000_000;
const HOUR: i64 = 3_600_000;

fn ics(start_ms: i64, alarm: Option<&str>) -> Vec<u8> {
    let mut body = format!(
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{UID}\r\nDTSTAMP:20240101T000000Z\r\nDTSTART:{}\r\nDTEND:{}\r\nSUMMARY:Test\r\n",
        stamp(start_ms),
        stamp(start_ms + HOUR)
    );
    if let Some(trigger) = alarm {
        body.push_str(&format!(
            "BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:{trigger}\r\nDESCRIPTION:x\r\nEND:VALARM\r\n"
        ));
    }
    body.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
    body.into_bytes()
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn fixture() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    migrations().to_latest(&mut conn).unwrap();
    conn.execute(
        "INSERT INTO accounts(account_id,provider_kind,user_label,credential_ref,region_profile,enabled,created_at_ms,updated_at_ms)
         VALUES(?1,'caldav','test','calendar-todo/account/11111111-1111-4111-8111-111111111111','icloud-cn',1,?2,?2)",
        params![ACCOUNT, NOW],
    )
    .unwrap();
    conn
}

fn stage(start_ms: i64, alarm: Option<&str>, etag: &str) -> AccountInventoryStage {
    AccountInventoryStage {
        account_id: ACCOUNT.into(),
        provider_kind: "caldav".into(),
        user_label: "test".into(),
        credential_ref: format!("calendar-todo/account/{ACCOUNT}"),
        region_profile: "icloud-cn".into(),
        complete: true,
        authoritative: true,
        observed_at_ms: NOW,
        calendars: vec![CalendarInventoryStage {
            calendar_id: CALENDAR.into(),
            collection_url: COLLECTION.into(),
            display_name: "Cal".into(),
            server_color: None,
            privileges: "{}".into(),
            sync_token: Some("token-1".into()),
            resources: vec![StagedResource {
                href: HREF.into(),
                etag: etag.into(),
                content_type: "text/calendar".into(),
                raw_ics: ics(start_ms, alarm),
                uid: UID.into(),
                parse_status: ParseStatus::Parsed,
            }],
        }],
    }
}

fn stage_at(
    start_ms: i64,
    alarm: Option<&str>,
    etag: &str,
    observed_at_ms: i64,
) -> AccountInventoryStage {
    let mut staged = stage(start_ms, alarm, etag);
    staged.observed_at_ms = observed_at_ms;
    staged
}

fn jobs(conn: &Connection) -> Vec<(String, i64, String)> {
    let mut stmt = conn
        .prepare("SELECT occurrence_key,fire_at_utc_ms,state FROM notification_jobs ORDER BY fire_at_utc_ms,state")
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn states(conn: &Connection) -> Vec<String> {
    jobs(conn).into_iter().map(|row| row.2).collect()
}

fn delivery_count(conn: &mut Connection) -> usize {
    claim_due_notification_jobs(conn, NOW).unwrap().len()
}

#[test]
fn first_import_of_overdue_events_schedules_nothing_deliverable() {
    for offset in [-60_000, -14 * 60_000, -900_000, -24 * HOUR] {
        let mut conn = fixture();
        promote_account_inventory(&mut conn, stage(NOW + offset, None, "\"e1\""), NOW).unwrap();
        assert_eq!(states(&conn), vec!["suppressed"], "offset {offset}");
        assert_eq!(delivery_count(&mut conn), 0, "offset {offset}");
    }
}

#[test]
fn first_import_of_future_event_is_planned_and_delivered_once() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    assert_eq!(states(&conn), vec!["planned"]);
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW).unwrap().len(),
        0
    );
    let due = claim_due_notification_jobs(&mut conn, NOW + HOUR).unwrap();
    assert_eq!(due.len(), 1);
    finish_notification_job(&mut conn, &due[0].job_id, true, NOW + HOUR).unwrap();
    assert_eq!(states(&conn), vec!["delivered"]);
}

#[test]
fn previously_planned_fire_keeps_bounded_catch_up() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    // Startup/resume after the fire expired by 20 minutes.
    reconcile_notification_jobs(&mut conn, NOW + HOUR + 20 * 60_000).unwrap();
    let due = claim_due_notification_jobs(&mut conn, NOW + HOUR + 20 * 60_000).unwrap();
    assert_eq!(due.len(), 1);
    assert!(due[0].catch_up);
}

#[test]
fn external_edit_into_the_past_does_not_revive_an_overdue_fire() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    // External client moves the event to one hour ago; ordinary sync applies it.
    let incremental = AccountIncrementalStage {
        account_id: ACCOUNT.into(),
        complete: true,
        authoritative: true,
        observed_at_ms: NOW,
        calendars: vec![CalendarIncrementalStage {
            calendar_id: CALENDAR.into(),
            base_sync_token: "token-1".into(),
            new_sync_token: "token-2".into(),
            changed_resources: vec![StagedResource {
                href: HREF.into(),
                etag: "\"e2\"".into(),
                content_type: "text/calendar".into(),
                raw_ics: ics(NOW - HOUR, None),
                uid: UID.into(),
                parse_status: ParseStatus::Parsed,
            }],
            removed_hrefs: vec![],
        }],
    };
    promote_account_incremental(&mut conn, incremental, NOW).unwrap();
    // The old planned instant is cancelled and the new, already-due instant is
    // suppressed: nothing becomes deliverable.
    assert!(!states(&conn).iter().any(|s| s == "planned"));
    assert!(states(&conn).iter().any(|s| s == "suppressed"));
    assert_eq!(delivery_count(&mut conn), 0);
}

#[test]
fn external_edit_of_unchanged_fire_keeps_one_delivery() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    let incremental = AccountIncrementalStage {
        account_id: ACCOUNT.into(),
        complete: true,
        authoritative: true,
        observed_at_ms: NOW,
        calendars: vec![CalendarIncrementalStage {
            calendar_id: CALENDAR.into(),
            base_sync_token: "token-1".into(),
            new_sync_token: "token-2".into(),
            changed_resources: vec![StagedResource {
                href: HREF.into(),
                etag: "\"e2\"".into(),
                content_type: "text/calendar".into(),
                raw_ics: ics(NOW + HOUR, None),
                uid: UID.into(),
                parse_status: ParseStatus::Parsed,
            }],
            removed_hrefs: vec![],
        }],
    };
    promote_account_incremental(&mut conn, incremental, NOW).unwrap();
    reconcile_notification_jobs(&mut conn, NOW).unwrap();
    assert_eq!(states(&conn), vec!["planned"]);
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + HOUR)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn terminal_fire_survives_revision_change_and_resync() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    let due = claim_due_notification_jobs(&mut conn, NOW + HOUR).unwrap();
    finish_notification_job(&mut conn, &due[0].job_id, true, NOW + HOUR).unwrap();
    reconcile_notification_jobs(&mut conn, NOW + 2 * HOUR).unwrap();
    // The same delayed readiness change (new ETag) must not re-deliver.
    let incremental = AccountIncrementalStage {
        account_id: ACCOUNT.into(),
        complete: true,
        authoritative: true,
        observed_at_ms: NOW + 2 * HOUR,
        calendars: vec![CalendarIncrementalStage {
            calendar_id: CALENDAR.into(),
            base_sync_token: "token-1".into(),
            new_sync_token: "token-2".into(),
            changed_resources: vec![StagedResource {
                href: HREF.into(),
                etag: "\"e2\"".into(),
                content_type: "text/calendar".into(),
                raw_ics: ics(NOW + HOUR, None),
                uid: UID.into(),
                parse_status: ParseStatus::Parsed,
            }],
            removed_hrefs: vec![],
        }],
    };
    promote_account_incremental(&mut conn, incremental, NOW + 2 * HOUR).unwrap();
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + 2 * HOUR)
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn repeated_sync_of_same_history_is_idempotent() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW - HOUR, None, "\"e1\""), NOW).unwrap();
    let first = jobs(&conn);
    promote_account_inventory(&mut conn, stage(NOW - HOUR, None, "\"e1\""), NOW).unwrap();
    promote_account_inventory(&mut conn, stage(NOW - HOUR, None, "\"e1\""), NOW).unwrap();
    assert_eq!(jobs(&conn), first);
    assert_eq!(delivery_count(&mut conn), 0);
}

#[test]
fn suppressed_fire_does_not_revive_after_restart() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW - HOUR, None, "\"e1\""), NOW).unwrap();
    for tick in 1..4 {
        reconcile_notification_jobs(&mut conn, NOW + tick * HOUR).unwrap();
        assert_eq!(states(&conn), vec!["suppressed"]);
    }
}

#[test]
fn explicit_alarm_in_the_future_on_a_started_event_still_notifies() {
    let mut conn = fixture();
    // Started 30 minutes ago, alarm 30 minutes after start.
    promote_account_inventory(
        &mut conn,
        stage(NOW - 30 * 60_000, Some("PT45M"), "\"e1\""),
        NOW,
    )
    .unwrap();
    assert_eq!(states(&conn), vec!["planned"]);
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW).unwrap().len(),
        0
    );
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + 15 * 60_000)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn alarm_that_moved_into_the_past_is_suppressed_not_flushed() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, Some("PT30M"), "\"e1\""), NOW).unwrap();
    assert_eq!(states(&conn), vec!["planned"]);
    // External edit: event now starts in the past, alarm instant also past.
    let incremental = AccountIncrementalStage {
        account_id: ACCOUNT.into(),
        complete: true,
        authoritative: true,
        observed_at_ms: NOW,
        calendars: vec![CalendarIncrementalStage {
            calendar_id: CALENDAR.into(),
            base_sync_token: "token-1".into(),
            new_sync_token: "token-2".into(),
            changed_resources: vec![StagedResource {
                href: HREF.into(),
                etag: "\"e2\"".into(),
                content_type: "text/calendar".into(),
                raw_ics: ics(NOW - 2 * HOUR, Some("PT30M")),
                uid: UID.into(),
                parse_status: ParseStatus::Parsed,
            }],
            removed_hrefs: vec![],
        }],
    };
    promote_account_incremental(&mut conn, incremental, NOW).unwrap();
    assert_eq!(delivery_count(&mut conn), 0);
    assert!(!states(&conn).iter().any(|s| s == "planned"));
}

#[test]
fn hidden_then_revealed_calendar_restores_legitimate_catch_up() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    conn.execute("UPDATE calendars SET selected=0", []).unwrap();
    reconcile_notification_jobs(&mut conn, NOW).unwrap();
    assert_eq!(states(&conn), vec!["cancelled"]);
    conn.execute("UPDATE calendars SET selected=1", []).unwrap();
    reconcile_notification_jobs(&mut conn, NOW + HOUR + 5 * 60_000).unwrap();
    let due = claim_due_notification_jobs(&mut conn, NOW + HOUR + 5 * 60_000).unwrap();
    assert_eq!(due.len(), 1);
}

#[test]
fn removal_cancels_the_job_without_delivery() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    let incremental = AccountIncrementalStage {
        account_id: ACCOUNT.into(),
        complete: true,
        authoritative: true,
        observed_at_ms: NOW,
        calendars: vec![CalendarIncrementalStage {
            calendar_id: CALENDAR.into(),
            base_sync_token: "token-1".into(),
            new_sync_token: "token-2".into(),
            changed_resources: vec![],
            removed_hrefs: vec![HREF.into()],
        }],
    };
    promote_account_incremental(&mut conn, incremental, NOW).unwrap();
    assert!(!states(&conn).iter().any(|s| s == "planned"));
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + HOUR)
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn notifications_disabled_then_reenabled_does_not_flush() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    conn.execute(
        "UPDATE app_settings SET reminder_notifications_enabled=0 WHERE singleton_id=1",
        [],
    )
    .unwrap();
    assert_eq!(delivery_count(&mut conn), 0);
    conn.execute(
        "UPDATE app_settings SET reminder_notifications_enabled=1 WHERE singleton_id=1",
        [],
    )
    .unwrap();
    // Mirrors update_app_settings: everything already due is spent on re-enable.
    conn.execute(
        "UPDATE notification_jobs SET state='suppressed',finished_at_utc_ms=?1 WHERE state='planned' AND fire_at_utc_ms<=?1",
        [NOW + 2 * HOUR],
    )
    .unwrap();
    assert_eq!(delivery_count(&mut conn), 0);
}

#[test]
fn catch_up_bounds_are_inclusive_at_the_documented_edges() {
    for (late, delivered, catch_up) in [
        (15 * 60_000_i64, true, false),
        (15 * 60_000 + 1, true, true),
        (24 * HOUR, true, true),
        (24 * HOUR + 1, false, false),
    ] {
        let mut conn = fixture();
        promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
        reconcile_notification_jobs(&mut conn, NOW + HOUR + late).unwrap();
        let due = claim_due_notification_jobs(&mut conn, NOW + HOUR + late).unwrap();
        assert_eq!(due.len(), usize::from(delivered), "late {late}");
        if delivered {
            assert_eq!(due[0].catch_up, catch_up, "late {late}");
        }
    }
}

#[test]
fn recurrence_instances_keep_independent_jobs() {
    let mut conn = fixture();
    let raw = format!(
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{UID}\r\nDTSTAMP:20240101T000000Z\r\nDTSTART:{}\r\nDTEND:{}\r\nRRULE:FREQ=DAILY;COUNT=3\r\nSUMMARY:R\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        stamp(NOW + HOUR),
        stamp(NOW + 2 * HOUR)
    );
    let inventory = AccountInventoryStage {
        account_id: ACCOUNT.into(),
        provider_kind: "caldav".into(),
        user_label: "test".into(),
        credential_ref: format!("calendar-todo/account/{ACCOUNT}"),
        region_profile: "icloud-cn".into(),
        complete: true,
        authoritative: true,
        observed_at_ms: NOW,
        calendars: vec![CalendarInventoryStage {
            calendar_id: CALENDAR.into(),
            collection_url: COLLECTION.into(),
            display_name: "Cal".into(),
            server_color: None,
            privileges: "{}".into(),
            sync_token: Some("token-1".into()),
            resources: vec![StagedResource {
                href: HREF.into(),
                etag: "\"e1\"".into(),
                content_type: "text/calendar".into(),
                raw_ics: raw.into_bytes(),
                uid: UID.into(),
                parse_status: ParseStatus::Parsed,
            }],
        }],
    };
    promote_account_inventory(&mut conn, inventory, NOW).unwrap();
    let occurrences: Vec<String> = jobs(&conn)
        .into_iter()
        .map(|(key, _, _)| key)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert!(
        occurrences.len() >= 2,
        "occurrences: {occurrences:?} jobs: {:?}",
        jobs(&conn)
    );
    let first = claim_due_notification_jobs(&mut conn, NOW + HOUR).unwrap();
    assert_eq!(first.len(), 1, "jobs: {:?}", jobs(&conn));
    let second = claim_due_notification_jobs(&mut conn, NOW + 25 * HOUR).unwrap();
    assert_eq!(second.len(), 1, "jobs: {:?}", jobs(&conn));
    assert_ne!(first[0].occurrence_key, second[0].occurrence_key);
}

#[test]
fn external_edit_that_replaces_the_default_fire_suppresses_the_new_overdue_one() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    // External client adds an alarm two hours before a start that is already past.
    let incremental = AccountIncrementalStage {
        account_id: ACCOUNT.into(),
        complete: true,
        authoritative: true,
        observed_at_ms: NOW,
        calendars: vec![CalendarIncrementalStage {
            calendar_id: CALENDAR.into(),
            base_sync_token: "token-1".into(),
            new_sync_token: "token-2".into(),
            changed_resources: vec![StagedResource {
                href: HREF.into(),
                etag: "\"e2\"".into(),
                content_type: "text/calendar".into(),
                raw_ics: ics(NOW - HOUR, Some("-PT2H")),
                uid: UID.into(),
                parse_status: ParseStatus::Parsed,
            }],
            removed_hrefs: vec![],
        }],
    };
    promote_account_incremental(&mut conn, incremental, NOW).unwrap();
    assert!(!states(&conn).iter().any(|s| s == "planned"));
    assert_eq!(delivery_count(&mut conn), 0);
}

#[test]
fn clock_jump_past_the_catch_up_window_marks_the_fire_missed() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    let later = NOW + HOUR + 24 * HOUR + 1;
    reconcile_notification_jobs(&mut conn, later).unwrap();
    assert_eq!(
        claim_due_notification_jobs(&mut conn, later).unwrap().len(),
        0
    );
    assert_eq!(states(&conn), vec!["missed"]);
}

#[test]
fn clock_before_the_fire_keeps_it_planned_and_unclaimable() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    reconcile_notification_jobs(&mut conn, NOW - HOUR).unwrap();
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW - HOUR)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(states(&conn), vec!["planned"]);
}

fn local_todo(id: Option<&str>, title: &str, at: i64) -> SaveLocalTodo {
    SaveLocalTodo {
        todo_id: id.map(str::to_string),
        selected_occurrence_key: None,
        calendar_id: "00000000-0000-4000-8000-0000000000c1".into(),
        title: title.into(),
        notes: None,
        location: None,
        start_at_utc_ms: Some(at),
        due_at_utc_ms: None,
        priority: Priority::Normal,
        important: false,
        tag_names: vec![],
        recurrence: None,
    }
}

#[test]
fn local_todo_created_in_the_past_does_not_notify() {
    let mut conn = fixture();
    save_local_todo(&mut conn, local_todo(None, "Past", NOW - 5 * 60_000), NOW).unwrap();
    assert_eq!(states(&conn), vec!["suppressed"]);
    assert_eq!(delivery_count(&mut conn), 0);
}

#[test]
fn local_todo_created_in_the_future_notifies_once() {
    let mut conn = fixture();
    save_local_todo(&mut conn, local_todo(None, "Soon", NOW + HOUR), NOW).unwrap();
    assert_eq!(states(&conn), vec!["planned"]);
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW).unwrap().len(),
        0
    );
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + HOUR)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn local_todo_moved_into_the_past_is_suppressed() {
    let mut conn = fixture();
    let todo = save_local_todo(&mut conn, local_todo(None, "Later", NOW + HOUR), NOW).unwrap();
    save_local_todo(
        &mut conn,
        local_todo(Some(&todo.todo_id), "Later", NOW - HOUR),
        NOW,
    )
    .unwrap();
    assert!(!states(&conn).iter().any(|s| s == "planned"));
    assert_eq!(delivery_count(&mut conn), 0);
}

#[test]
fn local_todo_text_edit_keeps_its_planned_fire() {
    let mut conn = fixture();
    let todo = save_local_todo(&mut conn, local_todo(None, "Later", NOW + HOUR), NOW).unwrap();
    save_local_todo(
        &mut conn,
        local_todo(Some(&todo.todo_id), "Renamed", NOW + HOUR),
        NOW,
    )
    .unwrap();
    assert_eq!(states(&conn), vec!["planned"]);
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + HOUR)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn first_import_of_two_hundred_historical_events_delivers_nothing() {
    let mut conn = fixture();
    let resources = (0..200)
        .map(|index| {
            let uid = format!("history-{index}@example");
            let body = format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20240101T000000Z\r\nDTSTART:{}\r\nDTEND:{}\r\nSUMMARY:H{index}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
                stamp(NOW - (index as i64 + 1) * 60_000),
                stamp(NOW - (index as i64 + 1) * 60_000 + HOUR)
            );
            StagedResource {
                href: format!("/22222222-2222-4222-8222-222222222222/h{index}.ics"),
                etag: format!("\"h{index}\""),
                content_type: "text/calendar".into(),
                raw_ics: body.into_bytes(),
                uid,
                parse_status: ParseStatus::Parsed,
            }
        })
        .collect();
    let inventory = AccountInventoryStage {
        account_id: ACCOUNT.into(),
        provider_kind: "caldav".into(),
        user_label: "test".into(),
        credential_ref: format!("calendar-todo/account/{ACCOUNT}"),
        region_profile: "icloud-cn".into(),
        complete: true,
        authoritative: true,
        observed_at_ms: NOW,
        calendars: vec![CalendarInventoryStage {
            calendar_id: CALENDAR.into(),
            collection_url: COLLECTION.into(),
            display_name: "Cal".into(),
            server_color: None,
            privileges: "{}".into(),
            sync_token: Some("token-1".into()),
            resources,
        }],
    };
    promote_account_inventory(&mut conn, inventory, NOW).unwrap();
    assert_eq!(jobs(&conn).len(), 200);
    assert!(states(&conn).iter().all(|state| state == "suppressed"));
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW).unwrap().len(),
        0
    );
}

#[test]
fn confirmed_remote_deletion_cancels_the_planned_reminder() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    let resource_id: String = conn
        .query_row("SELECT resource_id FROM remote_resources", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "UPDATE remote_resources SET lifecycle='missing_candidate',candidate_evidence_valid=1",
        [],
    )
    .unwrap();
    reconcile_deletion_evidence(
        &mut conn,
        AccountDeletionReconciliation {
            account_id: ACCOUNT.into(),
            complete: true,
            authoritative: true,
            probes: vec![DeletionProbe {
                resource_id,
                status: DirectProbeStatus::NotFound,
                observed_at_ms: NOW,
            }],
        },
    )
    .unwrap();
    assert_eq!(states(&conn), vec!["cancelled"]);
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + HOUR)
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn legacy_terminal_and_planned_duplicate_cannot_both_deliver() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    let job_id: String = conn
        .query_row("SELECT job_id FROM notification_jobs", [], |r| r.get(0))
        .unwrap();
    let due = claim_due_notification_jobs(&mut conn, NOW + HOUR).unwrap();
    assert_eq!(due.len(), 1);
    finish_notification_job(&mut conn, &job_id, true, NOW + HOUR).unwrap();
    // Pre-upgrade rows could hold the same logical instant under a second revision.
    conn.execute(
        "INSERT INTO notification_jobs(job_id,source_kind,source_id,occurrence_key,revision,fire_at_utc_ms,state)
         SELECT ?1,source_kind,source_id,occurrence_key,revision||'-legacy',fire_at_utc_ms,'planned'
         FROM notification_jobs WHERE job_id=?2",
        params![uuid::Uuid::new_v4().to_string(), job_id],
    )
    .unwrap();
    reconcile_notification_jobs(&mut conn, NOW + HOUR).unwrap();
    assert!(!states(&conn).iter().any(|state| state == "planned"));
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + HOUR)
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn local_recurrence_metadata_plans_its_future_reminders() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    let resource_id: String = conn
        .query_row("SELECT resource_id FROM remote_resources", [], |r| r.get(0))
        .unwrap();
    let transaction = conn.transaction().unwrap();
    update_calendar_local_metadata_in(
        &transaction,
        UpdateCalendarLocalMetadata {
            resource_id,
            tag_names: vec![],
            recurrence: Some(
                crate::domain::calendar_event::CalendarFixedRecurrenceInput {
                    frequency: crate::domain::local_todo::RecurrenceFrequency::Daily,
                    interval: 1,
                    options: Default::default(),
                },
            ),
            important: None,
        },
        NOW,
    )
    .unwrap();
    transaction.commit().unwrap();
    let local_fires: Vec<i64> = jobs(&conn)
        .into_iter()
        .filter(|(key, _, _)| key.starts_with("LOCAL:"))
        .map(|(_, fire, _)| fire)
        .collect();
    assert!(!local_fires.is_empty(), "jobs: {:?}", jobs(&conn));
    assert!(local_fires.iter().all(|fire| *fire > NOW));
    assert!(
        !states(&conn).iter().any(|state| state == "suppressed"),
        "jobs: {:?}",
        jobs(&conn)
    );
}

#[test]
fn claim_drains_more_than_the_batch_limit_across_calls() {
    let mut conn = fixture();
    for index in 0..130 {
        save_local_todo(
            &mut conn,
            local_todo(None, &format!("t{index}"), NOW + HOUR),
            NOW,
        )
        .unwrap();
    }
    assert_eq!(states(&conn).len(), 130);
    let first = claim_due_notification_jobs(&mut conn, NOW + HOUR).unwrap();
    assert_eq!(first.len(), 128);
    let second = claim_due_notification_jobs(&mut conn, NOW + HOUR).unwrap();
    assert_eq!(second.len(), 2);
    let third = claim_due_notification_jobs(&mut conn, NOW + HOUR).unwrap();
    assert_eq!(third.len(), 0);
}

#[test]
fn reappearing_series_does_not_revive_a_deleted_fire() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    let resource_id: String = conn
        .query_row("SELECT resource_id FROM remote_resources", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "UPDATE remote_resources SET lifecycle='missing_candidate',candidate_evidence_valid=1",
        [],
    )
    .unwrap();
    reconcile_deletion_evidence(
        &mut conn,
        AccountDeletionReconciliation {
            account_id: ACCOUNT.into(),
            complete: true,
            authoritative: true,
            probes: vec![DeletionProbe {
                resource_id,
                status: DirectProbeStatus::Gone,
                observed_at_ms: NOW,
            }],
        },
    )
    .unwrap();
    // The same event reappears after its reminder instant has passed.
    promote_account_inventory(
        &mut conn,
        stage_at(NOW + HOUR, None, "\"e2\"", NOW + 2 * HOUR),
        NOW + 2 * HOUR,
    )
    .unwrap();
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + 2 * HOUR)
            .unwrap()
            .len(),
        0,
        "jobs: {:?}",
        jobs(&conn)
    );
}

#[test]
fn missed_fire_stays_terminal_across_a_revision_change() {
    let mut conn = fixture();
    promote_account_inventory(&mut conn, stage(NOW + HOUR, None, "\"e1\""), NOW).unwrap();
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + 26 * HOUR)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(states(&conn), vec!["missed"]);
    promote_account_inventory(
        &mut conn,
        stage_at(NOW + HOUR, None, "\"e2\"", NOW + 26 * HOUR),
        NOW + 26 * HOUR,
    )
    .unwrap();
    assert_eq!(states(&conn), vec!["missed"]);
    assert_eq!(
        claim_due_notification_jobs(&mut conn, NOW + 27 * HOUR)
            .unwrap()
            .len(),
        0
    );
}

fn stamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}
