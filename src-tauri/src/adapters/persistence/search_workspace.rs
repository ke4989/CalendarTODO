use super::*;
use crate::domain::search_workspace::*;

pub(super) const MIGRATION: &str = r#"
CREATE TABLE search_preferences (
 singleton_id INTEGER PRIMARY KEY CHECK(singleton_id=1),
 fields_json TEXT NOT NULL DEFAULT '["title"]',
 include_completed INTEGER NOT NULL DEFAULT 1 CHECK(include_completed IN(0,1))
) STRICT;
INSERT INTO search_preferences(singleton_id) VALUES(1);
CREATE INDEX search_local_time ON local_todos(COALESCE(start_at_utc_ms,due_at_utc_ms),todo_id) WHERE active=1;
CREATE INDEX search_occurrence_time ON local_todo_occurrences(COALESCE(nominal_start_at_utc_ms,nominal_due_at_utc_ms),local_todo_id,occurrence_key);
CREATE INDEX search_calendar_time ON calendar_event_projections(start_at_utc_ms,series_id,recurrence_key);
"#;

pub(super) fn preferences(
    c: &Connection,
    update: Option<SearchPreferences>,
) -> Result<SearchPreferences, PersistenceError> {
    if let Some(value) = update {
        WorkspaceQuery {
            text: String::new(),
            preferences: value.clone(),
            anchor_utc_ms: 0,
            anchor_key: None,
            direction: SearchDirection::After,
            cursor: None,
        }
        .terms()
        .map_err(|_| PersistenceError::UnsafeSyncStage)?;
        c.execute("UPDATE search_preferences SET fields_json=?1,include_completed=?2 WHERE singleton_id=1", params![serde_json::to_string(&value.fields).map_err(|_| PersistenceError::UnsafeSyncStage)?,value.include_completed])?;
    }
    let (fields, include_completed): (String, bool) = c.query_row(
        "SELECT fields_json,include_completed FROM search_preferences WHERE singleton_id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let value = SearchPreferences {
        fields: serde_json::from_str(&fields).map_err(|_| PersistenceError::UnsafeSyncStage)?,
        include_completed,
    };
    value
        .validate()
        .map_err(|_| PersistenceError::UnsafeSyncStage)?;
    Ok(value)
}

