//! Bounded namespace-aware response stream. Nonmatching responses are dropped
//! before the next response; only the two exact locators escape this module.
use crate::adapters::caldav_transport::{DavTransportPolicy, RawDavResponse};
use quick_xml::{NsReader, events::Event, name::ResolveResult};
use url::Url;
const DAV: &str = "DAV:";
const CAL: &str = "urn:ietf:params:xml:ns:caldav";
struct Node {
    name: String,
    text: String,
    children: Vec<Node>,
}
impl Node {
    fn child(&self, name: &str) -> Result<&Self, ()> {
        let mut nodes = self.children.iter().filter(|n| n.name == name);
        let node = nodes.next().ok_or(())?;
        if nodes.next().is_some() {
            return Err(());
        }
        Ok(node)
    }
    fn shape(&self, names: &[&str]) -> Result<(), ()> {
        if !self.text.trim().is_empty() || self.children.len() != names.len() {
            return Err(());
        }
        for name in names {
            self.child(name)?;
        }
        Ok(())
    }
    fn leaf(&self) -> Result<&str, ()> {
        if self.children.is_empty() {
            Ok(&self.text)
        } else {
            Err(())
        }
    }
}
fn name(ns: ResolveResult<'_>, local: &str) -> Result<String, ()> {
    let ResolveResult::Bound(ns) = ns else {
        return Err(());
    };

    if (ns.as_ref() == DAV
        && [
            "multistatus",
            "response",
            "href",
            "propstat",
            "prop",
            "status",
            "displayname",
            "resourcetype",
            "collection",
            "current-user-privilege-set",
            "privilege",
            "read",
            "write",
            "write-properties",
            "write-content",
            "bind",
            "unbind",
            "current-user-principal",
        ]
        .contains(&local))
        || (ns.as_ref() == CAL && ["calendar", "calendar-home-set"].contains(&local))
    {
        Ok(local.into())
    } else {
        Err(())
    }
}
fn stream(
    raw: &RawDavResponse,
    mut response: impl FnMut(Node) -> Result<(), ()>,
) -> Result<(), ()> {
    if raw.status() != 207
        || raw.location().is_some()
        || raw.body().is_empty()
        || raw.body().len() > 64 * 1024
    {
        return Err(());
    }
    let content = raw.content_type().ok_or(())?;
    if content.len() > 128
        || ![
            "application/xml",
            "text/xml",
            "application/xml; charset=utf-8",
            "text/xml; charset=utf-8",
        ]
        .iter()
        .any(|v| content.eq_ignore_ascii_case(v))
    {
        return Err(());
    }
    let text = std::str::from_utf8(raw.body()).map_err(|_| ())?;
    if text.chars().any(|c| !matches!(c, '\u{9}' | '\u{a}' | '\u{d}' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')) { return Err(()) }
    let mut reader = NsReader::from_reader(raw.body());
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Node> = Vec::new();
    let (mut nodes, mut responses, mut closed, mut declaration) = (0, 0, false, false);
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| ())?;
        match event {
            Event::Start(event) => {
                if closed || stack.len() >= 8 {
                    return Err(());
                }
                nodes += 1;
                if nodes > 4096 {
                    return Err(());
                }
                for attr in event.attributes() {
                    let attr = attr.map_err(|_| ())?;
                    if !(attr.key.as_ref() == "xmlns" || attr.key.as_ref().starts_with("xmlns:"))
                        || ![DAV, CAL].contains(&attr.value.as_ref())
                    {
                        return Err(());
                    }
                }
                let name = name(ns, event.local_name().as_ref())?;
                if (stack.is_empty() && name != "multistatus")
                    || (stack.len() == 1 && name != "response")
                {
                    return Err(());
                }
                stack.push(Node {
                    name,
                    text: String::new(),
                    children: Vec::new(),
                });
            }
            Event::End(event) => {
                let node = stack.pop().ok_or(())?;
                if node.name != name(ns, event.local_name().as_ref())? {
                    return Err(());
                }
                if stack.len() == 1 {
                    responses += 1;
                    if responses > 128 {
                        return Err(());
                    }
                    response(node)?;
                } else if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    if !node.text.trim().is_empty() {
                        return Err(());
                    }
                    closed = true;
                }
            }
            Event::Text(text) => {
                let value = text.as_ref();
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(value);
                } else if !value.trim().is_empty() {
                    return Err(());
                }
            }
            Event::Decl(event) if !declaration && nodes == 0 => {
                if ![
                    "xml version=\"1.0\"",
                    "xml version=\"1.0\" encoding=\"UTF-8\"",
                    "xml version=\"1.0\" encoding=\"utf-8\"",
                ]
                .contains(&event.as_ref())
                {
                    return Err(());
                }
                declaration = true;
            }
            Event::Eof => break,
            _ => return Err(()),
        }
    }
    if !closed || !stack.is_empty() || responses == 0 {
        return Err(());
    }
    Ok(())
}
fn properties(response: &Node) -> Result<(&str, &Node), ()> {
    response.shape(&["href", "propstat"])?;
    let propstat = response.child("propstat")?;
    propstat.shape(&["prop", "status"])?;
    if !["HTTP/1.1 200 OK", "HTTP/1.0 200 OK"].contains(&propstat.child("status")?.leaf()?.trim()) {
        return Err(());
    }
    Ok((response.child("href")?.leaf()?, propstat.child("prop")?))
}
fn target(base: &Url, href: &str, policy: &DavTransportPolicy) -> Result<Url, ()> {
    let url = base.join(href).map_err(|_| ())?;
    if !policy.probe_collection_matches(base, href)
        || !crate::adapters::persistence::probe_target::safe_route(url.path())
    {
        return Err(());
    }
    Ok(url)
}
pub(super) enum Resolution {
    Principal,
    Home,
}
pub(super) fn resolve(
    raw: &RawDavResponse,
    base: &Url,
    resolution: Resolution,
    policy: &DavTransportPolicy,
) -> Result<Url, ()> {
    let mut found = None;
    stream(raw, |response| {
        if found.is_some() {
            return Err(());
        }
        let (href, prop) = properties(&response)?;
        if href != base.path() && href != base.as_str() {
            return Err(());
        }
        let property = match resolution {
            Resolution::Principal => "current-user-principal",
            Resolution::Home => "calendar-home-set",
        };
        prop.shape(&[property])?;
        let value = prop.child(property)?;
        value.shape(&["href"])?;
        found = Some(target(base, value.child("href")?.leaf()?, policy)?);
        Ok(())
    })?;
    found.ok_or(())
}
pub(super) fn locate(
    raw: &RawDavResponse,
    home: &Url,
    policy: &DavTransportPolicy,
) -> Result<[Url; 2], ()> {
    let mut slots = [None, None];
    stream(raw, |response| {
        let (_, prop) = properties(&response)?;
        prop.shape(&["displayname", "resourcetype", "current-user-privilege-set"])?;
        let name = prop.child("displayname")?.leaf()?;
        let index = match name {
            "Calendar TODO Probe" => 0,
            "Calendar TODO Probe 2" => 1,
            _ => return Ok(()),
        };
        if slots[index].is_some() {
            return Err(());
        }
        let url = target(home, response.child("href")?.leaf()?, policy)?;
        // The selected calendar must be an immediate child of this home.
        let suffix = url.path().strip_prefix(home.path()).ok_or(())?;
        if suffix.is_empty() || !suffix.ends_with('/') || suffix[..suffix.len() - 1].contains('/') {
            return Err(());
        }
        prop.child("resourcetype")?
            .shape(&["collection", "calendar"])?;
        slots[index] = Some(url);
        Ok(())
    })?;
    let [first, second] = slots;
    let first = first.ok_or(())?;
    let second = second.ok_or(())?;
    if first == second {
        return Err(());
    }
    Ok([first, second])
}
