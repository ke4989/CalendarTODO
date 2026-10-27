//! Synthetic actor integration: cache, promotion, rebuild, identity and reminder safety.
use super::*;
use crate::domain::calendar_sync::{
    CalendarIncrementalStage, CalendarInventoryStage, StagedResource,
};
const ACCOUNT: &str = "77777777-7777-4777-8777-777777777771";
const CALENDAR: &str = "77777777-7777-4777-8777-777777777772";
const HREF: &str = "/synthetic/tzid.ics";
fn at(value: &str) -> i64 {
    value.parse::<jiff::Timestamp>().unwrap().as_millisecond()
}
struct Fixed(i64);
impl crate::ports::clock::Clock for Fixed {
    fn now_utc_ms(&self) -> Result<i64, crate::ports::clock::ClockError> {
        Ok(self.0)
    }
}
fn raw() -> Vec<u8> {
    b"BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:Asia/Shanghai\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:synthetic-tzid\r\nSUMMARY:Synthetic weekly\r\nDTSTART;TZID=Asia/Shanghai:20300106T090000\r\nDURATION:PT1H\r\nRRULE:FREQ=WEEKLY;COUNT=4;BYDAY=SU;WKST=SU\r\nX-UNKNOWN:retained\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".to_vec()
}
fn stage(body: Vec<u8>, etag: &str, token: &str) -> AccountInventoryStage {
    AccountInventoryStage {
        account_id: ACCOUNT.into(),
        provider_kind: "caldav".into(),
        user_label: "Synthetic".into(),
        credential_ref: format!("calendar-todo/account/{ACCOUNT}"),
        region_profile: "synthetic".into(),
        complete: true,
        authoritative: true,
        observed_at_ms: at("2030-01-13T01:00:00Z"),
        calendars: vec![CalendarInventoryStage {
            calendar_id: CALENDAR.into(),
            collection_url: "https://example.invalid/synthetic/".into(),
            display_name: "Synthetic".into(),
            server_color: None,
            privileges: "read".into(),
            sync_token: Some(token.into()),
            resources: vec![StagedResource {
                href: HREF.into(),
                etag: etag.into(),
                content_type: "text/calendar".into(),
                raw_ics: body,
                uid: "synthetic-tzid".into(),
                parse_status: ParseStatus::ReadOnly,
            }],
        }],
    }
}
#[test]
fn tzid_promotion_rebuild_restart_preserve_state_raw_and_terminal_reminders() {
    let directory =
        std::env::temp_dir().join(format!("calendar-tzid-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("synthetic.sqlite3");
    let now = at("2030-01-13T01:00:00Z");
    let database = Database::open_with_clock(&path, Arc::new(Fixed(now))).unwrap();
    let original = raw();
    assert!(
        crate::domain::remote_event_update::validate_event_base_v3(
            &original,
            "synthetic-tzid",
            false
        )
        .is_err()
    );
    database
        .promote_captured_account_inventory(
            database.begin_sync_capture(ACCOUNT.into()).unwrap(),
            stage(original.clone(), "etag-1", "token-1")
                .validate()
                .unwrap(),
        )
        .unwrap();
    let items = database.list_calendar_agenda_items().unwrap();
    assert_eq!(items.len(), 4);
    assert!(items.iter().all(|v| v.status == ProjectionStatus::Ready
        && v.unsupported_reason.is_none()
        && v.title == "Synthetic weekly"
        && !v.remote_edit_eligible
        && !v.remote_time_edit_eligible));
    let retained = items[2].clone();
    database
        .set_calendar_completed(
            retained.series_id.clone(),
            retained.recurrence_key.clone(),
            true,
        )
        .unwrap();
    database
        .create_calendar_subtask(
            CreateCalendarSubtask {
                series_id: retained.series_id.clone(),
                recurrence_key: retained.recurrence_key.clone(),
                text: "Synthetic subtask".into(),
            }
            .validate()
            .unwrap(),
        )
        .unwrap();
    database
        .refresh_calendar_projections(now, RecurrenceHorizon::new(0, 2 * 86_400_000).unwrap())
        .unwrap();
    assert!(
        database
            .list_calendar_agenda_items()
            .unwrap()
            .iter()
            .all(|v| v.recurrence_key != retained.recurrence_key)
    );
    database
        .refresh_calendar_projections(now, RecurrenceHorizon::standard())
        .unwrap();
    let full = stage(original.clone(), "etag-2", "token-2");
    database
        .promote_captured_account_incremental(
            database.begin_sync_capture(ACCOUNT.into()).unwrap(),
            AccountIncrementalStage {
                account_id: ACCOUNT.into(),
                complete: true,
                authoritative: true,
                observed_at_ms: now + 1,
                calendars: vec![CalendarIncrementalStage {
                    calendar_id: CALENDAR.into(),
                    base_sync_token: "token-1".into(),
                    new_sync_token: "token-2".into(),
                    changed_resources: full.calendars[0].resources.clone(),
                    removed_hrefs: vec![],
                }],
            }
            .validate()
            .unwrap(),
        )
        .unwrap();
    let connection = Connection::open(&path).unwrap();
    let stored: (Vec<u8>, String) = connection
        .query_row(
            "SELECT raw_ics,parse_status FROM remote_resources",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, (original, "read_only".into()));
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM notification_jobs WHERE state='planned' AND fire_at_utc_ms<=?1",[now],|r|r.get::<_,i64>(0)).unwrap(),0);
    assert!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM notification_jobs WHERE state='suppressed'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap()
            >= 2
    );
    drop(connection);
    let future = at("2030-01-27T01:00:00Z");
    assert_eq!(
        database.reconcile_notification_jobs(now).unwrap(),
        Some(future)
    );
    let due = database.claim_due_notification_jobs(future).unwrap();
    assert_eq!(due.len(), 1);
    database
        .finish_notification_job(due[0].job_id.clone(), true, future)
        .unwrap();
    drop(database);
    // Simulate an old unsupported disposable projection; startup must use retained raw bytes.
    let connection = Connection::open(&path).unwrap();
    connection.execute("UPDATE calendar_event_projections SET projection_status='unsupported',unsupported_reason='unsupported_time',start_at_utc_ms=NULL,end_at_utc_ms=NULL",[]).unwrap();
    drop(connection);
    let reopened = Database::open_with_clock(&path, Arc::new(Fixed(future))).unwrap();
    let restored = reopened.list_calendar_agenda_items().unwrap();
    assert_eq!(restored.len(), 4);
    let restored = restored
        .iter()
        .find(|v| v.recurrence_key == retained.recurrence_key)
        .unwrap();
    assert_eq!(restored.work_item_id, retained.work_item_id);
    assert!(restored.completed);
    assert_eq!(restored.subtasks[0].text, "Synthetic subtask");
    assert!(
        reopened
            .claim_due_notification_jobs(future)
            .unwrap()
            .is_empty()
    );
    assert_eq!(reopened.reconcile_notification_jobs(future).unwrap(), None);
    drop(reopened);
    std::fs::remove_dir_all(directory).unwrap();
}
