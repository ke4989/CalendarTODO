#![allow(dead_code)]

use crate::application::{
    account_registry::AccountSourceRegistry,
    remote_mutation_runtime::{ClosedMutationLifecycle, RemoteMutationRuntime},
};
use serde::Serialize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Execution-closed product composition checkpoint. It owns the reviewed
/// internal worker graph but exposes no submit, runner, transport or sender.
pub(crate) struct ExecutionClosedProductionMutationComposition {
    account_id: String,
    attachment_generation: u64,
    accounts: Arc<AccountSourceRegistry>,
    runtime: RemoteMutationRuntime,
    services: Arc<crate::adapters::production_mutation::VersionedMutationServices>,
    lifecycle: Arc<ClosedMutationLifecycle>,
    stopped: AtomicBool,
    schedule_owner: std::sync::Mutex<
        Option<Arc<dyn crate::application::remote_mutation_runtime::ClosedEventSchedulePort>>,
    >,
    conflicts: crate::application::production_mutation_conflict::ReviewedMutationConflictSurface,
}

pub(crate) type ClosedCompositionResult = Result<ExecutionClosedProductionMutationComposition, ()>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecutionClosedProductionMutationView {
    pub account_id: String,
    pub attachment_generation: u64,
    pub owner_current: bool,
    pub production_execution_available: bool,
}

impl ExecutionClosedProductionMutationComposition {
    pub(crate) fn from_versioned_parts(
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
        account_id: String,
        attachment_generation: u64,
        accounts: Arc<AccountSourceRegistry>,
        services: Arc<crate::adapters::production_mutation::VersionedMutationServices>,
        runtime: RemoteMutationRuntime,
        conflicts: crate::application::production_mutation_conflict::ReviewedMutationConflictSurface,
    ) -> Result<Self, ()> {
        if account_id.is_empty()
            || attachment_generation == 0
            || services.account() != account_id
            || !services.belongs_to(&accounts)
            || !services.current_under_gate(gate)
            || !accounts.is_current_attachment(&account_id, attachment_generation)
            || !conflicts.verify_for(&accounts, &account_id, attachment_generation)
            || !conflicts.matches_gate(gate)
        {
            return Err(());
        }
        let lifecycle = runtime.closed_versioned_lifecycle(
            &account_id,
            attachment_generation,
            &services,
            &accounts,
        )?;
        Ok(Self {
            account_id,
            attachment_generation,
            accounts,
            services,
            runtime,
            lifecycle: Arc::new(lifecycle),
            stopped: AtomicBool::new(false),
            schedule_owner: std::sync::Mutex::new(None),
            conflicts,
        })
    }

    pub(crate) fn install_notification_scope(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
        composer: crate::application::reminder_deadlines::ProductionReminderComposer,
    ) -> Result<(), ()> {
        if self.stopped.load(Ordering::SeqCst)
            || !self.services.current_under_gate(gate)
            || !self.conflicts.matches_gate(gate)
        {
            return Err(());
        }
        let scope = composer.mint(
            gate,
            &self.accounts,
            &self.account_id,
            self.attachment_generation,
            &self.lifecycle,
            &self.services,
        )?;
        let mut owner = self.schedule_owner.lock().map_err(|_| ())?;
        if owner.is_some() {
            return Err(());
        }
        self.runtime
            .bind_production_schedule_port(&self.lifecycle, &scope)?;
        let notification_live = scope.notification_live();
        *owner = Some(scope);
        if !notification_live
            || !self.services.current_under_gate(gate)
            || !self.lifecycle.scheduling_alive()
        {
            return Err(());
        }
        Ok(())
    }

    pub(crate) fn event_schedule_failed(&self) -> bool {
        self.runtime.event_schedule_failed()
    }
    pub(crate) fn view(&self) -> ExecutionClosedProductionMutationView {
        let notification_current = self.schedule_owner.lock().is_ok_and(|owner| {
            owner
                .as_ref()
                .is_none_or(|port| port.notification_current())
        });
        let owner_current = notification_current
            && !self.stopped.load(Ordering::SeqCst)
            && self.conflicts.verify_for(
                &self.accounts,
                &self.account_id,
                self.attachment_generation,
            )
            && self.lifecycle.is_live()
            && self.services.belongs_to(&self.accounts)
            && self
                .accounts
                .is_current_attachment(&self.account_id, self.attachment_generation);
        ExecutionClosedProductionMutationView {
            account_id: self.account_id.clone(),
            attachment_generation: self.attachment_generation,
            owner_current,
            production_execution_available: false,
        }
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown_deferred().publish_pending();
    }

    pub(crate) fn shutdown_deferred(&self) -> Arc<ClosedMutationLifecycle> {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            let _ = self.lifecycle.retire_local();
        }
        self.lifecycle.clone()
    }

    pub(crate) fn lifecycle(&self) -> &ClosedMutationLifecycle {
        &self.lifecycle
    }
    pub(crate) fn lifecycle_registration(
        &self,
    ) -> ((String, u64), std::sync::Weak<ClosedMutationLifecycle>) {
        (
            (self.account_id.clone(), self.attachment_generation),
            Arc::downgrade(&self.lifecycle),
        )
    }

    pub(crate) fn submit_activated(
        &self,
        permit: super::remote_execution_activation::ActivationPermit,
    ) -> Result<super::remote_mutation_runtime::MutationRuntimeView, ()> {
        if !permit.authorizes(self) || !self.view().owner_current {
            return Err(());
        }
        self.runtime
            .submit_for_attachment(&self.account_id, self.attachment_generation)
            .map_err(|_| ())
    }
    pub(crate) fn submit_scoped_activated(
        &self,
        mut permit: super::remote_execution_activation::ActivationPermit,
    ) -> Result<super::remote_mutation_runtime::MutationRuntimeView, ()> {
        if !permit.authorizes(self) || !self.view().owner_current {
            return Err(());
        }
        let (binding, scope) = permit.take_event().ok_or(())?;
        if scope.ids().1 != self.account_id {
            return Err(());
        }
        self.runtime
            .submit_scoped_event(self.attachment_generation, binding, scope)
            .map_err(|_| ())
    }
    pub(crate) fn scoped_event_view(
        &self,
        scope: &crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> Result<super::remote_mutation_runtime::ScopedEventRuntimeView, ()> {
        if !self.view().owner_current || scope.ids().1 != self.account_id {
            return Err(());
        }
        self.runtime.scoped_event_view(scope)
    }
    pub(crate) fn install_scoped_emitter(
        &self,
        emitter: Arc<dyn super::remote_mutation_runtime::MutationStateEmitter>,
    ) -> Result<(), ()> {
        if !self.view().owner_current {
            return Err(());
        }
        self.runtime.install_scoped_emitter(emitter)
    }
}

impl Drop for ExecutionClosedProductionMutationComposition {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for ExecutionClosedProductionMutationComposition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ExecutionClosedProductionMutationComposition(<redacted>)")
    }
}
