//! Synthetic public regressions. No copied account/event data.
use super::*;
use crate::domain::{calendar_alarm, calendar_event};

fn calendar(zone: &str, start: &str, rule: &str, extra: &str, detached: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:{zone}\r\nBEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETFROM:+0800\r\nTZOFFSETTO:+0800\r\nRRULE:FREQ=YEARLY;BYMONTH=1;BYMONTHDAY=1\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:synthetic-tzid\r\nSUMMARY:Synthetic weekly\r\nDTSTART;TZID={zone}:{start}\r\nDURATION:PT1H15M30S\r\nRRULE:{rule}\r\n{extra}END:VEVENT\r\n{detached}END:VCALENDAR\r\n"
    )
}
fn expand(raw: &str) -> Result<RecurrenceExpansion, RecurrenceError> {
    expand_vevent_recurrence(
        raw.as_bytes(),
        "synthetic-tzid",
        ExpansionWindow {
            start_at_utc_ms: parse_utc("20300101T000000Z").unwrap(),
            end_at_utc_ms: parse_utc("20310101T000000Z").unwrap(),
            max_occurrences: 512,
        },
    )
}
fn keys(result: &RecurrenceExpansion) -> Vec<&str> {
    result
        .occurrences
        .iter()
        .map(|v| v.identity.canonical_key.as_str())
        .collect()
}
#[test]
fn tzid_fold_near_window_or_until_is_not_silently_omitted() {
    for (start, frequency) in [("20301027T013000", "WEEKLY"), ("20301102T013000", "DAILY")] {
        for (rule, end) in [
            (format!("FREQ={frequency};COUNT=2"), "20301103T061500Z"),
            (
                format!("FREQ={frequency};UNTIL=20301103T061500Z"),
                "20301110T000000Z",
            ),
        ] {
            let raw = calendar("America/New_York", start, &rule, "", "");
            assert_eq!(
                expand_vevent_recurrence(
                    raw.as_bytes(),
                    "synthetic-tzid",
                    ExpansionWindow {
                        start_at_utc_ms: parse_utc("20301020T000000Z").unwrap(),
                        end_at_utc_ms: parse_utc(end).unwrap(),
                        max_occurrences: 512
                    }
                )
                .unwrap_err(),
                RecurrenceError::UnsupportedTime
            );
        }
    }
}
#[test]
fn tzid_unreferenced_zone_declaration_does_not_change_projection() {
    let raw = calendar(
        "Asia/Shanghai",
        "20300106T090000",
        "FREQ=WEEKLY;COUNT=2",
        "",
        "",
    );
    let additional = raw.replace(
        "BEGIN:VEVENT",
        "BEGIN:VTIMEZONE\r\nTZID:Europe/London\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT",
    );
    assert_eq!(
        expand(&raw).unwrap().occurrences,
        expand(&additional).unwrap().occurrences
    );
}

