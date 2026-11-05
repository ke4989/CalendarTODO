//! Actor-owned committed target candidates, not Probe source/session authority.
//! No persisted schema currently attests discovery's concrete session owner.

// No Clone, Debug, Serialize, public fields or caller-controlled constructor.

// Product routing is captured separately from diagnostic target candidates.
// This is a current committed pair, never historical discovery/socket proof.

pub(in crate::adapters) fn safe_route(route: &str) -> bool {
    if route.contains(['%', '\\', '?', '#'])
        || route.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return false;
    }
    let path = if route.starts_with('/') && !route.starts_with("//") {
        route
    } else {
        let Ok(url) = url::Url::parse(route) else {
            return false;
        };
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.host_str().is_none()
            || url.port_or_known_default() != Some(443)
            || url.as_str() != route
        {
            return false;
        }
        return safe_path(url.path());
    };
    safe_path(path)
}

fn safe_path(path: &str) -> bool {
    path.starts_with('/')
        && path != "/"
        && path.ends_with('/')
        && !path.contains("//")
        && !path.split('/').any(|s| s == "." || s == "..")
}