// UNION streams are lightweight facts. No subtasks, raw ICS or mutation qualification.
// Pending head and successor fields participate BEFORE ordering and cursor filtering.
const FACTS: &str = r#"
WITH updates AS (
 SELECT o.resource_id,o.intent_json AS head,u.intent_json AS successor
 FROM outbox_operations o LEFT JOIN remote_update_successors u ON u.resource_id=o.resource_id AND u.state<>'cancelled'
 WHERE o.operation_kind='update_event' AND o.state IN('pending','conflict')
), facts AS (
 SELECT 'todo' kind,t.todo_id id,'SINGLETON' occurrence,'' resource,
 t.local_calendar_id calendar,c.name calendar_name,c.color color,t.title,t.notes,t.location,
 COALESCE(t.start_at_utc_ms,t.due_at_utc_ms) stamp,t.due_at_utc_ms finish,COALESCE(w.completed,0) completed,
 '' head,'' successor
 FROM local_todos t JOIN local_calendars c ON c.calendar_id=t.local_calendar_id
 LEFT JOIN work_item_state w ON w.local_todo_id=t.todo_id AND w.local_occurrence_key='SINGLETON'
 WHERE t.active=1 AND c.hidden=0 AND NOT EXISTS(SELECT 1 FROM local_todo_recurrence lr WHERE lr.local_todo_id=t.todo_id)
 UNION ALL
 SELECT 'todo',t.todo_id,o.occurrence_key,'',t.local_calendar_id,c.name,c.color,t.title,t.notes,t.location,
 COALESCE(o.nominal_start_at_utc_ms,o.nominal_due_at_utc_ms),o.nominal_due_at_utc_ms,COALESCE(w.completed,0),'',''
 FROM local_todo_occurrences o JOIN local_todos t ON t.todo_id=o.local_todo_id JOIN local_calendars c ON c.calendar_id=t.local_calendar_id
 LEFT JOIN work_item_state w ON w.local_todo_id=t.todo_id AND w.local_occurrence_key=o.occurrence_key
 WHERE t.active=1 AND c.hidden=0 AND o.occurrence_state<>'cancelled' AND (o.occurrence_state='active' OR COALESCE(w.completed,0)=1)
 UNION ALL
 SELECT 'calendar',p.series_id,p.recurrence_key,p.resource_id,p.calendar_id,c.display_name,COALESCE(c.color_override,c.server_color),
 COALESCE(json_extract(u.successor,'$.fields.title'),json_extract(u.head,'$.fields.title'),p.title),
 CASE WHEN json_type(u.successor,'$.fields.description') NOT IN('null') THEN CASE WHEN json_extract(u.successor,'$.fields.description.action')='set' THEN json_extract(u.successor,'$.fields.description.value') END
      WHEN json_type(u.head,'$.fields.description') NOT IN('null') THEN CASE WHEN json_extract(u.head,'$.fields.description.action')='set' THEN json_extract(u.head,'$.fields.description.value') END ELSE p.notes END,
 CASE WHEN json_type(u.successor,'$.fields.location') NOT IN('null') THEN CASE WHEN json_extract(u.successor,'$.fields.location.action')='set' THEN json_extract(u.successor,'$.fields.location.value') END
      WHEN json_type(u.head,'$.fields.location') NOT IN('null') THEN CASE WHEN json_extract(u.head,'$.fields.location.action')='set' THEN json_extract(u.head,'$.fields.location.value') END ELSE p.location END,
 COALESCE(json_extract(u.successor,'$.fields.time.startAtUtcMs'),json_extract(u.head,'$.fields.time.startAtUtcMs'),p.start_at_utc_ms),
 COALESCE(json_extract(u.successor,'$.fields.time.endAtUtcMs'),json_extract(u.head,'$.fields.time.endAtUtcMs'),p.end_at_utc_ms),COALESCE(w.completed,0),COALESCE(u.head,''),COALESCE(u.successor,'')
 FROM calendar_event_projections p JOIN calendars c ON c.calendar_id=p.calendar_id JOIN accounts a ON a.account_id=c.account_id
 JOIN remote_resources r ON r.resource_id=p.resource_id LEFT JOIN updates u ON u.resource_id=p.resource_id
 LEFT JOIN work_item_state w ON w.calendar_series_id=p.series_id AND w.recurrence_key=p.recurrence_key
 WHERE r.lifecycle='active' AND p.cancelled=0 AND c.selected=1 AND c.available=1 AND a.removed_at_ms IS NULL
 AND NOT(p.recurrence_key='SINGLETON' AND EXISTS(SELECT 1 FROM calendar_resource_recurrence rr WHERE rr.resource_id=p.resource_id AND rr.enabled=1))
 UNION ALL
 SELECT 'calendar',o.resource_id,'SINGLETON',o.resource_id,o.calendar_id,c.display_name,COALESCE(c.color_override,c.server_color),
 json_extract(o.intent_json,'$.title'),json_extract(o.intent_json,'$.description'),json_extract(o.intent_json,'$.location'),
 json_extract(o.intent_json,'$.startAtUtcMs'),json_extract(o.intent_json,'$.endAtUtcMs'),0,'',''
 FROM remote_create_operations o JOIN calendars c ON c.calendar_id=o.calendar_id JOIN accounts a ON a.account_id=o.account_id
 WHERE o.intent_version='utc-v2' AND o.state='pending' AND c.selected=1 AND c.available=1 AND a.removed_at_ms IS NULL
), keyed AS (SELECT *,kind||':'||id||':'||occurrence AS stable_key,COALESCE(stamp,9007199254740991) AS sort_stamp FROM facts)
"#;

