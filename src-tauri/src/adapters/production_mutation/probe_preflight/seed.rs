//! Exact committed-route witness only. Never first-route discovery or completion.
use super::*;

// Distinct consuming permit and envelope; caller bytes cannot construct either.
pub(crate) struct ProbeSeedSendPermit(ProbeMetadataSendPermit);
pub(crate) struct ProbeSeedEnvelope(GuardedProbeMetadata);

pub(crate) struct ProbeSeedOwner {
    target: url::Url,
    concrete: Arc<super::super::ProductionTitleMutationTransport>,
    runtime: ConflictAttachmentOwner,
    sessions: Weak<ProductionMutationSessions>,
    source: ProbeSourceScope,
}

impl ProbeSeedOwner {
    pub(crate) fn target(&self) -> &str {
        self.target.as_str()
    }
    pub(crate) fn same_target_route(&self, route: &str) -> bool {
        self.target.join(route).is_ok_and(|url| url == self.target)
    }
    // Actor-safe owner checks: never request another actor read from its thread.
    pub(crate) fn current(&self, candidate: &CommittedProbeTarget) -> bool {
        let Some(sessions) = self.sessions.upgrade() else {
            return false;
        };
        let (registry, actor) = sessions.accounts.creation_owner_provenance();
        let (account, _, route, _) = candidate.checked_parts();
        candidate.matches_actor(&actor)
            && self.source.current()
            && self.runtime.probe_seed_owner_alive(&registry, &actor)
            && !sessions.creation_closed.load(Ordering::SeqCst)
            && sessions.sessions.lock().is_ok_and(|state| {
                !state.stopped
                    && state
                        .entries
                        .get(account)
                        .is_some_and(|current| Arc::ptr_eq(current, &self.concrete))
            })
            && !self.concrete.cancelled.load(Ordering::SeqCst)
            && self
                .concrete
                .policy
                .probe_collection_matches(&self.concrete.entry_url, route)
    }
    pub(crate) fn ledger(&self) -> Option<&ProbeLedgerScope> {
        self.source.ledger.as_ref()
    }
}

impl ProbeSeedEnvelope {
    pub(crate) fn into_parts(self) -> Result<(CommittedProbeTarget, String, ProbeSeedOwner), ()> {
        let proof = self.0;
        let revision = proof.seed_revision.ok_or(())?;
        let concrete = proof.seed_concrete.ok_or(())?;
        let scope = proof._scope.revalidate_now()?;
        let source = scope.source.ok_or(())?;
        if source.ledger.is_none() || !source.current() {
            return Err(());
        }
        Ok((
            scope.candidate,
            revision,
            ProbeSeedOwner {
                target: scope.request.url().clone(),
                concrete,
                runtime: scope.runtime,
                sessions: scope.sessions,
                source,
            },
        ))
    }
}

impl ProductionMutationSessions {
    pub(crate) fn mint_probe_seed_permit(
        self: &Arc<Self>,
        prepared: PreparedProbeMetadata,
    ) -> Result<ProbeSeedSendPermit, ()> {
        if !prepared
            .source
            .as_ref()
            .is_some_and(|s| s.ledger.is_some() && s.current())
        {
            return Err(());
        }
        let mut permit = self.mint_probe_metadata_permit(prepared)?;
        permit.seed = true;
        Ok(ProbeSeedSendPermit(permit))
    }
    pub(crate) fn execute_probe_seed_permit(
        self: &Arc<Self>,
        permit: ProbeSeedSendPermit,
    ) -> Result<ProbeSeedEnvelope, crate::adapters::caldav_transport::DavExecutionError> {
        let proof = self.execute_probe_metadata_permit(permit.0)?;
        if proof.seed_revision.is_none() {
            return Err(crate::adapters::caldav_transport::DavExecutionError::InvalidRequest);
        }
        Ok(ProbeSeedEnvelope(proof))
    }
}
