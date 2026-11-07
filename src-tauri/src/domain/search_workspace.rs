//! Bounded read-only search contracts. Identities and cursors confer no write authority.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchPreferences {
    pub fields: Vec<SearchField>,
    pub include_completed: bool,
}
impl SearchPreferences {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.fields.is_empty() || self.fields.len() > 4 {
            return Err("invalid search fields");
        }
        for (i, field) in self.fields.iter().enumerate() {
            if self.fields[..i].contains(field) {
                return Err("duplicate search field");
            }
        }
        Ok(())
    }
}
impl Default for SearchPreferences {
    fn default() -> Self {
        Self {
            fields: vec![SearchField::Title],
            include_completed: true,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SearchField {
    Title,
    Notes,
    Location,
    Tags,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SearchDirection {
    Before,
    After,
    NoTime,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchPosition {
    pub time: i64,
    pub key: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchCursor {
    pub position: SearchPosition,
    pub revision: u64,
    pub fingerprint: String,
    pub direction: SearchDirection,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceQuery {
    pub text: String,
    pub preferences: SearchPreferences,
    pub anchor_utc_ms: i64,
    #[serde(default)]
    pub anchor_key: Option<String>,
    pub direction: SearchDirection,
    pub cursor: Option<SearchCursor>,
}
impl WorkspaceQuery {
    pub fn terms(&self) -> Result<Vec<String>, &'static str> {
        self.preferences.validate()?;
        crate::domain::search::terms(&crate::domain::search::SearchQuery {
            text: self.text.clone(),
            include_completed: self.preferences.include_completed,
        })
    }
    /// Keywords start across the full cached timeline; an explicit identity
    /// anchor still pins a viewport refresh, and cursors take precedence.
    pub fn initial_time(&self) -> i64 {
        if self.direction == SearchDirection::NoTime {
            9_007_199_254_740_991
        } else if self.direction == SearchDirection::After
            && self.anchor_key.is_none()
            && !self.text.trim().is_empty()
        {
            -9_007_199_254_740_991
        } else {
            self.anchor_utc_ms
        }
    }
    pub fn includes_untimed(&self) -> bool {
        self.direction == SearchDirection::NoTime || !self.text.trim().is_empty()
    }
    pub fn fingerprint(&self) -> String {
        let mask = self
            .preferences
            .fields
            .iter()
            .fold(0u8, |mask, field| mask | (1 << *field as u8));
        format!(
            "{}:{}:{}",
            mask,
            self.preferences.include_completed,
            crate::domain::search::normalize(&self.text)
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum SearchIdentity {
    RemoteOnly {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Todo {
        #[serde(rename = "todoId")]
        todo_id: String,
        #[serde(rename = "occurrenceKey")]
        occurrence_key: String,
    },
    Calendar {
        #[serde(rename = "seriesId")]
        series_id: String,
        #[serde(rename = "resourceId")]
        resource_id: String,
        #[serde(rename = "recurrenceKey")]
        recurrence_key: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSummary {
    pub key: String,
    pub identity: SearchIdentity,
    pub calendar_id: String,
    pub calendar_name: String,
    pub calendar_color: Option<String>,
    pub title: String,
    pub notes: Option<String>,
    pub location: Option<String>,
    pub tags: Vec<String>,
    pub start_at_utc_ms: Option<i64>,
    pub end_at_utc_ms: Option<i64>,
    pub completed: bool,
}
impl SearchSummary {
    pub fn position(&self) -> SearchPosition {
        SearchPosition {
            time: self.start_at_utc_ms.unwrap_or(9_007_199_254_740_991),
            key: self.key.clone(),
        }
    }
    pub fn matches(&self, fields: &[SearchField], terms: &[String]) -> bool {
        let values = fields
            .iter()
            .flat_map(|field| match field {
                SearchField::Title => vec![self.title.as_str()],
                SearchField::Notes => self.notes.as_deref().into_iter().collect(),
                SearchField::Location => self.location.as_deref().into_iter().collect(),
                SearchField::Tags => self.tags.iter().map(String::as_str).collect(),
            })
            .map(crate::domain::search::normalize)
            .collect::<Vec<_>>();
        terms
            .iter()
            .all(|term| values.iter().any(|value| value.contains(term)))
    }
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPage {
    pub items: Vec<SearchSummary>,
    pub continuation: Option<SearchCursor>,
    pub exhausted: bool,
    pub stale: bool,
    pub revision: u64,
    pub scanned: usize,
}
