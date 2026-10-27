//! Strict offline subset, independent of account-wide discovery parsing.
//! Parsed facts never authorize attachment, credentials, transport or writes.

use super::Rejection;
use quick_xml::{NsReader, events::Event, name::ResolveResult};
use url::Url;

const MAX_BODY: usize = 16 * 1024;
const MAX_HEADER: usize = 128;
const MAX_NODES: usize = 64;
const MAX_DEPTH: usize = 8;
const DAV: &str = "DAV:";
const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";

// Deliberately neither Clone/Debug/Serialize nor production qualification.
pub(super) struct MetadataFacts {
    pub(super) read: bool,
    pub(super) write_content: bool,
    pub(super) bind: bool,
    pub(super) unbind: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Multistatus,
    Response,
    Href,
    Propstat,
    Prop,
    Status,
    Displayname,
    Resourcetype,
    Collection,
    Calendar,
    Privileges,
    Privilege,
    Read,
    Write,
    WriteProperties,
    WriteContent,
    Bind,
    Unbind,
}

struct Node {
    tag: Tag,
    text: String,
    children: Vec<Node>,
}

fn tag(namespace: ResolveResult<'_>, local: &str) -> Result<Tag, Rejection> {
    let ResolveResult::Bound(namespace) = namespace else {
        return Err(Rejection::Preflight);
    };
    if namespace.as_ref() == CALDAV && local == "calendar" {
        return Ok(Tag::Calendar);
    }
    if namespace.as_ref() != DAV {
        return Err(Rejection::Preflight);
    }
    Ok(match local {
        "multistatus" => Tag::Multistatus,
        "response" => Tag::Response,
        "href" => Tag::Href,
        "propstat" => Tag::Propstat,
        "prop" => Tag::Prop,
        "status" => Tag::Status,
        "displayname" => Tag::Displayname,
        "resourcetype" => Tag::Resourcetype,
        "collection" => Tag::Collection,
        "current-user-privilege-set" => Tag::Privileges,
        "privilege" => Tag::Privilege,
        "read" => Tag::Read,
        "write" => Tag::Write,
        "write-properties" => Tag::WriteProperties,
        "write-content" => Tag::WriteContent,
        "bind" => Tag::Bind,
        "unbind" => Tag::Unbind,
        _ => return Err(Rejection::Preflight),
    })
}

fn attributes(event: &quick_xml::events::BytesStart<'_>) -> Result<(), Rejection> {
    for attribute in event.attributes() {
        let attribute = attribute.map_err(|_| Rejection::Preflight)?;
        let key = attribute.key.as_ref();
        if !(key == "xmlns" || key.starts_with("xmlns:"))
            || ![DAV, CALDAV].contains(&attribute.value.as_ref())
        {
            return Err(Rejection::Preflight);
        }
    }
    Ok(())
}

fn append(node: Node, stack: &mut [Node], root: &mut Option<Node>) -> Result<(), Rejection> {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else if root.replace(node).is_some() {
        return Err(Rejection::Preflight);
    }
    Ok(())
}

fn document(body: &[u8]) -> Result<Node, Rejection> {
    let mut reader = NsReader::from_reader(body);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    let mut count = 0;
    let mut declaration = false;
    loop {
        let (namespace, event) = reader
            .read_resolved_event()
            .map_err(|_| Rejection::Preflight)?;
        match event {
            Event::Start(event) => {
                attributes(&event)?;
                count += 1;
                if count > MAX_NODES
                    || stack.len() >= MAX_DEPTH
                    || (stack.is_empty() && root.is_some())
                {
                    return Err(Rejection::Preflight);
                }
                stack.push(Node {
                    tag: tag(namespace, event.local_name().as_ref())?,
                    text: String::new(),
                    children: Vec::new(),
                });
            }
            Event::End(event) => {
                let node = stack.pop().ok_or(Rejection::Preflight)?;
                if node.tag != tag(namespace, event.local_name().as_ref())? {
                    return Err(Rejection::Preflight);
                }
                append(node, &mut stack, &mut root)?;
            }
            Event::Text(text) => {
                let value = text.as_ref();
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(value);
                } else if !value.trim().is_empty() {
                    return Err(Rejection::Preflight);
                }
            }
            Event::Decl(event) if !declaration && count == 0 => {
                // A deliberately small UTF-8/XML-1.0 declaration envelope.
                if ![
                    "xml version=\"1.0\"",
                    "xml version=\"1.0\" encoding=\"UTF-8\"",
                    "xml version=\"1.0\" encoding=\"utf-8\"",
                ]
                .contains(&event.as_ref())
                {
                    return Err(Rejection::Preflight);
                }
                declaration = true;
            }
            Event::Eof => break,
            // Includes DOCTYPE, GeneralRef (even internal/numeric entities),
            // PI, comments and CDATA. No external resolution is possible.
            _ => return Err(Rejection::Preflight),
        }
    }
    if !stack.is_empty() {
        return Err(Rejection::Preflight);
    }
    root.ok_or(Rejection::Preflight)
}

