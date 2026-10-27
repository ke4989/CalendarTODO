use crate::domain::remote_mutation::single_vevent_title;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalTitleUpdateTarget {
    account_id: String,
    calendar_id: String,
    resource_id: String,
    attachment_generation: u64,
}

impl ConditionalTitleUpdateTarget {
    pub fn new(
        account_id: String,
        calendar_id: String,
        resource_id: String,
        attachment_generation: u64,
    ) -> Result<Self, ConditionalTitleUpdateDenial> {
        for value in [&account_id, &calendar_id, &resource_id] {
            Uuid::parse_str(value).map_err(|_| ConditionalTitleUpdateDenial::InvalidIdentity)?;
        }
        if attachment_generation == 0 {
            return Err(ConditionalTitleUpdateDenial::StaleAttachment);
        }
        Ok(Self {
            account_id,
            calendar_id,
            resource_id,
            attachment_generation,
        })
    }

    pub(crate) fn account_id(&self) -> &str {
        &self.account_id
    }

    pub(crate) fn calendar_id(&self) -> &str {
        &self.calendar_id
    }

    pub(crate) fn resource_id(&self) -> &str {
        &self.resource_id
    }

    pub(crate) fn attachment_generation(&self) -> u64 {
        self.attachment_generation
    }
}

/// Opaque, bounded evidence for one exact committed cache resource. The SQLite
/// actor reduces cache facts to this verdict before returning it, so no ETag,
/// href, raw ICS, privilege, or lifecycle detail crosses the query boundary.
pub struct ConditionalTitleUpdateEvidence {
    target: ConditionalTitleUpdateTarget,
    verdict: Result<(), ConditionalTitleUpdateDenial>,
}

pub(crate) struct CommittedConditionalTitleUpdateFacts<'a> {
    pub account_usable: bool,
    pub collection_available: bool,
    pub collection_writable: bool,
    pub resource_active: bool,
    pub parse_safe: bool,
    pub etag_usable: bool,
    pub raw_ics: &'a [u8],
}

impl ConditionalTitleUpdateEvidence {
    pub fn unavailable(target: ConditionalTitleUpdateTarget) -> Self {
        Self {
            target,
            verdict: Err(ConditionalTitleUpdateDenial::CapabilityUnavailable),
        }
    }

    /// Derive a bounded verdict while the committed cache values are still
    /// owned by the SQLite actor. The returned evidence retains none of them.
    pub(crate) fn from_committed_cache(
        target: ConditionalTitleUpdateTarget,
        facts: CommittedConditionalTitleUpdateFacts<'_>,
    ) -> Self {
        let verdict = if !facts.account_usable || !facts.collection_available {
            Err(ConditionalTitleUpdateDenial::AccountUnavailable)
        } else if !facts.collection_writable {
            Err(ConditionalTitleUpdateDenial::CollectionReadOnly)
        } else if !facts.resource_active {
            Err(ConditionalTitleUpdateDenial::ResourceInactive)
        } else if !facts.parse_safe {
            Err(ConditionalTitleUpdateDenial::UnsafeCalendarData)
        } else if !facts.etag_usable {
            Err(ConditionalTitleUpdateDenial::UnusableEtag)
        } else if single_vevent_title(facts.raw_ics).is_err() {
            Err(ConditionalTitleUpdateDenial::UnsafeCalendarData)
        } else if contains_property(facts.raw_ics, &["ORGANIZER", "ATTENDEE"]) {
            Err(ConditionalTitleUpdateDenial::Invitation)
        } else if contains_property(
            facts.raw_ics,
            &["RRULE", "RDATE", "EXDATE", "RECURRENCE-ID"],
        ) {
            Err(ConditionalTitleUpdateDenial::RecurrenceSensitive)
        } else {
            Ok(())
        };
        Self { target, verdict }
    }

    pub fn verify(
        &self,
        requested: &ConditionalTitleUpdateTarget,
    ) -> Result<(), ConditionalTitleUpdateDenial> {
        if self.target.account_id != requested.account_id
            || self.target.calendar_id != requested.calendar_id
            || self.target.resource_id != requested.resource_id
        {
            return Err(ConditionalTitleUpdateDenial::IdentityMismatch);
        }
        if self.target.attachment_generation != requested.attachment_generation {
            return Err(ConditionalTitleUpdateDenial::StaleAttachment);
        }
        self.verdict
    }
}

fn contains_property(raw: &[u8], names: &[&str]) -> bool {
    let Ok(text) = std::str::from_utf8(raw) else {
        return true;
    };
    text.replace("\r\n ", "")
        .replace("\r\n\t", "")
        .lines()
        .map(str::trim)
        .any(|line| {
            line.split_once([':', ';']).is_some_and(|(name, _)| {
                names
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate))
            })
        })
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalTitleUpdateDenial {
    #[error("conditional title-update identity is invalid")]
    InvalidIdentity,
    #[error("conditional title-update identity does not match evidence")]
    IdentityMismatch,
    #[error("conditional title-update attachment ownership is stale")]
    StaleAttachment,
    #[error("conditional title-update capability is unavailable")]
    CapabilityUnavailable,
    #[error("calendar account or collection is unavailable")]
    AccountUnavailable,
    #[error("calendar collection is read-only")]
    CollectionReadOnly,
    #[error("calendar resource is not active")]
    ResourceInactive,
    #[error("calendar resource has no usable strong committed ETag")]
    UnusableEtag,
    #[error("calendar data is unsafe for lossless title update")]
    UnsafeCalendarData,
    #[error("calendar invitation data is read-only")]
    Invitation,
    #[error("recurrence-sensitive title update is unavailable")]
    RecurrenceSensitive,
}
