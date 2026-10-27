//! Exact iCloud destination syntax and account discovery binding.
use super::caldav_transport::DavExecutionError;
use std::fmt;
use url::{Host, Url};

pub(crate) const BOOTSTRAP: &str = "https://caldav.icloud.com/";

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn bind_writable_fixture(
    database: &crate::adapters::persistence::Database,
    account: &str,
    host: &str,
) {
    let binding = ICloudBinding {
        bootstrap_url: BOOTSTRAP.into(),
        principal_url: format!("https://{host}/principal/"),
        home_url: format!("https://{host}/"),
        generation: 0,
    };
    let routes = database
        .available_calendar_ids_by_url(account.into())
        .unwrap();
    let settings = database.list_calendar_settings().unwrap();
    let calendars = routes
        .into_iter()
        .map(|(route, id)| {
            let setting = settings.iter().find(|s| s.calendar_id == id).unwrap();
            crate::adapters::persistence::DiscoveredCalendarRecord {
                calendar_id: id,
                collection_url: binding.collection(&route).unwrap().to_string(),
                display_name: setting.display_name.clone(),
                server_color: setting.server_color.clone(),
                privileges: "read,write-content".into(),
            }
        })
        .collect();
    database
        .replace_icloud_discovery(account.into(), calendars, binding, 1)
        .unwrap();
}

pub(crate) fn trusted_host(host: &str) -> bool {
    if matches!(host, "caldav.icloud.com" | "caldav.icloud.com.cn") {
        return true;
    }
    // Apple numbers shards `p` + 1..=3 ASCII digits (for example `p7`, `p34`,
    // `p235`). The suffix is anchored, so no other label can qualify.
    let Some(rest) = host.strip_prefix('p') else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    (1..=3).contains(&digits)
        && matches!(
            &rest[digits..],
            "-caldav.icloud.com" | "-caldav.icloud.com.cn"
        )
}

/// Raw authority spelling accepted before URL normalization. A port is allowed
/// only as the exact HTTPS default `:443`, which the URL parser then drops, so
/// every stored or compared URL stays portless. Any other port form, userinfo,
/// encoding or IP-literal spelling is rejected.
fn authority_spelling_is_safe(authority: &str) -> bool {
    if authority.is_empty()
        || !authority.is_ascii()
        || authority.contains(['%', '@', '?', '#', '[', ']'])
    {
        return false;
    }
    match authority.split_once(':') {
        None => true,
        Some((host, port)) => !host.is_empty() && port == "443",
    }
}

pub(crate) fn validate_url(url: &Url) -> Result<(), DavExecutionError> {
    if url.as_str().len() > 4096
        || url.scheme() != "https"
        || !matches!(url.host(), Some(Host::Domain(host)) if trusted_host(host))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(DavExecutionError::InvalidRequest);
    }
    Ok(())
}

/// Inspect spelling before URL normalization can hide authority/path syntax.
pub(crate) fn resolve(base: &Url, reference: &str) -> Result<Url, DavExecutionError> {
    if reference.is_empty()
        || reference.len() > 4096
        || reference
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        || reference.contains('\\')
    {
        return Err(DavExecutionError::InvalidRequest);
    }
    let lower = reference.to_ascii_lowercase();
    if ["%2f", "%5c", "%2e", "%3f", "%23", "%00", "%25"]
        .iter()
        .any(|s| lower.contains(s))
        || reference.split('/').any(|s| matches!(s, "." | ".."))
    {
        return Err(DavExecutionError::InvalidRequest);
    }
    let authority = if let Some((_, rest)) = reference.split_once("://") {
        Some(rest.split('/').next().unwrap_or(rest))
    } else {
        reference
            .strip_prefix("//")
            .map(|rest| rest.split('/').next().unwrap_or(rest))
    };
    if authority.is_some_and(|a| !authority_spelling_is_safe(a)) {
        return Err(DavExecutionError::InvalidRequest);
    }
    let url = base
        .join(reference)
        .map_err(|_| DavExecutionError::InvalidRequest)?;
    validate_url(&url)?;
    Ok(url)
}

pub(crate) fn parse(reference: &str) -> Result<Url, DavExecutionError> {
    let base = Url::parse(BOOTSTRAP).expect("fixed iCloud entry");
    if !reference.to_ascii_lowercase().starts_with("https://") {
        return Err(DavExecutionError::InvalidRequest);
    }
    resolve(&base, reference)
}

#[derive(Clone, PartialEq, Eq)]
pub struct ICloudBinding {
    pub bootstrap_url: String,
    pub principal_url: String,
    pub home_url: String,
    pub generation: i64,
}
impl fmt::Debug for ICloudBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ICloudBinding([REDACTED])")
    }
}
impl ICloudBinding {
    pub(crate) fn validate(&self) -> Result<Url, DavExecutionError> {
        let bootstrap = parse(&self.bootstrap_url)?;
        if !matches!(
            bootstrap.host_str(),
            Some("caldav.icloud.com" | "caldav.icloud.com.cn")
        ) || bootstrap.path() != "/"
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        parse(&self.principal_url)?;
        let home = parse(&self.home_url)?;
        if !home.path().ends_with('/') || self.generation < 0 {
            return Err(DavExecutionError::InvalidRequest);
        }
        Ok(home)
    }
    pub(crate) fn collection(&self, route: &str) -> Result<Url, DavExecutionError> {
        let home = self.validate()?;
        let collection = resolve(&home, route)?;
        if collection.origin() != home.origin()
            || !collection.path().ends_with('/')
            || !collection
                .path()
                .strip_prefix(home.path())
                .is_some_and(|tail| !tail.is_empty())
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        Ok(collection)
    }
}
