//! Private, explicit one-use read acquisition; never a CRUD/bootstrap capability.
use super::{ProbeSourceOwner, ProductionMutationSessions};
use crate::adapters::{
    caldav_transport::{DavAuthentication, DavDepth, DavMethod, DavRequest, RawDavResponse},
    persistence::probe_target::ProbeTargetSelection,
};
use crate::application::{
    attachment_runtime::{ClosedCompositionGate, ConflictAttachmentOwner},
    production_creation_owner::CreationSessionEvidence,
};
use std::sync::{Arc, Weak, atomic::Ordering};
use url::Url;
mod xml;

pub(crate) const PRINCIPAL_BODY: &[u8] =
    br#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;
pub(crate) const HOME_BODY: &[u8] = br#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-home-set/></d:prop></d:propfind>"#;

pub(crate) struct AcquisitionScope {
    sessions: Weak<ProductionMutationSessions>,
    concrete: Arc<super::super::ProductionTitleMutationTransport>,
    runtime: ConflictAttachmentOwner,
    session: CreationSessionEvidence,
    owner: Weak<()>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    account: String,
}
impl AcquisitionScope {
    pub(crate) fn under_gate(&self, gate: &ClosedCompositionGate<'_>) -> bool {
        let Some(sessions) = self.sessions.upgrade() else {
            return false;
        };
        let (registry, actor) = sessions.accounts.creation_owner_provenance();
        self.owner.upgrade().is_some()
            && !self.cancelled.load(Ordering::SeqCst)
            && !crate::adapters::caldav_transport::DavExecutor::cancellation(
                self.concrete.executor.as_ref(),
            )
            .is_some_and(|token| token.is_cancelled())
            && gate
                .attachments()
                .owns_activation_sessions(&sessions.accounts, &sessions)
            && self.runtime.matches_gate(gate)
            && self.runtime.current(&registry, &actor)
            && sessions
                .creation_session_evidence(&self.account)
                .is_ok_and(|s| s.same_owner(&self.session))
            && sessions
                .session(&self.account)
                .is_ok_and(|s| Arc::ptr_eq(&s, &self.concrete))
            && sessions
                .accounts
                .profile(&self.account)
                .is_ok_and(|p| p.remote_editing_enabled)
    }
    pub(crate) fn current(&self) -> bool {
        self.runtime.probe_acquisition_current(self)
    }
    // The actor independently checks durable account eligibility; never issue a
    // synchronous profile/session-evidence actor read while already on its thread.
    pub(crate) fn actor_owner_current(&self) -> bool {
        let Some(sessions) = self.sessions.upgrade() else {
            return false;
        };
        let (registry, actor) = sessions.accounts.creation_owner_provenance();
        self.owner.upgrade().is_some()
            && !self.cancelled.load(Ordering::SeqCst)
            && self.runtime.probe_seed_owner_alive(&registry, &actor)
            && !crate::adapters::caldav_transport::DavExecutor::cancellation(
                self.concrete.executor.as_ref(),
            )
            .is_some_and(|token| token.is_cancelled())
            && !sessions.creation_closed.load(Ordering::SeqCst)
            && sessions.sessions.lock().is_ok_and(|state| {
                !state.stopped
                    && state
                        .entries
                        .get(&self.account)
                        .is_some_and(|current| Arc::ptr_eq(current, &self.concrete))
            })
            && !self.concrete.cancelled.load(Ordering::SeqCst)
    }
}

