//! Exact committed product collection binding. No discovery or dispatch.
use super::{ProductionMutationSessions, ProductionTitleMutationTransport};
use crate::{
    adapters::persistence::probe_target::CommittedExactProbeCollections,
    application::{
        attachment_runtime::{ClosedCompositionGate, ConflictAttachmentOwner},
        production_creation_owner::CreationSessionEvidence,
    },
};
use std::sync::{Arc, Weak};

// No caller constructor, Clone, Serialize, routing view or dispatch handle.
pub(crate) struct ExactProbeBinding {
    collections: BindingCollections,
    sessions: Weak<ProductionMutationSessions>,
    concrete: Weak<ProductionTitleMutationTransport>,
    session: CreationSessionEvidence,
    runtime: ConflictAttachmentOwner,
}
enum BindingCollections {
    Probe(CommittedExactProbeCollections),
    Selected(crate::adapters::persistence::scoped_event::ScopedEventSnapshot),
}
impl BindingCollections {
    fn account_and_routes(&self) -> (&str, Vec<&str>) {
        match self {
            Self::Probe(pair) => {
                let (account, routes) = pair.account_and_routes();
                (account, routes.to_vec())
            }
            Self::Selected(scope) => {
                let (account, route) = scope.account_and_route();
                (account, vec![route])
            }
        }
    }
    fn matches_actor(&self, actor: &Weak<()>) -> bool {
        match self {
            Self::Probe(pair) => pair.matches_actor(actor),
            Self::Selected(scope) => scope.matches_actor(actor),
        }
    }
}
impl std::fmt::Debug for ExactProbeBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExactProbeBinding(<redacted>)")
    }
}
impl ExactProbeBinding {
    pub(crate) fn matches_event(
        &self,
        scope: &crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> bool {
        self.sessions.upgrade().is_some_and(|sessions| {
            let (account, routes) = self.collections.account_and_routes();
            scope.account_and_route().0 == account
                && routes.contains(&scope.account_and_route().1)
                && scope.matches_actor(&sessions.database.actor_provenance())
                && match &self.collections {
                    BindingCollections::Selected(selected) => selected.ids() == scope.ids(),
                    BindingCollections::Probe(_) => true,
                }
        })
    }
    // Post-commit ownership check deliberately does not consume the old actor epoch.
    pub(crate) fn event_owner_current(&self) -> bool {
        self.runtime.exact_event_owner_current(self)
    }
    pub(crate) fn event_actor_owner_alive(&self) -> bool {
        self.sessions.upgrade().is_some_and(|sessions| {
            let (registry, actor) = sessions.accounts.creation_owner_provenance();
            let (account, _) = self.collections.account_and_routes();
            self.session.matches(&sessions.accounts, account)
                && self.runtime.probe_seed_owner_alive(&registry, &actor)
                && sessions.session(account).is_ok_and(|current| {
                    self.concrete
                        .upgrade()
                        .is_some_and(|captured| Arc::ptr_eq(&current, &captured))
                })
        })
    }
    pub(crate) fn event_owner_under_gate(&self, gate: &ClosedCompositionGate<'_>) -> bool {
        self.sessions
            .upgrade()
            .is_some_and(|sessions| self.owners_current(gate, &sessions))
    }
    pub(crate) fn revalidate(self) -> Result<Self, ()> {
        let runtime = self.runtime.clone();
        runtime.revalidate_exact_probe_binding(self)
    }
    fn owners_current(
        &self,
        gate: &ClosedCompositionGate<'_>,
        sessions: &Arc<ProductionMutationSessions>,
    ) -> bool {
        let (registry, actor) = sessions.accounts.creation_owner_provenance();
        let (account, routes) = self.collections.account_and_routes();
        gate.attachments()
            .owns_activation_sessions(&sessions.accounts, sessions)
            && sessions.database.matches_clock(&sessions.clock)
            && self.collections.matches_actor(&actor)
            && self.runtime.matches_gate(gate)
            && self.runtime.current(&registry, &actor)
            && sessions
                .creation_session_evidence(account)
                .is_ok_and(|fresh| fresh.same_owner(&self.session))
            && sessions.session(account).is_ok_and(|current| {
                self.concrete
                    .upgrade()
                    .is_some_and(|captured| Arc::ptr_eq(&captured, &current))
                    && routes.into_iter().all(|route| {
                        current
                            .policy
                            .probe_collection_matches(&current.entry_url, route)
                    })
            })
    }
    pub(crate) fn revalidate_under_gate(
        self,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<Self, ()> {
        self.revalidate_under_gate_inner(gate, || {})
    }
    fn revalidate_under_gate_inner(
        mut self,
        gate: &ClosedCompositionGate<'_>,
        after_actor: impl FnOnce(),
    ) -> Result<Self, ()> {
        let sessions = self.sessions.upgrade().ok_or(())?;
        if !self.owners_current(gate, &sessions) {
            return Err(());
        }
        self.collections = match self.collections {
            BindingCollections::Probe(pair) => BindingCollections::Probe(
                sessions
                    .database
                    .revalidate_exact_probe_collections(pair)
                    .map_err(|_| ())?,
            ),
            BindingCollections::Selected(scope) => {
                sessions
                    .database
                    .scoped_event_status(&scope)
                    .map_err(|_| ())?;
                BindingCollections::Selected(scope)
            }
        };
        after_actor();
        if !self.owners_current(gate, &sessions) {
            return Err(());
        }
        Ok(self)
    }
}
impl ProductionMutationSessions {
    pub(crate) fn selected_owner_account(
        self: &Arc<Self>,
        gate: &ClosedCompositionGate<'_>,
        scope: &crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> Result<String, ()> {
        if !gate
            .attachments()
            .owns_activation_sessions(&self.accounts, self)
            || !self.database.matches_clock(&self.clock)
            || !scope.matches_actor(&self.database.actor_provenance())
        {
            return Err(());
        }
        self.database.scoped_event_status(scope).map_err(|_| ())?;
        let binding = self
            .database
            .icloud_binding(scope.ids().1.to_owned())
            .map_err(|_| ())?
            .ok_or(())?;
        let policy = crate::adapters::caldav_transport::DavTransportPolicy::icloud_bound(
            &binding,
            [scope.account_and_route().1.to_owned()],
        )
        .map_err(|_| ())?;
        if !policy.probe_collection_matches(
            &url::Url::parse(&binding.home_url).map_err(|_| ())?,
            scope.account_and_route().1,
        ) {
            return Err(());
        }
        Ok(scope.ids().1.to_owned())
    }
    pub(crate) fn bind_selected_collection(
        self: &Arc<Self>,
        gate: &ClosedCompositionGate<'_>,
        scope: crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> Result<ExactProbeBinding, ()> {
        let account = self.selected_owner_account(gate, &scope)?;
        let (registry, actor) = self.accounts.creation_owner_provenance();
        let (_, session, _) = gate
            .attachments()
            .creation_owner_evidence_under_gate(&account, gate)?;
        let runtime = gate
            .attachments()
            .conflict_owner_under_gate(gate, &account, &registry, &actor)?;
        let concrete = self.session(&account).map_err(|_| ())?;
        ExactProbeBinding {
            collections: BindingCollections::Selected(scope),
            sessions: Arc::downgrade(self),
            concrete: Arc::downgrade(&concrete),
            session,
            runtime,
        }
        .revalidate_under_gate(gate)
    }
    pub(crate) fn exact_probe_owner_account(
        self: &Arc<Self>,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<String, ()> {
        if !gate
            .attachments()
            .owns_activation_sessions(&self.accounts, self)
            || !self.database.matches_clock(&self.clock)
            || self
                .creation_closed
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(());
        }
        let collections = self.database.exact_probe_collections().map_err(|_| ())?;
        let (account, routes) = collections.account_and_routes();
        let binding = self
            .database
            .icloud_binding(account.to_owned())
            .map_err(|_| ())?
            .ok_or(())?;
        let policy = crate::adapters::caldav_transport::DavTransportPolicy::icloud_bound(
            &binding,
            routes.iter().map(|route| (*route).to_owned()),
        )
        .map_err(|_| ())?;
        let origin = url::Url::parse(&binding.home_url).map_err(|_| ())?;
        if !collections.matches_actor(&self.database.actor_provenance())
            || !routes
                .into_iter()
                .all(|route| policy.probe_collection_matches(&origin, route))
        {
            return Err(());
        }
        Ok(account.to_owned())
    }
    pub(crate) fn bind_exact_probe_collections(
        self: &Arc<Self>,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<ExactProbeBinding, ()> {
        let runtime = gate.attachments();
        if !runtime.owns_activation_sessions(&self.accounts, self)
            || !self.database.matches_clock(&self.clock)
        {
            return Err(());
        }
        let collections = self.database.exact_probe_collections().map_err(|_| ())?;
        let (account, routes) = collections.account_and_routes();
        let (registry, actor) = self.accounts.creation_owner_provenance();
        if !collections.matches_actor(&actor) {
            return Err(());
        }
        let (_, session, _) = runtime.creation_owner_evidence_under_gate(account, gate)?;
        let captured_runtime =
            runtime.conflict_owner_under_gate(gate, account, &registry, &actor)?;
        let concrete = self.session(account).map_err(|_| ())?;
        if !routes.into_iter().all(|route| {
            concrete
                .policy
                .probe_collection_matches(&concrete.entry_url, route)
        }) {
            return Err(());
        }
        ExactProbeBinding {
            collections: BindingCollections::Probe(collections),
            sessions: Arc::downgrade(self),
            concrete: Arc::downgrade(&concrete),
            session,
            runtime: captured_runtime,
        }
        .revalidate_under_gate(gate)
    }
}