pub(super) fn page(c: &Connection, query: WorkspaceQuery) -> Result<SearchPage, PersistenceError> {
    let terms = query
        .terms()
        .map_err(|_| PersistenceError::UnsafeSyncStage)?;
    let revision = c.total_changes();
    let fingerprint = query.fingerprint();
    if query.cursor.as_ref().is_some_and(|cursor| {
        cursor.revision != revision
            || cursor.fingerprint != fingerprint
            || cursor.direction != query.direction
    }) {
        return Ok(SearchPage {
            items: vec![],
            continuation: None,
            exhausted: false,
            stale: true,
            revision,
            scanned: 0,
        });
    }
    let reverse = query.direction == SearchDirection::Before;
    let position = query.cursor.as_ref().map(|v| v.position.clone());
    let filter = if position.is_some() {
        if reverse {
            "(sort_stamp,stable_key)<(?1,?2)"
        } else {
            "(sort_stamp,stable_key)>(?1,?2)"
        }
    } else if query.anchor_key.is_some() && !reverse {
        "(sort_stamp,stable_key)>=(?1,?2)"
    } else if reverse {
        "sort_stamp<?1"
    } else {
        "sort_stamp>=?1"
    };
    let stamp = position.as_ref().map_or(query.initial_time(), |v| v.time);
    let key = position
        .as_ref()
        .map_or(query.anchor_key.as_deref().unwrap_or(""), |v| {
            v.key.as_str()
        });
    let time_filter = if query.direction == SearchDirection::NoTime {
        "AND stamp IS NULL"
    } else if query.includes_untimed() {
        ""
    } else {
        "AND stamp IS NOT NULL"
    };
    let sql = format!(
        "{FACTS} SELECT kind,id,occurrence,resource,calendar,calendar_name,color,title,notes,location,stamp,finish,completed,head,successor FROM keyed WHERE {filter} AND (?3 OR completed=0) {time_filter} ORDER BY sort_stamp {},stable_key {} LIMIT 256",
        if reverse { "DESC" } else { "ASC" },
        if reverse { "DESC" } else { "ASC" }
    );
    let mut statement = c.prepare(&sql)?;
    let mut rows = statement.query(params![stamp, key, query.preferences.include_completed])?;
    let mut items = Vec::new();
    let tag_sql = if query.preferences.fields.contains(&SearchField::Tags) {
        let table = "SELECT 'todo:'||j.local_todo_id AS owner,t.name FROM local_todo_tags j JOIN tags t ON t.tag_id=j.tag_id UNION ALL SELECT 'calendar:'||j.resource_id,t.name FROM calendar_resource_tags j JOIN tags t ON t.tag_id=j.tag_id";
        Some(format!(
            "{FACTS}, candidates AS (SELECT kind,id,resource FROM keyed WHERE {filter} AND (?3 OR completed=0) {time_filter} ORDER BY sort_stamp {},stable_key {} LIMIT 256) SELECT tags.owner,tags.name FROM ({table}) tags JOIN candidates c ON tags.owner=CASE WHEN c.kind='todo' THEN 'todo:'||c.id ELSE 'calendar:'||c.resource END",
            if reverse { "DESC" } else { "ASC" },
            if reverse { "DESC" } else { "ASC" }
        ))
    } else {
        None
    };
    let mut tags = std::collections::HashMap::<String, Vec<String>>::new();
    if let Some(sql) = tag_sql {
        let mut statement = c.prepare(&sql)?;
        for row in statement.query_map(
            params![stamp, key, query.preferences.include_completed],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )? {
            let (owner, name) = row?;
            tags.entry(owner).or_default().push(name);
        }
    }
    let mut scanned = 0;
    let mut last = None;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let id: String = row.get(1)?;
        let occurrence: String = row.get(2)?;
        let resource: String = row.get(3)?;
        for index in [13, 14] {
            let intent: String = row.get(index)?;
            if !intent.is_empty() {
                crate::domain::remote_event_update::decode_stored_event_intent_v3(intent.as_bytes())?;
            }
        }
        let identity = if kind == "todo" {
            SearchIdentity::Todo {
                todo_id: id.clone(),
                occurrence_key: occurrence.clone(),
            }
        } else {
            SearchIdentity::Calendar {
                series_id: id.clone(),
                resource_id: resource.clone(),
                recurrence_key: occurrence.clone(),
            }
        };
        let mut item = SearchSummary {
            key: format!("{kind}:{id}:{occurrence}"),
            identity,
            calendar_id: row.get(4)?,
            calendar_name: row.get(5)?,
            calendar_color: row.get(6)?,
            title: row.get(7)?,
            notes: row.get(8)?,
            location: row.get(9)?,
            start_at_utc_ms: row.get(10)?,
            end_at_utc_ms: row.get(11)?,
            completed: row.get(12)?,
            tags: vec![],
        };
        if query.preferences.fields.contains(&SearchField::Tags) {
            item.tags = tags
                .get(&format!(
                    "{kind}:{}",
                    if kind == "todo" { &id } else { &resource }
                ))
                .cloned()
                .unwrap_or_default();
        }
        scanned += 1;
        last = Some(item.position());
        if item.matches(&query.preferences.fields, &terms) {
            items.push(item);
        }
        if items.len() == 25 {
            break;
        }
    }
    let exhausted = scanned < 256 && items.len() < 25;
    if reverse {
        items.reverse();
    }
    Ok(SearchPage {
        items,
        continuation: last.map(|position| SearchCursor {
            position,
            revision,
            fingerprint,
            direction: query.direction,
        }),
        exhausted,
        stale: false,
        revision,
        scanned,
    })
}