pub(crate) struct PreparedAcquisition {
    scope: Arc<AcquisitionScope>,
}
// Constructible only by the fixed sequencer below. One executor seal per exchange.
pub(crate) struct AcquisitionPermit {
    scope: Arc<AcquisitionScope>,
    request: DavRequest,
    authentication: Arc<DavAuthentication>,
}
impl AcquisitionPermit {
    pub(crate) fn current(&self) -> bool {
        self.scope.current()
    }
    pub(crate) fn matches_executor(
        &self,
        executor: &crate::adapters::caldav_http::HttpDavExecutor,
    ) -> bool {
        std::ptr::eq(executor, Arc::as_ptr(&self.scope.concrete.executor))
    }
    pub(in crate::adapters) fn parts(&self) -> (&DavRequest, &DavAuthentication) {
        (&self.request, &self.authentication)
    }
    pub(in crate::adapters) fn finish(
        self,
        raw: RawDavResponse,
        _: crate::adapters::caldav_http::CompleteAcquisitionExchange,
    ) -> Result<RawDavResponse, ()> {
        if !self.current() {
            return Err(());
        }
        Ok(raw)
    }
}
pub(crate) struct AcquiredPair {
    scope: Arc<AcquisitionScope>,
    targets: [Url; 2],
    revisions: [super::first_route::CollectionRevisionCapability; 2],
}
impl AcquiredPair {
    pub(crate) fn into_candidates(
        self,
    ) -> Result<[super::first_route::FirstRouteCandidate; 2], ()> {
        if !self.scope.current() {
            return Err(());
        }
        let sessions = self.scope.sessions.upgrade().ok_or(())?;
        let actor = sessions.database.actor_provenance();
        let [first, second] = self.targets;
        Ok([
            super::first_route::FirstRouteCandidate::acquired(
                actor.clone(),
                self.scope.account.clone(),
                ProbeTargetSelection::First,
                first,
                self.revisions[0],
                self.scope.clone(),
            ),
            super::first_route::FirstRouteCandidate::acquired(
                actor,
                self.scope.account.clone(),
                ProbeTargetSelection::Second,
                second,
                self.revisions[1],
                self.scope,
            ),
        ])
    }
}
impl ProductionMutationSessions {
    pub(crate) fn probe_harness_report(
        self: &Arc<Self>,
        prepared: PreparedAcquisition,
    ) -> super::acquisition_report::Report {
        use super::acquisition_report::{Failure, Outcome, Report, Stage};
        let mut report = Report::new(Outcome::Failed, Stage::Credential);
        if !Weak::ptr_eq(&prepared.scope.sessions, &Arc::downgrade(self))
            || !prepared.scope.current()
        {
            report.fail(Failure::OwnerLostOrCancelled);
            return report;
        }
        let authentication = (|| {
            let profile = self
                .accounts
                .profile(&prepared.scope.account)
                .map_err(|_| Failure::OwnerLostOrCancelled)?;
            let reference =
                crate::ports::credentials::CredentialRef::parse(&profile.credential_ref)
                    .map_err(|_| Failure::CredentialInvalid)?;
            if !prepared.scope.current() {
                return Err(Failure::OwnerLostOrCancelled);
            }
            report.credential_check = "attempted_once_at_execute";
            let credential = prepared
                .scope
                .concrete
                .credentials
                .read(&reference)
                .map_err(|e| match e {
                    crate::application::credentials::CredentialServiceError::NotFound => {
                        Failure::CredentialNotFound
                    }
                    crate::application::credentials::CredentialServiceError::InvalidInput => {
                        Failure::CredentialInvalid
                    }
                    crate::application::credentials::CredentialServiceError::Unavailable => {
                        Failure::CredentialUnavailable
                    }
                })?;
            Ok(DavAuthentication::from_credential(&credential))
        })();
        match authentication {
            Err(failure) => report.fail(failure),
            Ok(authentication) => match prepared.run_observed(self, authentication, &mut report) {
                Err(failure) => report.fail(failure),
                Ok(pair) => {
                    if !pair.scope.current() {
                        report.fail(Failure::OwnerLostOrCancelled);
                    } else if report.first_revision
                        == Some(super::acquisition_report::Revision::Missing)
                        || report.second_revision
                            == Some(super::acquisition_report::Revision::Missing)
                    {
                        report.outcome = Outcome::UnsupportedRevision;
                        report.failure = Some(Failure::MissingRevision);
                    } else if report.first_revision
                        == Some(super::acquisition_report::Revision::WeakOrMalformed)
                        || report.second_revision
                            == Some(super::acquisition_report::Revision::WeakOrMalformed)
                    {
                        report.outcome = Outcome::UnsupportedRevision;
                        report.failure = Some(Failure::WeakOrMalformedRevision);
                    } else {
                        report.outcome = Outcome::AcceptedRepresentationObservation;
                    }
                }
            },
        }
        report
    }
    pub(crate) fn prepare_probe_acquisition(
        self: &Arc<Self>,
        gate: &ClosedCompositionGate<'_>,
        account: &str,
    ) -> Result<(ProbeSourceOwner, PreparedAcquisition), ()> {
        if !gate
            .attachments()
            .owns_activation_sessions(&self.accounts, self)
        {
            return Err(());
        }
        let (registry, actor) = self.accounts.creation_owner_provenance();
        let (_, session, _) = gate
            .attachments()
            .creation_owner_evidence_under_gate(account, gate)?;
        let runtime = gate
            .attachments()
            .conflict_owner_under_gate(gate, account, &registry, &actor)?;
        let concrete = self.session(account).map_err(|_| ())?;
        if !self
            .creation_session_evidence(account)
            .is_ok_and(|s| s.same_owner(&session))
        {
            return Err(());
        }
        let owner = ProbeSourceOwner {
            owner: Arc::new(()),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let scope = Arc::new(AcquisitionScope {
            sessions: Arc::downgrade(self),
            concrete,
            runtime,
            session,
            owner: Arc::downgrade(&owner.owner),
            cancelled: owner.cancelled.clone(),
            account: account.into(),
        });
        if !scope.under_gate(gate) {
            return Err(());
        }
        Ok((owner, PreparedAcquisition { scope }))
    }
    pub(crate) fn acquire_probe_pair(
        self: &Arc<Self>,
        prepared: PreparedAcquisition,
    ) -> Result<AcquiredPair, ()> {
        if !Weak::ptr_eq(&prepared.scope.sessions, &Arc::downgrade(self))
            || !prepared.scope.current()
        {
            return Err(());
        }
        let authentication = prepared
            .scope
            .concrete
            .authentication(&prepared.scope.account)
            .map_err(|_| ())?;
        prepared.run(self, authentication)
    }
}
impl PreparedAcquisition {
    fn run(
        self,
        sessions: &Arc<ProductionMutationSessions>,
        authentication: DavAuthentication,
    ) -> Result<AcquiredPair, ()> {
        let mut report = super::acquisition_report::Report::new(
            super::acquisition_report::Outcome::Failed,
            super::acquisition_report::Stage::Credential,
        );
        self.run_observed(sessions, authentication, &mut report)
            .map_err(|_| ())
    }
    fn exchange(
        &self,
        authentication: &Arc<DavAuthentication>,
        target: Url,
        body: &[u8],
        depth: DavDepth,
        stage: super::acquisition_report::Stage,
        report: &mut super::acquisition_report::Report,
    ) -> Result<RawDavResponse, super::acquisition_report::Failure> {
        use super::acquisition_report::{Failure, Report};
        report.stage = stage;
        self.scope
            .concrete
            .policy
            .validate_destination(&target)
            .map_err(Report::transport)?;
        if target.origin() != self.scope.concrete.entry_url.origin()
            || target.query().is_some()
            || target.fragment().is_some()
        {
            return Err(Failure::UnsafeOrAmbiguousMetadata);
        }
        let permit = AcquisitionPermit {
            scope: self.scope.clone(),
            request: DavRequest::new(DavMethod::Propfind, target, body.to_vec(), depth, 30_000)
                .map_err(Report::transport)?,
            authentication: authentication.clone(),
        };
        report.guarded_executor_entries += 1;
        let raw = self
            .scope
            .concrete
            .executor
            .execute_probe_acquisition_guarded(permit)
            .map_err(Report::transport)?;
        if raw.location().is_some() || (300..400).contains(&raw.status()) {
            return Err(Failure::Redirect);
        }
        if raw.status() != 207 {
            return Err(Failure::HttpRejected);
        }
        Ok(raw)
    }
    fn run_observed(
        self,
        sessions: &Arc<ProductionMutationSessions>,
        authentication: DavAuthentication,
        report: &mut super::acquisition_report::Report,
    ) -> Result<AcquiredPair, super::acquisition_report::Failure> {
        use super::acquisition_report::{Failure, Revision, Stage};
        let authentication = Arc::new(authentication);
        if !Weak::ptr_eq(&self.scope.sessions, &Arc::downgrade(sessions)) || !self.scope.current() {
            return Err(Failure::OwnerLostOrCancelled);
        }
        let entry = self.scope.concrete.entry_url.clone();
        let principal = xml::resolve(
            &self.exchange(
                &authentication,
                entry.clone(),
                PRINCIPAL_BODY,
                DavDepth::Zero,
                Stage::Principal,
                report,
            )?,
            &entry,
            xml::Resolution::Principal,
            &self.scope.concrete.policy,
        )
        .map_err(|_| Failure::UnsafeOrAmbiguousMetadata)?;
        let home = xml::resolve(
            &self.exchange(
                &authentication,
                principal.clone(),
                HOME_BODY,
                DavDepth::Zero,
                Stage::Home,
                report,
            )?,
            &principal,
            xml::Resolution::Home,
            &self.scope.concrete.policy,
        )
        .map_err(|_| Failure::UnsafeOrAmbiguousMetadata)?;
        let body = crate::adapters::caldav_transport::probe_harness::METADATA_BODY;
        let targets = xml::locate(
            &self.exchange(
                &authentication,
                home.clone(),
                body,
                DavDepth::One,
                Stage::Locate,
                report,
            )?,
            &home,
            &self.scope.concrete.policy,
        )
        .map_err(|_| Failure::UnsafeOrAmbiguousMetadata)?;
        let mut revisions = [super::first_route::CollectionRevisionCapability::Missing; 2];
        for (index, target) in targets.iter().enumerate() {
            let raw = self.exchange(
                &authentication,
                target.clone(),
                body,
                DavDepth::Zero,
                if index == 0 {
                    Stage::First
                } else {
                    Stage::Second
                },
                report,
            )?;
            crate::adapters::caldav_transport::probe_harness::validate_required_metadata(
                raw.body(),
                raw.content_type()
                    .ok_or(Failure::UnsafeOrAmbiguousMetadata)?,
                target,
                if index == 0 {
                    "Calendar TODO Probe"
                } else {
                    "Calendar TODO Probe 2"
                },
            )
            .map_err(|_| Failure::UnsafeOrAmbiguousMetadata)?;
            revisions[index] = super::first_route::classify_collection_revision(raw.etag());
            report.verified_slots += 1;
            let revision = match revisions[index] {
                super::first_route::CollectionRevisionCapability::Missing => Revision::Missing,
                super::first_route::CollectionRevisionCapability::Invalid => {
                    Revision::WeakOrMalformed
                }
                super::first_route::CollectionRevisionCapability::StrongRepresentationOnly => {
                    Revision::StrongRepresentationOnly
                }
            };
            if index == 0 {
                report.first_revision = Some(revision)
            } else {
                report.second_revision = Some(revision)
            }
        }
        report.stage = Stage::Publication;
        if !self.scope.current() {
            return Err(Failure::OwnerLostOrCancelled);
        }
        Ok(AcquiredPair {
            scope: self.scope,
            targets,
            revisions,
        })
    }
}
