//! Session-only range search. No synchronization promotion or notification APIs.
use crate::{
    adapters::{caldav_transport::DavCancellation, persistence::Database},
    application::account_registry::AccountSourceRegistry,
    domain::{
        calendar_event::{ProjectionStatus, normalize_vevent},
        calendar_recurrence::{ExpansionWindow, expand_vevent_recurrence},
        search_workspace::*,
    },
    ports::calendar_range::CalendarRangeRequest,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_SESSION_ROWS: usize = 10_000;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RangeInput {
    pub start_at_utc_ms: i64,
    pub end_at_utc_ms: i64,
}
impl RangeInput {
    pub fn validate(&self) -> Result<(), ()> {
        if self.start_at_utc_ms < 0
            || self.end_at_utc_ms <= self.start_at_utc_ms
            || self.end_at_utc_ms.saturating_sub(self.start_at_utc_ms) > 5 * 366 * 86_400_000
        {
            Err(())
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeView {
    pub task_id: String,
    pub state: &'static str,
    pub completed: usize,
    pub total: usize,
    pub unsupported: usize,
    pub range: RangeInput,
}
struct Segment {
    account: String,
    calendar: String,
    name: String,
    color: Option<String>,
    generation: u64,
    start: i64,
    end: i64,
}
struct CachedSegment {
    account: String,
    calendar: String,
    generation: u64,
    start: i64,
    end: i64,
    fetched: Instant,
    rows: Vec<SearchSummary>,
    bytes: usize,
    resources: Vec<(String, String, u64)>,
    unsupported: usize,
    capacity_exceeded: bool,
}
struct Task {
    view: RangeView,
    cancellation: DavCancellation,
    segments: Vec<Segment>,
    next: usize,
    in_flight: bool,
}
#[derive(Default)]
struct State {
    task: Option<Task>,
    cache: Vec<CachedSegment>,
    revision: Option<u64>,
}
#[derive(Clone, Default)]
pub struct RemoteSearch {
    state: Arc<Mutex<State>>,
}
impl RemoteSearch {
    pub fn cancel(&self) -> Result<(), ()> {
        let mut state = self.state.lock().map_err(|_| ())?;
        if let Some(task) = state.task.as_mut() {
            task.cancellation.cancel();
            task.view.state = "cancelled";
        }
        Ok(())
    }
    pub fn start(
        &self,
        input: RangeInput,
        db: &Database,
        registry: &AccountSourceRegistry,
    ) -> Result<RangeView, ()> {
        input.validate()?;
        let calendars = db.list_calendar_settings().map_err(|_| ())?;
        let mut segments = Vec::new();
        for calendar in calendars.into_iter().filter(|v| v.visible && v.available) {
            let Some(generation) = registry.attachment_generation(&calendar.account_id) else {
                continue;
            };
            let profile = registry.profile(&calendar.account_id).map_err(|_| ())?;
            if !profile.enabled || profile.region_profile != "icloud-cn" {
                continue;
            }
            let mut start = input.start_at_utc_ms;
            while start < input.end_at_utc_ms {
                let end = start
                    .saturating_add(90 * 86_400_000)
                    .min(input.end_at_utc_ms);
                segments.push(Segment {
                    account: calendar.account_id.clone(),
                    calendar: calendar.calendar_id.clone(),
                    name: calendar.display_name.clone(),
                    color: calendar.effective_color.clone(),
                    generation,
                    start,
                    end,
                });
                start = end;
            }
        }
        if segments.is_empty() || segments.len() > 512 {
            return Err(());
        }
        let view = RangeView {
            task_id: uuid::Uuid::new_v4().to_string(),
            state: "reading",
            completed: 0,
            total: segments.len(),
            unsupported: 0,
            range: input,
        };
        let mut state = self.state.lock().map_err(|_| ())?;
        if let Some(old) = state.task.take() {
            old.cancellation.cancel();
        }
        let revision = db.search_read_revision().map_err(|_| ())?;
        if state.revision != Some(revision) {
            state.cache.clear();
            state.revision = Some(revision);
        }
        prune(&mut state.cache, registry);
        state.task = Some(Task {
            view: view.clone(),
            cancellation: DavCancellation::new(),
            segments,
            next: 0,
            in_flight: false,
        });
        Ok(view)
    }
    pub fn step(
        &self,
        id: &str,
        db: &Database,
        registry: &AccountSourceRegistry,
    ) -> Result<RangeView, ()> {
        let (segment, cancellation) = {
            let mut state = self.state.lock().map_err(|_| ())?;
            prune(&mut state.cache, registry);
            let task = state
                .task
                .as_mut()
                .filter(|t| t.view.task_id == id)
                .ok_or(())?;
            if task.view.state != "reading" {
                return Ok(task.view.clone());
            }
            if task.next >= task.segments.len() {
                task.view.state = "complete";
                return Ok(task.view.clone());
            }
            if task.in_flight {
                return Err(());
            }
            task.in_flight = true;
            let value = &task.segments[task.next];
            (
                Segment {
                    account: value.account.clone(),
                    calendar: value.calendar.clone(),
                    name: value.name.clone(),
                    color: value.color.clone(),
                    generation: value.generation,
                    start: value.start,
                    end: value.end,
                },
                task.cancellation.clone(),
            )
        };
        let reused = {
            let state = self.state.lock().map_err(|_| ())?;
            state
                .cache
                .iter()
                .find(|v| {
                    v.account == segment.account
                        && v.calendar == segment.calendar
                        && v.generation == segment.generation
                        && v.start == segment.start
                        && v.end == segment.end
                })
                .map(|v| v.unsupported)
        };
        let result = if reused.is_some() {
            Ok(None)
        } else {
            self.read_segment(&segment, &cancellation, db, registry)
                .map(Some)
        };
        let revision = db.search_read_revision().map_err(|_| ())?;
        let mut state = self.state.lock().map_err(|_| ())?;
        let task = state
            .task
            .as_mut()
            .filter(|t| t.view.task_id == id)
            .ok_or(())?;
        task.in_flight = false;
        if task.cancellation.is_cancelled() {
            return Ok(task.view.clone());
        }
        if state.revision != Some(revision)
            || registry.attachment_generation(&segment.account) != Some(segment.generation)
        {
            state.cache.clear();
            state.revision = Some(revision);
            state.task.as_mut().ok_or(())?.view.state = "failed";
            return Ok(state.task.as_ref().ok_or(())?.view.clone());
        }
        match result {
            Ok(cached) => {
                if let Some((cached, unsupported)) = cached {
                    let alias = state
                        .cache
                        .iter()
                        .filter(|old| {
                            old.account == cached.account && old.calendar != cached.calendar
                        })
                        .any(|old| {
                            old.rows
                                .iter()
                                .any(|row| cached.rows.iter().any(|new| new.key == row.key))
                        });
                    let contradiction = state
                        .cache
                        .iter()
                        .filter(|old| {
                            old.account == cached.account && old.calendar == cached.calendar
                        })
                        .any(|old| {
                            old.resources.iter().any(|(href, etag, hash)| {
                                cached
                                    .resources
                                    .iter()
                                    .any(|(new_href, new_etag, new_hash)| {
                                        href == new_href && (etag != new_etag || hash != new_hash)
                                    })
                            })
                        });
                    if contradiction || alias {
                        state.cache.retain(|old| {
                            old.account != cached.account
                                || (!alias && old.calendar != cached.calendar)
                        });
                        state.task.as_mut().ok_or(())?.view.state = "failed";
                        return Ok(state.task.as_ref().ok_or(())?.view.clone());
                    }
                    let count =
                        state.cache.iter().map(|c| c.rows.len()).sum::<usize>() + cached.rows.len();
                    let bytes = state.cache.iter().map(|c| c.bytes).sum::<usize>() + cached.bytes;
                    let resources = state.cache.iter().map(|c| c.resources.len()).sum::<usize>()
                        + cached.resources.len();
                    if cached.capacity_exceeded
                        || count > MAX_SESSION_ROWS
                        || bytes > 32 * 1024 * 1024
                        || resources > 2000
                    {
                        state.task.as_mut().ok_or(())?.view.state = "partialCapacity";
                    } else {
                        state.cache.push(cached);
                        let task = state.task.as_mut().ok_or(())?;
                        task.view.unsupported += unsupported;
                    }
                }
                let task = state.task.as_mut().ok_or(())?;
                task.view.unsupported += reused.unwrap_or(0);
                if task.view.state == "reading" {
                    task.next += 1;
                    task.view.completed += 1;
                    if task.next == task.segments.len() {
                        task.view.state = "complete";
                    }
                }
            }
            Err(()) => state.task.as_mut().ok_or(())?.view.state = "failed",
        }
        Ok(state.task.as_ref().ok_or(())?.view.clone())
    }
    /// Explicit development observation of one already-bound Probe segment.
    /// The separate empty actor supplies only cache-key lookups; transport and
    /// generation ownership remain with the live, exact-bound registry.
    fn read_segment(
        &self,
        s: &Segment,
        cancellation: &DavCancellation,
        db: &Database,
        registry: &AccountSourceRegistry,
    ) -> Result<(CachedSegment, usize), ()> {
        let (generation, result) = registry
            .collect_range(
                &s.account,
                &CalendarRangeRequest {
                    calendar_id: s.calendar.clone(),
                    start_at_utc_ms: s.start,
                    end_at_utc_ms: s.end,
                    cancellation: cancellation.clone(),
                },
            )
            .map_err(|_| ())?;
        if generation != s.generation || result.calendar_id != s.calendar {
            return Err(());
        }
        let mut rows = BTreeMap::new();
        let mut capacity_exceeded = false;
        let mut bytes = 0;
        let mut resources = Vec::new();
        let mut uids = BTreeSet::new();
        let mut unsupported = 0;
        'resources: for resource in result.resources {
            bytes += resource.raw_ics.len();
            if bytes > 32 * 1024 * 1024 {
                return Err(());
            }
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            resource.raw_ics.hash(&mut hasher);
            resources.push((resource.href.clone(), resource.etag, hasher.finish()));
            let recurring =
                crate::domain::calendar_sync::extract_vevent_series_uid(&resource.raw_ics)
                    .map_err(|_| ())?;
            if recurring != resource.uid || !uids.insert(resource.uid.clone()) {
                return Err(());
            }
            let normalized = normalize_vevent(&resource.raw_ics, &resource.uid);
            let occurrences = if normalized.status == ProjectionStatus::Ready {
                if normalized.cancelled {
                    vec![]
                } else {
                    vec![(
                        "SINGLETON".to_string(),
                        normalized.title,
                        normalized.notes,
                        normalized.location,
                        normalized.start_at_utc_ms,
                        normalized.end_at_utc_ms,
                    )]
                }
            } else {
                match expand_vevent_recurrence(
                    &resource.raw_ics,
                    &resource.uid,
                    ExpansionWindow {
                        start_at_utc_ms: s.start,
                        end_at_utc_ms: s.end,
                        max_occurrences: 512,
                    },
                ) {
                    Ok(expanded) => expanded
                        .occurrences
                        .into_iter()
                        .filter(|v| !v.cancelled)
                        .map(|v| {
                            (
                                v.identity.canonical_key,
                                v.title,
                                v.notes,
                                v.location,
                                Some(v.effective_start_at_utc_ms),
                                Some(v.effective_end_at_utc_ms),
                            )
                        })
                        .collect(),
                    Err(_) => {
                        unsupported += 1;
                        continue;
                    }
                }
            };
            for (recurrence, title, notes, location, start, end) in occurrences {
                if !start.is_some_and(|v| v < s.end && end.unwrap_or(v.saturating_add(1)) > s.start)
                {
                    continue;
                }
                // This actor lookup suppresses durable/pending identities; session reads never overwrite them.
                let Some(key) = db
                    .search_remote_key(s.account.clone(), resource.uid.clone(), recurrence.clone())
                    .map_err(|_| ())?
                else {
                    continue;
                };
                let key = key.unwrap_or_else(|| {
                    format!("remote:{}:{}:{}", s.account, resource.uid, recurrence)
                });
                let token = uuid::Uuid::new_v4().to_string();
                if rows.len() == MAX_SESSION_ROWS && !rows.contains_key(&key) {
                    capacity_exceeded = true;
                    break 'resources;
                }
                rows.entry(key.clone()).or_insert(SearchSummary {
                    key,
                    identity: SearchIdentity::RemoteOnly { session_id: token },
                    calendar_id: s.calendar.clone(),
                    calendar_name: s.name.clone(),
                    calendar_color: s.color.clone(),
                    title,
                    notes,
                    location,
                    tags: vec![],
                    start_at_utc_ms: start,
                    end_at_utc_ms: end,
                    completed: false,
                });
            }
        }
        if cancellation.is_cancelled() {
            return Err(());
        }
        Ok((
            CachedSegment {
                account: s.account.clone(),
                calendar: s.calendar.clone(),
                generation: s.generation,
                start: s.start,
                end: s.end,
                fetched: Instant::now(),
                rows: rows.into_values().collect(),
                bytes,
                resources,
                unsupported,
                capacity_exceeded,
            },
            unsupported,
        ))
    }
    pub fn merge(
        &self,
        query: &WorkspaceQuery,
        page: &mut SearchPage,
        registry: &AccountSourceRegistry,
    ) -> Result<(), ()> {
        let terms = query.terms().map_err(|_| ())?;
        let mut state = self.state.lock().map_err(|_| ())?;
        if state.revision != Some(page.revision) {
            state.cache.clear();
            state.revision = Some(page.revision);
        }
        prune(&mut state.cache, registry);
        let mut unique = BTreeSet::new();
        let mut all = page.items.clone();
        for cached in &state.cache {
            for row in &cached.rows {
                if !unique.insert(row.key.clone())
                    || !row.matches(&query.preferences.fields, &terms)
                {
                    continue;
                }
                let position = row.position();
                let included = if let Some(cursor) = &query.cursor {
                    if query.direction == SearchDirection::Before {
                        position < cursor.position
                    } else {
                        position > cursor.position
                    }
                } else if query.direction == SearchDirection::Before {
                    position.time < query.anchor_utc_ms
                } else if query.direction == SearchDirection::NoTime {
                    row.start_at_utc_ms.is_none()
                } else {
                    position.time > query.anchor_utc_ms
                        || (position.time == query.anchor_utc_ms
                            && query
                                .anchor_key
                                .as_ref()
                                .is_none_or(|key| position.key >= *key))
                };
                let scanned = page.exhausted
                    || page.continuation.as_ref().is_some_and(|cursor| {
                        if query.direction == SearchDirection::Before {
                            position >= cursor.position
                        } else {
                            position <= cursor.position
                        }
                    });
                if included
                    && scanned
                    && (query.direction == SearchDirection::NoTime || row.start_at_utc_ms.is_some())
                    && !all.iter().any(|v| v.key == row.key)
                {
                    all.push(row.clone());
                }
            }
        }
        all.sort_by_key(SearchSummary::position);
        let extra = all.len() > 25;
        if query.direction == SearchDirection::Before {
            if extra {
                all.drain(..all.len() - 25);
            }
        } else {
            all.truncate(25);
        }
        if extra || (!page.exhausted && !all.is_empty()) {
            let last = if query.direction == SearchDirection::Before {
                &all[0]
            } else {
                all.last().ok_or(())?
            };
            page.continuation = Some(SearchCursor {
                position: last.position(),
                revision: page.revision,
                fingerprint: query.fingerprint(),
                direction: query.direction,
            });
        }
        page.items = all;
        page.exhausted &= !extra;
        Ok(())
    }
    pub fn clear(&self) -> Result<(), ()> {
        let mut state = self.state.lock().map_err(|_| ())?;
        state.cache.clear();
        Ok(())
    }
    pub fn detail(
        &self,
        id: &str,
        registry: &AccountSourceRegistry,
        db: &Database,
    ) -> Result<SearchSummary, ()> {
        let revision = db.search_read_revision().map_err(|_| ())?;
        let mut state = self.state.lock().map_err(|_| ())?;
        if state.revision != Some(revision) {
            state.cache.clear();
            state.revision = Some(revision);
        }
        prune(&mut state.cache, registry);
        state
            .cache
            .iter()
            .flat_map(|v| &v.rows)
            .find(
                |v| matches!(&v.identity,SearchIdentity::RemoteOnly{session_id} if session_id==id),
            )
            .cloned()
            .ok_or(())
    }
}
fn prune(cache: &mut Vec<CachedSegment>, registry: &AccountSourceRegistry) {
    cache.retain(|v| {
        v.fetched.elapsed() < Duration::from_secs(600)
            && registry.attachment_generation(&v.account) == Some(v.generation)
    });
}
