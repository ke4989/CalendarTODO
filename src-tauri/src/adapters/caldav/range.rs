use super::*;
use crate::domain::calendar_recurrence::format_utc_timestamp;
use crate::ports::calendar_range::*;

pub(super) fn collect(
    source: &ExecutingCalDavAdapter,
    request: &CalendarRangeRequest,
) -> Result<CalendarRangeSegment, ProviderReadError> {
    if request.end_at_utc_ms <= request.start_at_utc_ms
        || request
            .end_at_utc_ms
            .saturating_sub(request.start_at_utc_ms)
            > 90 * 86_400_000
        || request.cancellation.is_cancelled()
    {
        return Err(ProviderReadError::UnsafeData);
    }
    // This feature only uses an already bound source; never principal/home discovery.
    if source.icloud_binding.is_none() {
        return Err(ProviderReadError::UnsafeData);
    }
    let context = source.scoped_context()?;
    let route = context
        .calendar_ids_by_href
        .iter()
        .find(|(_, id)| **id == request.calendar_id)
        .map(|(route, _)| route)
        .ok_or(ProviderReadError::UnsafeData)?;
    let collection = source
        .entry_url
        .join(route)
        .map_err(|_| ProviderReadError::UnsafeData)?;
    let body = query_body(request.start_at_utc_ms, request.end_at_utc_ms)?;
    let report = source.execute_range(
        DavMethod::Report,
        collection.clone(),
        body,
        DavDepth::One,
        &request.cancellation,
    )?;
    if report.status() != 207 {
        return Err(if report.status() == 401 || report.status() == 403 {
            ProviderReadError::Unauthorized
        } else {
            ProviderReadError::Incomplete
        });
    }
    let members = parse(report.body(), collection.as_str())?;
    let mut resources = Vec::new();
    for (href, etag, calendar_data) in members {
        if request.cancellation.is_cancelled() {
            return Err(ProviderReadError::Transient);
        }
        let (raw_ics, revision) = if let Some(raw) = calendar_data {
            (raw, etag)
        } else {
            let target = crate::adapters::icloud_caldav::resolve(&collection, &href)
                .map_err(map_execution_error)?;
            let response = source.execute_range(
                DavMethod::Get,
                target,
                vec![],
                DavDepth::Zero,
                &request.cancellation,
            )?;
            require_resource_status(response.status())?;
            (
                response.body().to_vec(),
                staged_revision(response.etag(), &etag)?,
            )
        };
        if raw_ics.is_empty() || raw_ics.len() > 4 * 1024 * 1024 {
            return Err(ProviderReadError::UnsafeData);
        }
        let uid = extract_single_uid(&raw_ics)?;
        resources.push(CalendarRangeResource {
            href,
            etag: revision,
            uid,
            raw_ics,
        });
        if resources.len() > 2000 {
            return Err(ProviderReadError::Incomplete);
        }
    }
    Ok(CalendarRangeSegment {
        calendar_id: request.calendar_id.clone(),
        resources,
    })
}
pub(super) fn query_body(start: i64, end: i64) -> Result<Vec<u8>, ProviderReadError> {
    let start = format_utc_timestamp(start).map_err(|_| ProviderReadError::UnsafeData)?;
    let end = format_utc_timestamp(end).map_err(|_| ProviderReadError::UnsafeData)?;
    Ok(format!("<c:calendar-query xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name=\"VCALENDAR\"><c:comp-filter name=\"VEVENT\"><c:time-range start=\"{start}\" end=\"{end}\"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>").into_bytes())
}
#[derive(Default)]
struct Member {
    href: Option<String>,
    status: Option<u16>,
    etag: Option<String>,
    data: Option<Vec<u8>>,
    prop_status: Option<u16>,
    prop_etag: Option<String>,
    prop_data: Option<Vec<u8>>,
}
fn parse(
    bytes: &[u8],
    collection: &str,
) -> Result<Vec<(String, String, Option<Vec<u8>>)>, ProviderReadError> {
    let mut reader = DavElementReader::new(bytes);
    let mut stack = Vec::new();
    let mut current = None::<Member>;
    let mut output = Vec::new();
    let mut seen = BTreeSet::new();
    let mut root = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = local_name(event.local_name().as_ref())?;
                if stack.is_empty() {
                    if name != "multistatus" || root {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    root = true;
                }
                if stack.last().is_some_and(|name| name == "calendar-data") {
                    return Err(ProviderReadError::UnsafeData);
                }
                if matches!(name.as_str(), "getetag" | "calendar-data")
                    && (stack.last().map(String::as_str) != Some("prop")
                        || !stack.iter().any(|name| name == "propstat"))
                {
                    return Err(ProviderReadError::UnsafeData);
                }
                if name == "response" {
                    if current.is_some() {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    current = Some(Member::default());
                }
                stack.push(name);
            }
            Ok(Event::Text(text)) => {
                if let Some(row) = current.as_mut() {
                    let value = text.as_ref().to_owned();
                    match stack.last().map(String::as_str) {
                        Some("href") => {
                            if row.href.replace(value).is_some() {
                                return Err(ProviderReadError::UnsafeData);
                            }
                        }
                        Some("getetag") => {
                            if row.prop_etag.replace(value).is_some() {
                                return Err(ProviderReadError::UnsafeData);
                            }
                        }
                        Some("calendar-data") => {
                            if row.prop_data.replace(value.into_bytes()).is_some() {
                                return Err(ProviderReadError::UnsafeData);
                            }
                        }
                        Some("status") => {
                            let target = if stack.iter().any(|v| v == "propstat") {
                                &mut row.prop_status
                            } else {
                                &mut row.status
                            };
                            if target.replace(dav_status_code(&value)).is_some() {
                                return Err(ProviderReadError::UnsafeData);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::End(event)) => {
                let name = local_name(event.local_name().as_ref())?;
                if stack.pop().as_deref() != Some(&name) {
                    return Err(ProviderReadError::UnsafeData);
                }
                if name == "propstat" {
                    let row = current.as_mut().ok_or(ProviderReadError::UnsafeData)?;
                    if row.prop_status != Some(200) {
                        return Err(ProviderReadError::Incomplete);
                    }
                    // Inline bytes and their revision must share the same
                    // successful property evidence; never assemble a pair
                    // across independent propstats.
                    if row.prop_data.is_some() && row.prop_etag.is_none() {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    if let Some(etag) = row.prop_etag.take() {
                        if row.etag.replace(etag).is_some() {
                            return Err(ProviderReadError::UnsafeData);
                        }
                    }
                    if let Some(data) = row.prop_data.take() {
                        if row.data.replace(data).is_some() {
                            return Err(ProviderReadError::UnsafeData);
                        }
                    }
                    row.prop_status = None;
                }
                if name == "response" {
                    let row = current.take().ok_or(ProviderReadError::UnsafeData)?;
                    if row.status.is_some_and(|v| v != 200) {
                        return Err(ProviderReadError::Incomplete);
                    }
                    let href = normalize_icloud_response_href(
                        collection,
                        &row.href.ok_or(ProviderReadError::Incomplete)?,
                    )?;
                    let etag = row.etag.ok_or(ProviderReadError::Incomplete)?;
                    if !resource_belongs_to_collection(collection, &href)
                        || !seen.insert(href.clone())
                        || etag.is_empty()
                        || etag.len() > 1024
                        || etag.chars().any(char::is_control)
                    {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    output.push((href, etag, row.data));
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(ProviderReadError::UnsafeData),
            _ => {}
        }
    }
    if !root || !stack.is_empty() || current.is_some() {
        return Err(ProviderReadError::UnsafeData);
    }
    Ok(output)
}
