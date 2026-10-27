//! Inert Probe target/preflight constraints. This module deliberately has no
//! production owner constructor, executor, credentials or activation binding.
//! A later concrete Probe-only composition must supply those independently.

use super::{DavDepth, DavMethod, DavRequest, DavTransportPolicy};
use serde::Deserialize;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use url::Url;

mod metadata;

const MAX_INPUT: usize = 512;
const MAX_ROUTE: usize = 2048;
pub(crate) const METADATA_BODY: &[u8] = br#"<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/><d:resourcetype/><d:current-user-privilege-set/></d:prop></d:propfind>"#;

// Pure internal shape/parser helpers, not owner or executor authorization.
pub(crate) fn metadata_request(target: Url) -> Result<DavRequest, ()> {
    DavRequest::new(
        DavMethod::Propfind,
        target,
        METADATA_BODY.to_vec(),
        DavDepth::Zero,
        30_000,
    )
    .map_err(|_| ())
}

pub(crate) fn validate_required_metadata(
    body: &[u8],
    content_type: &str,
    target: &Url,
    name: &str,
) -> Result<(), ()> {
    if !matches!(name, "Calendar TODO Probe" | "Calendar TODO Probe 2") {
        return Err(());
    }
    let facts = metadata::parse(body, content_type, target, name).map_err(|_| ())?;
    if !(facts.read && facts.write_content && facts.bind && facts.unbind) {
        return Err(());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SemanticInput {
    calendar: ProbeCalendar,
    plan: ProbePlan,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(try_from = "String")]
enum ProbeCalendar {
    First,
    Second,
}

impl TryFrom<String> for ProbeCalendar {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "Calendar TODO Probe" => Ok(Self::First),
            "Calendar TODO Probe 2" => Ok(Self::Second),
            _ => Err("invalid Probe calendar"),
        }
    }
}

#[derive(Deserialize)]
#[serde(try_from = "String")]
enum ProbePlan {
    ReadOnly,
    RoundTrip,
}

impl TryFrom<String> for ProbePlan {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "readOnlyPreflight" => Ok(Self::ReadOnly),
            "utcCrudRoundTrip" => Ok(Self::RoundTrip),
            _ => Err("invalid Probe plan"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Rejection {
    Input,
    Target,
    Owner,
    Preflight,
}

fn parse_input(raw: &[u8]) -> Result<SemanticInput, Rejection> {
    if raw.len() > MAX_INPUT {
        return Err(Rejection::Input);
    }
    serde_json::from_slice(raw).map_err(|_| Rejection::Input)
}

// This evidence cannot be constructed by another module. Test fixtures below
// model owner replacement/cancellation; they are not production provenance.
struct OwnerEvidence {
    session: Weak<AtomicBool>,
    owner: Weak<()>,
}

impl OwnerEvidence {
    fn current(&self, session: &Arc<AtomicBool>, owner: &Arc<()>) -> bool {
        Weak::ptr_eq(&self.session, &Arc::downgrade(session))
            && Weak::ptr_eq(&self.owner, &Arc::downgrade(owner))
            && self.owner.upgrade().is_some()
            && self
                .session
                .upgrade()
                .is_some_and(|s| !s.load(Ordering::SeqCst))
    }
}

// Non-cloneable, non-serializable, no Debug and no public target getter.
struct OfflineBinding {
    calendar: ProbeCalendar,
    target: Url,
    evidence: OwnerEvidence,
}

impl OfflineBinding {
    fn validate(
        calendar: ProbeCalendar,
        origin: &str,
        route: &str,
        policy: &DavTransportPolicy,
        evidence: OwnerEvidence,
        session: &Arc<AtomicBool>,
        owner: &Arc<()>,
    ) -> Result<Self, Rejection> {
        if !evidence.current(session, owner) {
            return Err(Rejection::Owner);
        }
        // Reject normalization, relative/authority URLs, dot segments and
        // encoded paths rather than silently repairing historical route data.
        if origin.len() > MAX_ROUTE
            || route.len() > MAX_ROUTE
            || !route.starts_with('/')
            || route.starts_with("//")
            || !route.ends_with('/')
            || route == "/"
            || route.contains(['%', '\\', '?', '#'])
            || route.chars().any(|c| c.is_control() || c.is_whitespace())
            || route.split('/').any(|s| s == "." || s == "..")
        {
            return Err(Rejection::Target);
        }
        let base = Url::parse(origin).map_err(|_| Rejection::Target)?;
        policy
            .validate_destination(&base)
            .map_err(|_| Rejection::Target)?;
        if base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
            || base.as_str() != origin
        {
            return Err(Rejection::Target);
        }
        let target = base.join(route).map_err(|_| Rejection::Target)?;
        policy
            .validate_destination(&target)
            .map_err(|_| Rejection::Target)?;
        if target.origin() != base.origin() || target.path() != route {
            return Err(Rejection::Target);
        }
        Ok(Self {
            calendar,
            target,
            evidence,
        })
    }

    fn prepare(
        &self,
        input: &SemanticInput,
        session: &Arc<AtomicBool>,
        owner: &Arc<()>,
    ) -> Result<OfflinePreflight<'_>, Rejection> {
        if !self.evidence.current(session, owner) {
            return Err(Rejection::Owner);
        }
        if input.calendar != self.calendar {
            return Err(Rejection::Target);
        }
        // No DavRequest escapes this private module, and no execute is called.
        let request = metadata_request(self.target.clone()).map_err(|_| Rejection::Preflight)?;
        Ok(OfflinePreflight {
            request,
            binding: self,
        })
    }
}

struct OfflinePreflight<'a> {
    request: DavRequest,
    binding: &'a OfflineBinding,
}

