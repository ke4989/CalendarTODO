//! Explicit process-local activation. Durable admission remains execution-free;
//! only this owner can issue a graph-specific, non-cloneable submission permit.
use super::{
    account_registry::AccountSourceRegistry, attachment_runtime::AttachmentRuntime,
    production_crud_composition::ExecutionClosedProductionMutationComposition,
    reminders::ReminderRuntime, remote_creation_facade::RemoteCreationApplication,
    remote_mutation_facade::RemoteMutationApplication,
    remote_mutation_runtime::MutationRuntimeView,
};
use crate::adapters::production_mutation::ProductionMutationSessions;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ActivationInput {
    pub account_id: String,
    pub choice: ActivationChoice,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ActivationAccountInput {
    pub account_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivationShutdownInput {}

pub(crate) fn parse_input<T: serde::de::DeserializeOwned>(input: &str) -> Result<T, ()> {
    if input.len() > 1024 || !input.trim_start().starts_with('{') {
        return Err(());
    }
    serde_json::from_str(input).map_err(|_| ())
}

/// No default and no implicit boolean conversion: enabling is an explicit act.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ActivationChoice {
    EnableReviewedExecution,
    DisableExecution,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivationView {
    pub account_id: String,
    pub activated: bool,
    pub execution_available: bool,
}

pub(crate) struct ActivationOwners {
    pub accounts: Arc<AccountSourceRegistry>,
    pub attachments: AttachmentRuntime,
    pub sessions: Arc<ProductionMutationSessions>,
    pub creation: RemoteCreationApplication,
    pub conflicts: RemoteMutationApplication,
    pub reminders: Arc<ReminderRuntime>,
    pub clock: Arc<dyn crate::ports::clock::Clock>,
}

struct EnabledGraph {
    graph: Arc<ExecutionClosedProductionMutationComposition>,
    enabled: Arc<AtomicBool>,
    scoped_only: bool,
}

pub(crate) struct RemoteExecutionActivation {
    owners: ActivationOwners,
    // Never acquired by attachment, transport, publication or scheduler callbacks.
    // It serializes enable/disable without holding the graph-map gate during drain.
    transition: Mutex<()>,
    graphs: Mutex<HashMap<String, EnabledGraph>>,
    closing: AtomicBool,
    scoped_emitter: Mutex<Option<Arc<dyn super::remote_mutation_runtime::MutationStateEmitter>>>,
}

pub(crate) struct ActivationPermit {
    graph: Weak<ExecutionClosedProductionMutationComposition>,
    enabled: Weak<AtomicBool>,
    event: Option<(
        crate::adapters::production_mutation::exact_probe::ExactProbeBinding,
        crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    )>,
}

impl ActivationPermit {
    pub(crate) fn take_event(
        &mut self,
    ) -> Option<(
        crate::adapters::production_mutation::exact_probe::ExactProbeBinding,
        crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    )> {
        self.event.take()
    }
    pub(crate) fn authorizes(&self, graph: &ExecutionClosedProductionMutationComposition) -> bool {
        self.enabled
            .upgrade()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
            && self
                .graph
                .upgrade()
                .is_some_and(|owned| std::ptr::eq(owned.as_ref(), graph))
    }
}

impl RemoteExecutionActivation {
    pub(crate) fn matches_confirmed_sync(
        &self,
        sync: &crate::application::sync_runtime::SyncExecutionRuntime,
    ) -> bool {
        sync.belongs_to_registry(&self.owners.accounts, &self.owners.clock)
    }
    pub(crate) fn ensure_confirmed_connection(
        &self,
        account: &str,
        route: &str,
    ) -> Result<
        crate::application::attachment_runtime::ConfirmedConnection,
        crate::application::account_profiles::AccountProfileServiceError,
    > {
        if self.closing.load(Ordering::SeqCst) || !self.captured_owners_current() {
            return Err(
                crate::application::account_profiles::AccountProfileServiceError::Unavailable,
            );
        }
        self.owners
            .attachments
            .ensure_confirmed_connection(&self.owners.sessions, account, route)
    }
    pub(crate) fn remote_session_ready(&self, account: &str) -> bool {
        !self.closing.load(Ordering::SeqCst)
            && self.captured_owners_current()
            && self
                .owners
                .attachments
                .connected_owner_current(&self.owners.sessions, account, None)
    }
    pub(crate) fn admit_connected_event<T>(
        &self,
        database: &crate::adapters::persistence::Database,
        admit: impl FnOnce(
            crate::adapters::persistence::scoped_event::ConnectedOwnerCheck,
        ) -> Result<T, crate::adapters::persistence::PersistenceError>,
    ) -> Result<T, crate::adapters::persistence::PersistenceError> {
        if self.closing.load(Ordering::SeqCst)
            || !self.captured_owners_current()
            || !Weak::ptr_eq(
                &database.actor_provenance(),
                &self.owners.accounts.creation_owner_provenance().1,
            )
            || !database.matches_clock(&self.owners.clock)
        {
            return Err(
                crate::adapters::persistence::PersistenceError::RemoteAccountConnectionRequired,
            );
        }
        self.owners
            .attachments
            .admit_connected_event(&self.owners.sessions, admit)
    }
    pub(crate) fn prepare_confirmed_event_binding(
        &self,
        scope: crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> Result<crate::adapters::production_mutation::exact_probe::ExactProbeBinding, ()> {
        if self.closing.load(Ordering::SeqCst) || !self.captured_owners_current() {
            return Err(());
        }
        self.owners
            .attachments
            .prepare_selected_calendar_owner(&self.owners.sessions, scope)
    }
    pub(crate) fn belongs_to_application(
        &self,
        database: &crate::adapters::persistence::Database,
        attachments: &AttachmentRuntime,
        profiles: &super::account_profiles::AccountProfileService,
        conflicts: &RemoteMutationApplication,
        reminders: &Arc<ReminderRuntime>,
        clock: &Arc<dyn crate::ports::clock::Clock>,
    ) -> bool {
        Weak::ptr_eq(
            &database.actor_provenance(),
            &self.owners.accounts.creation_owner_provenance().1,
        ) && self.owners.attachments.same_owner(attachments)
            && profiles.belongs_to_registry(&self.owners.accounts)
            && self.owners.conflicts.same_admission_owner(conflicts)
            && Arc::ptr_eq(&self.owners.reminders, reminders)
            && Arc::ptr_eq(&self.owners.clock, clock)
            && reminders.matches_application_owner(database, clock)
            && self.captured_owners_current()
            && !self.closing.load(Ordering::SeqCst)
    }
    fn captured_owners_current(&self) -> bool {
        let actor = self.owners.accounts.creation_owner_provenance().1;
        self.owners
            .attachments
            .owns_activation_sessions(&self.owners.accounts, &self.owners.sessions)
            && self.owners.sessions.matches_captured_owners(
                &self.owners.accounts,
                &actor,
                &self.owners.clock,
            )
            && self
                .owners
                .creation
                .admission_status_evidence()
                .matches_actor(&actor)
            && self
                .owners
                .reminders
                .matches_captured_owner(&actor, &self.owners.clock)
    }
    /// Passive owner capture: no graph, query, credential read, claim or dispatch.
    pub(crate) fn new(owners: ActivationOwners) -> Self {
        Self {
            owners,
            transition: Mutex::new(()),
            graphs: Mutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
            scoped_emitter: Mutex::new(None),
        }
    }

    fn eligible(&self, account: &str) -> bool {
        !self.closing.load(Ordering::SeqCst)
            && self.captured_owners_current()
            && self
                .owners
                .attachments
                .reviewed_mutation_lifecycle()
                .verify()
            && self.owners.accounts.profile(account).is_ok_and(|p| {
                p.enabled
                    && p.credential_configured
                    && p.remote_editing_enabled
                    && p.provider_kind == crate::domain::account::ProviderKind::CalDav
                    && p.region_profile == "icloud-cn"
            })
    }

    pub(crate) fn view(&self, account: &str) -> Result<ActivationView, ()> {
        uuid::Uuid::parse_str(account).map_err(|_| ())?;
        let graphs = self.graphs.lock().map_err(|_| ())?;
        let enabled = graphs.get(account);
        Ok(ActivationView {
            account_id: account.to_owned(),
            activated: enabled.is_some_and(|e| e.enabled.load(Ordering::SeqCst)),
            execution_available: self.eligible(account)
                && enabled.is_some_and(|e| {
                    !e.scoped_only
                        && e.enabled.load(Ordering::SeqCst)
                        && e.graph.view().owner_current
                }),
        })
    }

    pub(crate) fn change(&self, input: ActivationInput) -> Result<ActivationView, ()> {
        uuid::Uuid::parse_str(&input.account_id).map_err(|_| ())?;
        let _transition = self.transition.lock().map_err(|_| ())?;
        match input.choice {
            ActivationChoice::DisableExecution => self.retire(&input.account_id)?,
            ActivationChoice::EnableReviewedExecution => {
                if !self.eligible(&input.account_id) {
                    return Err(());
                }
                if self.view(&input.account_id)?.execution_available {
                    return self.view(&input.account_id);
                }
                self.retire(&input.account_id)?;
                self.owners
                    .attachments
                    .prepare_activation_session(&input.account_id)?;
                let composer = self
                    .owners
                    .reminders
                    .scheduling_composer()
                    .map_err(|_| ())?;
                let graph = Arc::new(
                    self.owners
                        .attachments
                        .compose_execution_closed_with_reminders(
                            &self.owners.sessions,
                            &self.owners.creation,
                            &self.owners.conflicts,
                            &input.account_id,
                            composer,
                        )?,
                );
                if !self.eligible(&input.account_id) || !graph.view().owner_current {
                    self.owners
                        .attachments
                        .retire_execution_closed(&graph)
                        .map_err(|_| ())?;
                    return Err(());
                }
                self.graphs.lock().map_err(|_| ())?.insert(
                    input.account_id.clone(),
                    EnabledGraph {
                        graph,
                        enabled: Arc::new(AtomicBool::new(true)),
                        scoped_only: false,
                    },
                );
            }
        }
        self.view(&input.account_id)
    }

    fn retire(&self, account: &str) -> Result<(), ()> {
        let captured = {
            let graphs = self.graphs.lock().map_err(|_| ())?;
            graphs.get(account).map(|entry| {
                entry.enabled.store(false, Ordering::SeqCst);
                entry.graph.clone()
            })
        };
        if let Some(graph) = captured {
            self.owners
                .attachments
                .retire_execution_closed(&graph)
                .map_err(|_| ())?;
            // On rejected retirement retain the graph, so disable/shutdown can retry.
            self.graphs.lock().map_err(|_| ())?.remove(account);
        }
        Ok(())
    }

    pub(crate) fn submit(&self, account: &str) -> Result<MutationRuntimeView, ()> {
        let _transition = self.transition.lock().map_err(|_| ())?;
        if !self.eligible(account) {
            return Err(());
        }
        let (graph, permit) = {
            let graphs = self.graphs.lock().map_err(|_| ())?;
            let entry = graphs.get(account).ok_or(())?;
            if entry.scoped_only {
                return Err(());
            }
            (
                entry.graph.clone(),
                ActivationPermit {
                    graph: Arc::downgrade(&entry.graph),
                    enabled: Arc::downgrade(&entry.enabled),
                    event: None,
                },
            )
        };
        graph.submit_activated(permit)
    }

    pub(crate) fn submit_scoped_event(
        &self,
        binding: crate::adapters::production_mutation::exact_probe::ExactProbeBinding,
        scope: crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> Result<MutationRuntimeView, ()> {
        let _transition = self.transition.lock().map_err(|_| ())?;
        let binding = binding.revalidate()?;
        let account = scope.ids().1.to_owned();
        if !binding.matches_event(&scope) || !self.eligible(&account) {
            return Err(());
        }
        let stale = self
            .graphs
            .lock()
            .map_err(|_| ())?
            .get(&account)
            .is_some_and(|entry| {
                !entry.enabled.load(Ordering::SeqCst) || !entry.graph.view().owner_current
            });
        if stale {
            // Connect/replacement retires the captured attachment graph. Its map
            // entry cannot be reused for the new current session.
            self.retire(&account)?;
        }
        if !self.graphs.lock().map_err(|_| ())?.contains_key(&account) {
            let graph = Arc::new(
                self.owners
                    .attachments
                    .compose_execution_closed_with_reminders(
                        &self.owners.sessions,
                        &self.owners.creation,
                        &self.owners.conflicts,
                        &account,
                        self.owners
                            .reminders
                            .scheduling_composer()
                            .map_err(|_| ())?,
                    )?,
            );
            if let Some(emitter) = self.scoped_emitter.lock().map_err(|_| ())?.clone() {
                graph.install_scoped_emitter(emitter)?;
            }
            if !graph.view().owner_current || !binding.event_owner_current() {
                return Err(());
            }
            self.graphs.lock().map_err(|_| ())?.insert(
                account.clone(),
                EnabledGraph {
                    graph,
                    enabled: Arc::new(AtomicBool::new(true)),
                    scoped_only: true,
                },
            );
        }
        let (graph, permit) = {
            let graphs = self.graphs.lock().map_err(|_| ())?;
            let entry = graphs.get(&account).ok_or(())?;
            (
                entry.graph.clone(),
                ActivationPermit {
                    graph: Arc::downgrade(&entry.graph),
                    enabled: Arc::downgrade(&entry.enabled),
                    event: Some((binding, scope)),
                },
            )
        };
        graph.submit_scoped_activated(permit)
    }

    pub(crate) fn scoped_event_view(
        &self,
        scope: &crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> Result<super::remote_mutation_runtime::ScopedEventRuntimeView, ()> {
        let graph = self
            .graphs
            .lock()
            .map_err(|_| ())?
            .get(scope.ids().1)
            .ok_or(())?
            .graph
            .clone();
        graph.scoped_event_view(scope)
    }
    pub(crate) fn install_scoped_emitter(
        &self,
        emitter: Arc<dyn super::remote_mutation_runtime::MutationStateEmitter>,
    ) -> Result<(), ()> {
        let _transition = self.transition.lock().map_err(|_| ())?;
        if self.closing.load(Ordering::SeqCst) || !self.graphs.lock().map_err(|_| ())?.is_empty() {
            return Err(());
        }
        let mut current = self.scoped_emitter.lock().map_err(|_| ())?;
        if current.is_some() {
            return Err(());
        }
        *current = Some(emitter);
        Ok(())
    }

    pub(crate) fn shutdown(&self) -> Result<(), ()> {
        self.closing.store(true, Ordering::SeqCst);
        let _transition = self.transition.lock().map_err(|_| ())?;
        {
            let graphs = self.graphs.lock().map_err(|_| ())?;
            for entry in graphs.values() {
                entry.enabled.store(false, Ordering::SeqCst);
            }
        }
        self.owners
            .attachments
            .try_shutdown_closed()
            .map_err(|_| ())?;
        self.graphs.lock().map_err(|_| ())?.clear();
        Ok(())
    }
}

impl Drop for RemoteExecutionActivation {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
