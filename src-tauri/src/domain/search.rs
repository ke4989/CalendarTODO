use super::{calendar_event::CalendarAgendaItem, local_todo::LocalTodo};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchQuery {
    pub text: String,
    #[serde(default = "include_completed")]
    pub include_completed: bool,
}
fn include_completed() -> bool {
    true
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "item", rename_all = "camelCase")]
pub enum SearchHit {
    Todo(LocalTodo),
    Calendar(CalendarAgendaItem),
}
impl SearchHit {
    pub fn fields(&self) -> Vec<&str> {
        match self {
            Self::Todo(v) => std::iter::once(v.title.as_str())
                .chain(v.notes.as_deref())
                .chain(v.location.as_deref())
                .chain(v.tags.iter().map(|t| t.name.as_str()))
                .collect(),
            Self::Calendar(v) => std::iter::once(v.title.as_str())
                .chain(v.notes.as_deref())
                .chain(v.location.as_deref())
                .chain(v.tags.iter().map(|t| t.name.as_str()))
                .collect(),
        }
    }
    pub fn completed(&self) -> bool {
        match self {
            Self::Todo(v) => v.completed,
            Self::Calendar(v) => v.completed,
        }
    }
    pub fn time(&self) -> Option<i64> {
        match self {
            Self::Todo(v) => v.start_at_utc_ms.or(v.due_at_utc_ms),
            Self::Calendar(v) => v.start_at_utc_ms,
        }
    }
    pub fn key(&self) -> String {
        match self {
            Self::Todo(v) => format!("todo:{}:{}", v.todo_id, v.occurrence_key),
            Self::Calendar(v) => format!("calendar:{}:{}", v.series_id, v.recurrence_key),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
}

pub fn normalize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if ('\u{ff01}'..='\u{ff5e}').contains(&c) {
                char::from_u32(c as u32 - 0xfee0).unwrap()
            } else {
                c
            }
        })
        .flat_map(char::to_lowercase)
        .collect()
}
pub fn terms(query: &SearchQuery) -> Result<Vec<String>, &'static str> {
    if query.text.chars().count() > 512 {
        return Err("query too long");
    }
    let terms = normalize(&query.text)
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if terms.len() > 8 || terms.iter().any(|v| v.chars().count() > 64) {
        return Err("query terms too long");
    }
    Ok(terms)
}
pub fn select(
    query: &SearchQuery,
    terms: &[String],
    hits: impl IntoIterator<Item = SearchHit>,
) -> SearchResults {
    let mut hits = hits
        .into_iter()
        .filter(|hit| {
            if !query.include_completed && hit.completed() {
                return false;
            }
            let fields = hit.fields().into_iter().map(normalize).collect::<Vec<_>>();
            terms
                .iter()
                .all(|term| fields.iter().any(|value| value.contains(term)))
        })
        .collect::<Vec<_>>();
    hits.sort_by_cached_key(|hit| (hit.time().unwrap_or(i64::MAX), hit.key()));
    let truncated = hits.len() > 200;
    hits.truncate(200);
    SearchResults { hits, truncated }
}
