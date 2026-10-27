//! Same-owner preparation plus private single-use guarded metadata permits.
//! Dedicated Probe source bridge; no ordinary application entry exists.

use super::ProductionMutationSessions;
use crate::{
    adapters::{
        caldav_transport::{DavRequest, RawDavResponse, probe_harness},
        persistence::{
            probe_ledger::{ProbeLedgerScope, ProbeLedgerToken},
            probe_target::CommittedProbeTarget,
        },
    },
    application::{
        attachment_runtime::{ClosedCompositionGate, ConflictAttachmentOwner},
        production_creation_owner::CreationSessionEvidence,
    },
};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

#[allow(dead_code)] // Internal witness boundary; no controller/product installation.
pub(crate) mod seed;

#[allow(dead_code)] // Offline first-route design; no production source mint exists.
pub(crate) mod first_route;

#[allow(dead_code)] // Offline evidence contract; no first-route producer/install.
pub(crate) mod revision_contract;

#[allow(dead_code)] // Offline review records only; external approval seal is empty.
pub(crate) mod acquisition_review;

pub mod acquisition_report;
#[allow(dead_code)] // Explicit private read acquisition; no application installation.
pub(crate) mod live_acquisition;

// A dedicated one-request source owner. Losing this owner permanently closes
// its captured scope, including already returned guarded proof.
pub(crate) struct ProbeSourceOwner {
    owner: Arc<()>,
    cancelled: Arc<AtomicBool>,
}