fn children(node: &Node, expected: &[Tag]) -> Result<(), Rejection> {
    if !node.text.trim().is_empty()
        || node.children.len() != expected.len()
        || expected
            .iter()
            .any(|tag| node.children.iter().filter(|n| n.tag == *tag).count() != 1)
    {
        return Err(Rejection::Preflight);
    }
    Ok(())
}

fn child(node: &Node, tag: Tag) -> Result<&Node, Rejection> {
    node.children
        .iter()
        .find(|n| n.tag == tag)
        .ok_or(Rejection::Preflight)
}

fn leaf(node: &Node) -> Result<&str, Rejection> {
    if !node.children.is_empty() {
        return Err(Rejection::Preflight);
    }
    Ok(&node.text)
}

pub(super) fn parse(
    body: &[u8],
    content_type: &str,
    target: &Url,
    name: &str,
) -> Result<MetadataFacts, Rejection> {
    if body.is_empty()
        || body.len() > MAX_BODY
        || content_type.len() > MAX_HEADER
        || content_type.chars().any(|c| c.is_control())
        || ![
            "application/xml",
            "text/xml",
            "application/xml; charset=utf-8",
            "text/xml; charset=utf-8",
        ]
        .iter()
        .any(|v| content_type.eq_ignore_ascii_case(v))
    {
        return Err(Rejection::Preflight);
    }
    let text = std::str::from_utf8(body).map_err(|_| Rejection::Preflight)?;
    if text.chars().any(|c| !matches!(c, '\u{9}' | '\u{a}' | '\u{d}' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')) {
        return Err(Rejection::Preflight);
    }
    let root = document(body)?;
    if root.tag != Tag::Multistatus {
        return Err(Rejection::Preflight);
    }
    children(&root, &[Tag::Response])?;
    let response = child(&root, Tag::Response)?;
    children(response, &[Tag::Href, Tag::Propstat])?;
    let href = leaf(child(response, Tag::Href)?)?;
    if href != target.path() && href != target.as_str() {
        return Err(Rejection::Preflight);
    }
    let propstat = child(response, Tag::Propstat)?;
    children(propstat, &[Tag::Prop, Tag::Status])?;
    if !["HTTP/1.1 200 OK", "HTTP/1.0 200 OK"]
        .contains(&leaf(child(propstat, Tag::Status)?)?.trim())
    {
        return Err(Rejection::Preflight);
    }
    let prop = child(propstat, Tag::Prop)?;
    children(
        prop,
        &[Tag::Displayname, Tag::Resourcetype, Tag::Privileges],
    )?;
    if leaf(child(prop, Tag::Displayname)?)? != name {
        return Err(Rejection::Preflight);
    }
    let resource_type = child(prop, Tag::Resourcetype)?;
    children(resource_type, &[Tag::Collection, Tag::Calendar])?;
    for node in &resource_type.children {
        if !leaf(node)?.trim().is_empty() {
            return Err(Rejection::Preflight);
        }
    }
    let privileges = child(prop, Tag::Privileges)?;
    if !privileges.text.trim().is_empty() || privileges.children.is_empty() {
        return Err(Rejection::Preflight);
    }
    let mut seen = Vec::new();
    let mut facts = MetadataFacts {
        read: false,
        write_content: false,
        bind: false,
        unbind: false,
    };
    for privilege in &privileges.children {
        if privilege.tag != Tag::Privilege
            || !privilege.text.trim().is_empty()
            || privilege.children.len() != 1
        {
            return Err(Rejection::Preflight);
        }
        let node = &privilege.children[0];
        if !leaf(node)?.trim().is_empty() || seen.contains(&node.tag) {
            return Err(Rejection::Preflight);
        }
        seen.push(node.tag);
        match node.tag {
            Tag::Read => facts.read = true,
            Tag::WriteContent | Tag::Write => facts.write_content = true,
            Tag::Bind => facts.bind = true,
            Tag::Unbind => facts.unbind = true,
            Tag::WriteProperties => {}
            _ => return Err(Rejection::Preflight),
        }
    }
    if !facts.read {
        return Err(Rejection::Preflight);
    }
    Ok(facts)
}