#[test]
fn tzid_weekly_duration_until_raw_and_dispatch_order() {
    let raw = calendar(
        "Asia/Shanghai",
        "20300106T090000",
        "FREQ=WEEKLY;BYDAY=SU;WKST=SU;UNTIL=20300120T010000Z",
        "",
        "",
    );
    let result = expand(&raw).unwrap();
    assert_eq!(result.preserved_raw_ics, raw.as_bytes());
    assert_eq!(
        keys(&result),
        [
            "TZID:Asia/Shanghai:20300106T090000",
            "TZID:Asia/Shanghai:20300113T090000",
            "TZID:Asia/Shanghai:20300120T090000"
        ]
    );
    assert_eq!(
        result.occurrences[0].effective_start_at_utc_ms,
        parse_utc("20300106T010000Z").unwrap()
    );
    assert!(
        result
            .occurrences
            .iter()
            .all(|v| v.effective_end_at_utc_ms - v.effective_start_at_utc_ms == 4_530_000)
    );
    let reordered = raw
        .replace("DURATION:PT1H15M30S\r\n", "")
        .replace("END:VEVENT", "DURATION:PT1H15M30S\r\nEND:VEVENT");
    for input in [&raw, &reordered] {
        assert_eq!(
            calendar_event::normalize_vevent(input.as_bytes(), "synthetic-tzid").unsupported_reason,
            Some(calendar_event::UnsupportedReason::UnsupportedRecurrence)
        );
        assert_eq!(expand(input).unwrap().occurrences, result.occurrences);
    }
}
#[test]
fn tzid_wkst_changes_interval_week_grouping() {
    let raw = calendar(
        "Asia/Shanghai",
        "20300105T090000",
        "FREQ=WEEKLY;INTERVAL=2;BYDAY=SA,SU;WKST=SU;UNTIL=20300127T010000Z",
        "",
        "",
    );
    assert_eq!(
        keys(&expand(&raw).unwrap()),
        [
            "TZID:Asia/Shanghai:20300105T090000",
            "TZID:Asia/Shanghai:20300113T090000",
            "TZID:Asia/Shanghai:20300119T090000",
            "TZID:Asia/Shanghai:20300127T090000"
        ]
    );
    assert_eq!(
        keys(&expand(&raw.replace("WKST=SU", "WKST=MO")).unwrap()),
        [
            "TZID:Asia/Shanghai:20300105T090000",
            "TZID:Asia/Shanghai:20300106T090000",
            "TZID:Asia/Shanghai:20300119T090000",
            "TZID:Asia/Shanghai:20300120T090000"
        ]
    );
}
#[test]
fn tzid_utc_exdate_and_detached_keep_nominal_keys_and_alarms() {
    let detached = "BEGIN:VEVENT\r\nUID:synthetic-tzid\r\nRECURRENCE-ID;TZID=Asia/Shanghai:20300120T090000\r\nDTSTART;TZID=Asia/Shanghai:20300120T110000\r\nDTEND;TZID=Asia/Shanghai:20300120T113000\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:synthetic-tzid\r\nRECURRENCE-ID;TZID=Asia/Shanghai:20300127T090000\r\nDTSTART;TZID=Asia/Shanghai:20300127T090000\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n";
    let raw = calendar(
        "Asia/Shanghai",
        "20300106T090000",
        "FREQ=WEEKLY;COUNT=4;BYDAY=SU;WKST=SU",
        "EXDATE:20300113T010000Z\r\n",
        detached,
    );
    let result = expand(&raw).unwrap();
    assert_eq!(
        keys(&result),
        [
            "TZID:Asia/Shanghai:20300106T090000",
            "TZID:Asia/Shanghai:20300120T090000",
            "TZID:Asia/Shanghai:20300127T090000"
        ]
    );
    let moved = &result.occurrences[1];
    assert_eq!(
        moved.effective_start_at_utc_ms,
        parse_utc("20300120T030000Z").unwrap()
    );
    assert_eq!(
        moved.effective_end_at_utc_ms - moved.effective_start_at_utc_ms,
        1_800_000
    );
    assert!(result.occurrences[2].cancelled);
    let alarm = calendar_alarm::notification_fires_for_occurrence(
        raw.as_bytes(),
        &moved.identity.canonical_key,
        moved.effective_start_at_utc_ms,
        moved.effective_end_at_utc_ms,
    )
    .unwrap();
    assert_eq!(
        alarm[0].fire_at_utc_ms,
        moved.effective_start_at_utc_ms - 600_000
    );
    let first = &result.occurrences[0];
    assert_eq!(
        calendar_alarm::notification_fires_for_occurrence(
            raw.as_bytes(),
            &first.identity.canonical_key,
            first.effective_start_at_utc_ms,
            first.effective_end_at_utc_ms
        )
        .unwrap()[0]
            .fire_at_utc_ms,
        first.effective_start_at_utc_ms
    );
    assert!(
        calendar_alarm::notification_fires_for_occurrence(
            raw.replace("ACTION:DISPLAY", "ACTION:EMAIL").as_bytes(),
            &moved.identity.canonical_key,
            moved.effective_start_at_utc_ms,
            moved.effective_end_at_utc_ms
        )
        .is_err()
    );
}
#[test]
fn tzid_candidates_follow_civil_time_across_offset_changes() {
    let raw = calendar(
        "America/New_York",
        "20300303T090000",
        "FREQ=WEEKLY;COUNT=3;WKST=SU",
        "",
        "",
    );
    let result = expand(&raw).unwrap();
    assert_eq!(
        result
            .occurrences
            .iter()
            .map(|v| v.effective_start_at_utc_ms)
            .collect::<Vec<_>>(),
        [
            parse_utc("20300303T140000Z").unwrap(),
            parse_utc("20300310T130000Z").unwrap(),
            parse_utc("20300317T130000Z").unwrap()
        ]
    );
    assert_eq!(
        result.occurrences[1].identity.canonical_key,
        "TZID:America/New_York:20300310T090000"
    );
    assert!(
        expand(&calendar(
            "America/New_York",
            "20300303T023000",
            "FREQ=WEEKLY;UNTIL=20300304T000000Z",
            "",
            ""
        ))
        .is_ok()
    );
    for start in ["20300303T023000", "20301027T013000"] {
        assert_eq!(
            expand(&calendar(
                "America/New_York",
                start,
                "FREQ=WEEKLY;COUNT=3",
                "",
                ""
            ))
            .unwrap_err(),
            RecurrenceError::UnsupportedTime
        );
    }
}
#[test]
fn tzid_invalid_duration_zone_rule_and_identity_fail_closed() {
    let raw = calendar(
        "Asia/Shanghai",
        "20300106T090000",
        "FREQ=WEEKLY;COUNT=3;WKST=SU",
        "",
        "",
    );
    for duration in [
        "PT0S",
        "-PT1H",
        "P1D",
        "P1W",
        "PT1.5H",
        "PT1H2H",
        "PT1M1H",
        "PT999999999999999999999999H",
        "PT",
        "PT1",
        "PT+1H",
    ] {
        assert!(
            expand(&raw.replace("PT1H15M30S", duration)).is_err(),
            "{duration}"
        );
    }
    for duration in ["PT1H", "PT2M", "PT3S", "PT1H0M1S"] {
        assert!(
            expand(&raw.replace("PT1H15M30S", duration)).is_ok(),
            "{duration}"
        );
    }
    assert!(
        expand(&raw.replace(
            "DURATION:",
            "DTEND;TZID=Asia/Shanghai:20300106T100000\r\nDURATION:"
        ))
        .is_err()
    );
    assert!(expand(&raw.replace("Asia/Shanghai", "Custom/Unknown")).is_err());
    assert!(expand(&raw.replace("TZID:Asia/Shanghai\r\n", "")).is_err());
    assert!(expand(&raw.replace("TZID:Asia/Shanghai", "TZID:Custom/Unknown")).is_err());
    assert!(
        expand(&raw.replace(
            "TZID:Asia/Shanghai",
            "TZID:Asia/Shanghai\r\nTZID:Asia/Shanghai"
        ))
        .is_err()
    );
    assert!(expand(&raw.replace("TZID=Asia/Shanghai", "TZID=Asia/Tokyo")).is_err());
    assert!(expand(&raw.replace("WKST=SU", "WKST=XX")).is_err());
    assert!(expand(&raw.replace("FREQ=WEEKLY", "FREQ=YEARLY")).is_err());
    assert!(expand(&raw.replace("DTSTART;TZID=Asia/Shanghai", "DTSTART")).is_err());
    for key in [
        "TZID:Asia/Shanghai:20300106T090000",
        "TZID:America/New_York:20300310T090000",
    ] {
        assert!(calendar_event::validate_recurrence_key(key).is_ok());
    }
    for key in [
        "TZID:Asia/Shanghai:20301306T090000",
        "TZID:Asia/Shanghai:20300106T090000Z",
        "TZID::20300106T090000",
        "TZID:Asia\nShanghai:20300106T090000",
        "TZID:Asia/Shanghai:20300106T09000é",
    ] {
        assert!(calendar_event::validate_recurrence_key(key).is_err());
    }
}