struct OfflineMetadata<'a> {
    binding: &'a OfflineBinding,
    facts: metadata::MetadataFacts,
}

impl OfflineMetadata<'_> {
    fn current(&self, session: &Arc<AtomicBool>, owner: &Arc<()>) -> bool {
        self.binding.evidence.current(session, owner)
    }

    fn network_authorized(&self) -> bool {
        false
    }
}

struct MetadataEnvelope<'a> {
    final_url: &'a Url,
    exchanges: usize,
    status: u16,
    location: Option<&'a str>,
    content_type: &'a str,
    body: &'a [u8],
}

impl<'a> OfflinePreflight<'a> {
    fn network_authorized(&self) -> bool {
        false
    }

    // Pure exchange-envelope shape check, not parsed metadata or write proof.
    fn validate_exchange_shape(
        &self,
        final_url: &Url,
        exchanges: usize,
        status: u16,
        has_location: bool,
    ) -> Result<(), Rejection> {
        if final_url != self.request.url() || exchanges != 1 || status != 207 || has_location {
            return Err(Rejection::Preflight);
        }
        Ok(())
    }

    fn parse_metadata(
        &self,
        envelope: MetadataEnvelope<'_>,
        session: &Arc<AtomicBool>,
        owner: &Arc<()>,
    ) -> Result<OfflineMetadata<'a>, Rejection> {
        if !self.binding.evidence.current(session, owner) {
            return Err(Rejection::Owner);
        }
        self.validate_exchange_shape(
            envelope.final_url,
            envelope.exchanges,
            envelope.status,
            envelope.location.is_some(),
        )?;
        let name = match self.binding.calendar {
            ProbeCalendar::First => "Calendar TODO Probe",
            ProbeCalendar::Second => "Calendar TODO Probe 2",
        };
        let facts = metadata::parse(
            envelope.body,
            envelope.content_type,
            self.request.url(),
            name,
        )?;
        if !self.binding.evidence.current(session, owner) {
            return Err(Rejection::Owner);
        }
        Ok(OfflineMetadata {
            binding: self.binding,
            facts,
        })
    }
}
