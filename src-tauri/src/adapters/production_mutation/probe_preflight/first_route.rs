//! First-route candidates from private guarded acquisition or offline fixtures.
//! Candidates remain execution-free; collection metadata coverage is unproven.
use crate::adapters::persistence::probe_target::ProbeTargetSelection;
use std::sync::Weak;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CollectionRevisionCapability {
    Missing,
    Invalid,
    // HTTP representation validator only; metadata coverage is unproven.
    StrongRepresentationOnly,
}

pub(crate) fn classify_collection_revision(etag: Option<&str>) -> CollectionRevisionCapability {
    match etag {
        None => CollectionRevisionCapability::Missing,
        Some(value) if crate::domain::remote_mutation::strong_mutation_etag(value) => {
            CollectionRevisionCapability::StrongRepresentationOnly
        }
        Some(_) => CollectionRevisionCapability::Invalid,
    }
}

// Live provenance is private to the fixed reviewed acquisition sequencer.
enum ExactOnlySourceSeal {
    Acquired(std::sync::Arc<super::live_acquisition::AcquisitionScope>),
}

// Consuming, no Clone/Debug/Serialize or public fields/route getters. Scripted
// tests do not implement the absent production source or transport authority.
pub(crate) struct FirstRouteCandidate {
    actor: Weak<()>,
    account: String,
    selection: ProbeTargetSelection,
    target: url::Url,
    revision: CollectionRevisionCapability,
    _source: ExactOnlySourceSeal,
}

pub(crate) struct FirstRoutePreviewParts {
    pub(crate) actor: Weak<()>,
    pub(crate) account: String,
    pub(crate) selection: ProbeTargetSelection,
    pub(crate) target: url::Url,
    pub(crate) revision: CollectionRevisionCapability,
    source: ExactOnlySourceSeal,
}
impl FirstRoutePreviewParts {
    pub(crate) fn actor_owner_current(&self) -> bool {
        match &self.source {
            ExactOnlySourceSeal::Acquired(scope) => scope.actor_owner_current(),
        }
    }
}

impl FirstRouteCandidate {
    pub(super) fn acquired(
        actor: Weak<()>,
        account: String,
        selection: ProbeTargetSelection,
        target: url::Url,
        revision: CollectionRevisionCapability,
        scope: std::sync::Arc<super::live_acquisition::AcquisitionScope>,
    ) -> Self {
        Self {
            actor,
            account,
            selection,
            target,
            revision,
            _source: ExactOnlySourceSeal::Acquired(scope),
        }
    }
    pub(crate) fn into_preview(self) -> Result<FirstRoutePreviewParts, ()> {
        match &self._source {
            ExactOnlySourceSeal::Acquired(scope) if !scope.current() => return Err(()),
            _ => (),
        }
        Ok(FirstRoutePreviewParts {
            actor: self.actor,
            account: self.account,
            selection: self.selection,
            target: self.target,
            revision: self.revision,
            source: self._source,
        })
    }
}