pub(super) fn remote_key(
    c: &Connection,
    account: &str,
    uid: &str,
    recurrence: &str,
) -> Result<Option<Option<String>>, PersistenceError> {
    let series: Option<String> = c
        .query_row(
            "SELECT series_id FROM event_series WHERE account_id=?1 AND uid=?2",
            params![account, uid],
            |r| r.get(0),
        )
        .optional()?;
    let Some(series) = series else {
        return Ok(Some(None));
    };
    let pending:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM outbox_operations WHERE series_id=?1 AND state IN('pending','conflict')) OR EXISTS(SELECT 1 FROM remote_delete_operations WHERE series_id=?1 AND state IN('pending','conflict'))",[&series],|r|r.get(0))?;
    let cached:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM calendar_event_projections p JOIN remote_resources r ON r.resource_id=p.resource_id WHERE p.series_id=?1 AND p.recurrence_key=?2 AND r.lifecycle='active')",params![series,recurrence],|r|r.get(0))?;
    if pending || cached {
        Ok(None)
    } else {
        Ok(Some(Some(format!("calendar:{series}:{recurrence}"))))
    }
}
pub(super) fn detail(
    c: &Connection,
    identity: SearchIdentity,
) -> Result<crate::domain::search::SearchHit, PersistenceError> {
    use crate::domain::search::SearchHit;
    match identity {
        SearchIdentity::RemoteOnly { .. } => Err(PersistenceError::NotFound),
        SearchIdentity::Todo {
            todo_id,
            occurrence_key,
        } => {
            let visible: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM local_todos t JOIN local_calendars l ON l.calendar_id=t.local_calendar_id WHERE t.todo_id=?1 AND t.active=1 AND l.hidden=0)", [&todo_id], |r| r.get(0))?;
            if !visible {
                return Err(PersistenceError::NotFound);
            }
            Ok(SearchHit::Todo(get_occurrence(
                c,
                &todo_id,
                &occurrence_key,
            )?))
        }
        SearchIdentity::Calendar {
            series_id,
            resource_id,
            recurrence_key,
        } => list_calendar_agenda_items_selected(
            c,
            None,
            Some((&series_id, &resource_id, &recurrence_key)),
        )?
        .into_iter()
        .find(|v| {
            v.series_id == series_id
                && v.resource_id == resource_id
                && v.recurrence_key == recurrence_key
        })
        .map(SearchHit::Calendar)
        .ok_or(PersistenceError::NotFound),
    }
}