impl ProbeSourceOwner {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

impl Drop for ProbeSourceOwner {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct ProbeSourceScope {
    owner: Weak<()>,
    cancelled: Arc<AtomicBool>,
    ledger: Option<ProbeLedgerScope>,
    controller: Option<crate::application::attachment_runtime::ProbeControllerScope>,
}

impl ProbeSourceScope {
    fn current(&self) -> bool {
        self.owner.upgrade().is_some()
            && !self.cancelled.load(Ordering::SeqCst)
            && self.controller.as_ref().is_none_or(|scope| scope.current())
    }
}

// Only guarded proof from the reviewed one-exchange executor can construct
// this non-cloneable completion bridge. Untrusted metadata cannot mint it.
pub(crate) struct ProbeSourceCompletionSeal {
    scope: PreparedProbeMetadata,
}

type ProbeSourceCompletionParts = (
    CommittedProbeTarget,
    ConflictAttachmentOwner,
    CreationSessionEvidence,
    Weak<ProductionMutationSessions>,
    Weak<()>,
    Arc<AtomicBool>,
    Option<ProbeLedgerScope>,
    Option<crate::application::attachment_runtime::ProbeControllerScope>,
);

impl ProbeSourceCompletionSeal {
    pub(crate) fn into_parts(self) -> ProbeSourceCompletionParts {
        let scope = self.scope;
        let source = scope.source.expect("sealed dedicated source");
        (
            scope.candidate,
            scope.runtime,
            scope.session,
            scope.sessions,
            source.owner,
            source.cancelled,
            source.ledger,
            source.controller,
        )
    }
}

// Adapter-private consuming socket permit: neither Clone/Debug/Serialize nor
// an arbitrary callback or executor selection supplied by a caller.
pub(crate) struct ProbeMetadataSendPermit {
    request: DavRequest,
    authentication: crate::adapters::caldav_transport::DavAuthentication,
    session: Arc<super::ProductionTitleMutationTransport>,
    scope: Mutex<Option<PreparedProbeMetadata>>,
    seed: bool,
}

pub(crate) struct GuardedProbeMetadata {
    _scope: PreparedProbeMetadata,
    seed_revision: Option<String>,
    seed_concrete: Option<Arc<super::ProductionTitleMutationTransport>>,
}

impl GuardedProbeMetadata {
    pub(crate) fn belongs_to_source(&self, owner: &ProbeSourceOwner) -> bool {
        self._scope.source.as_ref().is_some_and(|source| {
            source.current() && Weak::ptr_eq(&source.owner, &Arc::downgrade(&owner.owner))
        })
    }
    pub(crate) fn completion_seal(
        self,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<ProbeSourceCompletionSeal, ()> {
        if self.seed_revision.is_some() {
            return Err(());
        }
        let scope = self._scope.revalidate(gate)?;
        if !scope.source.as_ref().is_some_and(ProbeSourceScope::current) {
            return Err(());
        }
        #[cfg(not(test))]
        if scope
            .source
            .as_ref()
            .and_then(|s| s.ledger.as_ref())
            .is_none()
        {
            return Err(());
        }
        Ok(ProbeSourceCompletionSeal { scope })
    }
    pub(crate) fn view(&self) -> ProbeMetadataPreparationView {
        ProbeMetadataPreparationView {
            metadata_shape_accepted: true,
            transport_authorized: false,
            completion_available: false,
        }
    }
}

// A consuming, private prepared shape; never a send capability or completion.
pub(crate) struct PreparedProbeMetadata {
    candidate: CommittedProbeTarget,
    runtime: ConflictAttachmentOwner,
    session: CreationSessionEvidence,
    sessions: Weak<ProductionMutationSessions>,
    request: DavRequest,
    source: Option<ProbeSourceScope>,
}

pub(crate) struct ProbeMetadataPreparationView {
    pub(crate) metadata_shape_accepted: bool,
    pub(crate) transport_authorized: bool,
    pub(crate) completion_available: bool,
}

impl ProductionMutationSessions {
    fn prepare_probe_source_core(
        self: &Arc<Self>,
        gate: &ClosedCompositionGate<'_>,
        candidate: CommittedProbeTarget,
    ) -> Result<(ProbeSourceOwner, PreparedProbeMetadata), ()> {
        let mut prepared = self.prepare_probe_metadata(gate, candidate)?;
        let owner = ProbeSourceOwner {
            owner: Arc::new(()),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        prepared.source = Some(ProbeSourceScope {
            owner: Arc::downgrade(&owner.owner),
            cancelled: owner.cancelled.clone(),
            ledger: None,
            controller: None,
        });
        Ok((owner, prepared.revalidate(gate)?))
    }
    pub(crate) fn prepare_probe_source_with_ledger(
        self: &Arc<Self>,
        gate: &ClosedCompositionGate<'_>,
        candidate: CommittedProbeTarget,
        token: &ProbeLedgerToken,
    ) -> Result<(ProbeSourceOwner, PreparedProbeMetadata), ()> {
        let ledger = self.database.probe_source_scope(token).map_err(|_| ())?;
        if !ledger.matches_target(&candidate) {
            return Err(());
        }
        let (owner, mut prepared) = self.prepare_probe_source_core(gate, candidate)?;
        prepared.source.as_mut().ok_or(())?.ledger = Some(ledger);
        Ok((owner, prepared.revalidate(gate)?))
    }
    pub(crate) fn execute_probe_metadata_permit(
        self: &Arc<Self>,
        permit: ProbeMetadataSendPermit,
    ) -> Result<GuardedProbeMetadata, crate::adapters::caldav_transport::DavExecutionError> {
        if !permit.scope.lock().is_ok_and(|scope| {
            scope
                .as_ref()
                .is_some_and(|prepared| Weak::ptr_eq(&prepared.sessions, &Arc::downgrade(self)))
        }) {
            return Err(crate::adapters::caldav_transport::DavExecutionError::Cancelled);
        }
        let executor = permit.session.executor.clone();
        executor.execute_probe_propfind_guarded(permit)
    }
    pub(crate) fn mint_probe_metadata_permit(
        self: &Arc<Self>,
        prepared: PreparedProbeMetadata,
    ) -> Result<ProbeMetadataSendPermit, ()> {
        let prepared = prepared.revalidate_now()?;
        if !Weak::ptr_eq(&prepared.sessions, &Arc::downgrade(self)) {
            return Err(());
        }
        let session = self
            .session(prepared.candidate.checked_parts().0)
            .map_err(|_| ())?;
        let authentication = session
            .authentication(prepared.candidate.checked_parts().0)
            .map_err(|_| ())?;
        self.finish_probe_permit_mint(prepared, session, authentication)
    }

    fn finish_probe_permit_mint(
        self: &Arc<Self>,
        prepared: PreparedProbeMetadata,
        session: Arc<super::ProductionTitleMutationTransport>,
        authentication: crate::adapters::caldav_transport::DavAuthentication,
    ) -> Result<ProbeMetadataSendPermit, ()> {
        let prepared = prepared.revalidate_now()?;
        if !Weak::ptr_eq(&prepared.sessions, &Arc::downgrade(self))
            || !self
                .session(prepared.candidate.checked_parts().0)
                .is_ok_and(|current| Arc::ptr_eq(&session, &current))
            || prepared.request.url()
                != &session
                    .entry_url
                    .join(prepared.candidate.checked_parts().2)
                    .map_err(|_| ())?
            || prepared.request.method() != crate::adapters::caldav_transport::DavMethod::Propfind
            || prepared.request.depth() != crate::adapters::caldav_transport::DavDepth::Zero
            || prepared.request.body() != probe_harness::METADATA_BODY
            || prepared.request.timeout_ms() != 30_000
        {
            return Err(());
        }
        let request = probe_harness::metadata_request(prepared.request.url().clone())?;
        Ok(ProbeMetadataSendPermit {
            request,
            authentication,
            session,
            scope: Mutex::new(Some(prepared)),
            seed: false,
        })
    }

    pub(crate) fn prepare_probe_metadata(
        self: &Arc<Self>,
        gate: &ClosedCompositionGate<'_>,
        candidate: CommittedProbeTarget,
    ) -> Result<PreparedProbeMetadata, ()> {
        let runtime = gate.attachments();
        if !runtime.owns_activation_sessions(&self.accounts, self) {
            return Err(());
        }
        let candidate = self
            .database
            .revalidate_probe_target(candidate)
            .map_err(|_| ())?;
        let (account, _, route, _) = candidate.checked_parts();
        let (registry, actor) = self.accounts.creation_owner_provenance();
        if !candidate.matches_actor(&actor) {
            return Err(());
        }
        let (_, session, _) = runtime.creation_owner_evidence_under_gate(account, gate)?;
        let captured_runtime =
            runtime.conflict_owner_under_gate(gate, account, &registry, &actor)?;
        if !self.probe_completion_session_matches(account, route, &session) {
            return Err(());
        }
        let concrete = self.session(account).map_err(|_| ())?;
        let target = concrete.entry_url.join(route).map_err(|_| ())?;
        let request = probe_harness::metadata_request(target)?;
        PreparedProbeMetadata {
            candidate,
            runtime: captured_runtime,
            session,
            sessions: Arc::downgrade(self),
            request,
            source: None,
        }
        .revalidate(gate)
    }
}

impl ProbeMetadataSendPermit {
    pub(crate) fn matches_executor(
        &self,
        executor: &crate::adapters::caldav_http::HttpDavExecutor,
    ) -> bool {
        std::ptr::eq(executor, Arc::as_ptr(&self.session.executor))
    }

