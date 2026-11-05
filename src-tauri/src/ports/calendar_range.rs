//! Partial range observations, deliberately not an inventory or synchronization stage.
use crate::adapters::caldav_transport::DavCancellation;
#[derive(Clone)]
pub struct CalendarRangeRequest {
    pub calendar_id: String,
    pub start_at_utc_ms: i64,
    pub end_at_utc_ms: i64,
    pub cancellation: DavCancellation,
}
pub struct CalendarRangeResource {
    pub href: String,
    pub etag: String,
    pub uid: String,
    pub raw_ics: Vec<u8>,
}
pub struct CalendarRangeSegment {
    pub calendar_id: String,
    pub resources: Vec<CalendarRangeResource>,
}