    pub(in crate::adapters) fn request_and_authentication(
        &self,
    ) -> (
        &DavRequest,
        &crate::adapters::caldav_transport::DavAuthentication,
    ) {
        (&self.request, &self.authentication)
    }

    pub(crate) fn current(&self) -> bool {
        let Ok(mut slot) = self.scope.lock() else {
            return false;
        };
        let Some(scope) = slot.take() else {
            return false;
        };
        let Ok(scope) = scope.revalidate_now() else {
            return false;
        };
        if scope.request.url() != self.request.url() {
            return false;
        }
        *slot = Some(scope);
        true
    }

    pub(in crate::adapters) fn finish(
        self,
        raw: RawDavResponse,
        _exchange: crate::adapters::caldav_http::CompleteProbeMetadataExchange,
    ) -> Result<GuardedProbeMetadata, ()> {
        let scope = self
            .scope
            .into_inner()
            .map_err(|_| ())?
            .ok_or(())?
            .revalidate_now()?;
        if raw.status() != 207 || raw.location().is_some() {
            return Err(());
        }
        let seed_revision = if self.seed {
            let revision = raw.etag().ok_or(())?;
            if !crate::domain::remote_mutation::strong_mutation_etag(revision) {
                return Err(());
            }
            Some(revision.to_owned())
        } else {
            if raw.etag().is_some() {
                return Err(());
            }
            None
        };
        probe_harness::validate_required_metadata(
            raw.body(),
            raw.content_type().ok_or(())?,
            scope.request.url(),
            scope.candidate.checked_parts().3,
        )?;
        let scope = scope.revalidate_now()?;
        Ok(GuardedProbeMetadata {
            _scope: scope,
            seed_revision,
            seed_concrete: self.seed.then_some(self.session),
        })
    }
}

impl PreparedProbeMetadata {
    fn owners_current(
        &self,
        gate: &ClosedCompositionGate<'_>,
        sessions: &Arc<ProductionMutationSessions>,
    ) -> bool {
        let (registry, actor) = sessions.accounts.creation_owner_provenance();
        let (account, _, route, _) = self.candidate.checked_parts();
        gate.attachments()
            .owns_activation_sessions(&sessions.accounts, sessions)
            && self.candidate.matches_actor(&actor)
            && self.runtime.matches_gate(gate)
            && self.runtime.current(&registry, &actor)
            && sessions.probe_completion_session_matches(account, route, &self.session)
            && self.source.as_ref().is_none_or(ProbeSourceScope::current)
            && self
                .source
                .as_ref()
                .and_then(|s| s.ledger.as_ref())
                .is_none_or(|scope| {
                    scope.matches_target(&self.candidate)
                        && sessions
                            .database
                            .probe_source_active(scope)
                            .unwrap_or(false)
                })
    }

    pub(crate) fn revalidate(mut self, gate: &ClosedCompositionGate<'_>) -> Result<Self, ()> {
        let sessions = self.sessions.upgrade().ok_or(())?;
        if !self.owners_current(gate, &sessions) {
            return Err(());
        }
        self.candidate = sessions
            .database
            .revalidate_probe_target(self.candidate)
            .map_err(|_| ())?;
        if !self.owners_current(gate, &sessions) {
            return Err(());
        }
        Ok(self)
    }

    fn revalidate_now(self) -> Result<Self, ()> {
        let captured = self.runtime.clone();
        captured.revalidate_probe_metadata(self)
    }

    // Response bytes are deliberately untrusted: accepting their shape never
    // proves an HTTP exchange or mints CompletedProbeSource.
    pub(crate) fn consume_untrusted_response(
        self,
        gate: &ClosedCompositionGate<'_>,
        raw: RawDavResponse,
    ) -> Result<ProbeMetadataPreparationView, ()> {
        let current = self.revalidate(gate)?;
        if raw.status() != 207 || raw.location().is_some() || raw.etag().is_some() {
            return Err(());
        }
        let name = current.candidate.checked_parts().3;
        probe_harness::validate_required_metadata(
            raw.body(),
            raw.content_type().ok_or(())?,
            current.request.url(),
            name,
        )?;
        let _validated = current.revalidate(gate)?;
        Ok(ProbeMetadataPreparationView {
            metadata_shape_accepted: true,
            transport_authorized: false,
            completion_available: false,
        })
    }
}

impl PreparedProbeMetadata {
    pub(crate) fn bind_controller(
        mut self,
        scope: crate::application::attachment_runtime::ProbeControllerScope,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<Self, ()> {
        let source = self.source.as_mut().ok_or(())?;
        if source.ledger.is_none() || source.controller.is_some() || !scope.current() {
            return Err(());
        }
        source.controller = Some(scope);
        self.revalidate(gate)
    }
}
