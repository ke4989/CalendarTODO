use crate::application::{
    account_registry::{AccountRegistryError, AccountSourceRegistry},
    production_mutation_policy::{ProductionMutationAdmission, ProductionMutationAdmissionError},
    remote_mutation_runtime::{MutationRuntimeError, RemoteMutationRuntime},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use thiserror::Error;

trait MutationLifecycleRuntime: Send + Sync + 'static {
    fn start(
        &self,
        account_id: &str,
        attachment_generation: u64,
    ) -> Result<(), MutationRuntimeError>;
    fn cancel_account(&self, account_id: &str);
    fn shutdown(&self);
}

impl MutationLifecycleRuntime for RemoteMutationRuntime {
    fn start(
        &self,
        account_id: &str,
        attachment_generation: u64,
    ) -> Result<(), MutationRuntimeError> {
        self.submit_for_attachment(account_id, attachment_generation)
            .map(|_| ())
    }

    fn cancel_account(&self, account_id: &str) {
        self.account_detached(account_id);
    }

    fn shutdown(&self) {
        RemoteMutationRuntime::shutdown(self);
    }
}

trait MutationAdmission: Send + Sync + 'static {
    fn admit(
        &self,
        account_id: &str,
        attachment_generation: u64,
    ) -> Result<(), ProductionMutationAdmissionError>;
    fn shutdown(&self);
}

impl MutationAdmission for ProductionMutationAdmission {
    fn admit(
        &self,
        account_id: &str,
        attachment_generation: u64,
    ) -> Result<(), ProductionMutationAdmissionError> {
        ProductionMutationAdmission::admit(self, account_id, attachment_generation)
    }

    fn shutdown(&self) {
        ProductionMutationAdmission::shutdown(self);
    }
}

trait MutationAccountLifecycle: Send + Sync + 'static {
    fn set_enabled(&self, account_id: &str, enabled: bool) -> Result<(), AccountRegistryError>;
    fn detach(&self, account_id: &str) -> Result<(), AccountRegistryError>;
    fn remove(&self, account_id: &str) -> Result<(), AccountRegistryError>;
    fn detach_all(&self) -> Result<(), AccountRegistryError>;
}

impl MutationAccountLifecycle for AccountSourceRegistry {
    fn set_enabled(&self, account_id: &str, enabled: bool) -> Result<(), AccountRegistryError> {
        AccountSourceRegistry::set_enabled(self, account_id, enabled).map(|_| ())
    }

    fn detach(&self, account_id: &str) -> Result<(), AccountRegistryError> {
        self.detach_source(account_id)
    }

    fn remove(&self, account_id: &str) -> Result<(), AccountRegistryError> {
        self.archive_profile(account_id)
    }

    fn detach_all(&self) -> Result<(), AccountRegistryError> {
        for account in self.list()? {
            self.detach_source(&account.profile.account_id)?;
        }
        Ok(())
    }
}

/// Internal lifecycle composition for a future production mutation worker.
/// It is deliberately absent from Tauri/AppState and its normal admission
/// dependency remains capability-unavailable.
pub struct RemoteMutationLifecycle {
    runtime: Arc<dyn MutationLifecycleRuntime>,
    admission: Arc<dyn MutationAdmission>,
    accounts: Arc<dyn MutationAccountLifecycle>,
    stopped: AtomicBool,
    lifecycle_gate: Mutex<bool>,
}

impl RemoteMutationLifecycle {
    pub fn unavailable(
        runtime: Arc<RemoteMutationRuntime>,
        admission: Arc<ProductionMutationAdmission>,
        accounts: Arc<AccountSourceRegistry>,
    ) -> Self {
        Self {
            runtime,
            admission,
            accounts,
            stopped: AtomicBool::new(false),
            lifecycle_gate: Mutex::new(false),
        }
    }

    pub fn start(
        &self,
        account_id: &str,
        attachment_generation: u64,
    ) -> Result<(), RemoteMutationLifecycleError> {
        let _gate = self.lifecycle_gate.lock().map_err(|_| {
            RemoteMutationLifecycleError::Account(AccountRegistryError::Unavailable)
        })?;
        if self.stopped.load(Ordering::SeqCst) {
            return Err(RemoteMutationLifecycleError::Stopped);
        }
        self.admission.admit(account_id, attachment_generation)?;
        self.runtime.start(account_id, attachment_generation)?;
        Ok(())
    }

    pub fn set_enabled(
        &self,
        account_id: &str,
        enabled: bool,
    ) -> Result<(), RemoteMutationLifecycleError> {
        let _gate = self.lifecycle_gate.lock().map_err(|_| {
            RemoteMutationLifecycleError::Account(AccountRegistryError::Unavailable)
        })?;
        if self.stopped.load(Ordering::SeqCst) {
            return Err(RemoteMutationLifecycleError::Stopped);
        }
        if !enabled {
            self.runtime.cancel_account(account_id);
        }
        self.accounts.set_enabled(account_id, enabled)?;
        Ok(())
    }

    pub fn detach(&self, account_id: &str) -> Result<(), RemoteMutationLifecycleError> {
        let _gate = self.lifecycle_gate.lock().map_err(|_| {
            RemoteMutationLifecycleError::Account(AccountRegistryError::Unavailable)
        })?;
        self.detach_locked(account_id)
    }

    fn detach_locked(&self, account_id: &str) -> Result<(), RemoteMutationLifecycleError> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(RemoteMutationLifecycleError::Stopped);
        }
        self.runtime.cancel_account(account_id);
        self.accounts.detach(account_id)?;
        Ok(())
    }

    /// Cancel mutation ownership and detach the old read source before the
    /// existing attachment runtime constructs and installs its replacement.
    pub fn prepare_replacement(
        &self,
        account_id: &str,
    ) -> Result<(), RemoteMutationLifecycleError> {
        let _gate = self.lifecycle_gate.lock().map_err(|_| {
            RemoteMutationLifecycleError::Account(AccountRegistryError::Unavailable)
        })?;
        self.detach_locked(account_id)
    }

    pub fn remove(&self, account_id: &str) -> Result<(), RemoteMutationLifecycleError> {
        let _gate = self.lifecycle_gate.lock().map_err(|_| {
            RemoteMutationLifecycleError::Account(AccountRegistryError::Unavailable)
        })?;
        if self.stopped.load(Ordering::SeqCst) {
            return Err(RemoteMutationLifecycleError::Stopped);
        }
        self.runtime.cancel_account(account_id);
        self.accounts.remove(account_id)?;
        Ok(())
    }

    pub fn shutdown(&self) -> Result<(), RemoteMutationLifecycleError> {
        let mut detached = self.lifecycle_gate.lock().map_err(|_| {
            RemoteMutationLifecycleError::Account(AccountRegistryError::Unavailable)
        })?;
        if *detached {
            return Ok(());
        }
        if !self.stopped.swap(true, Ordering::SeqCst) {
            self.runtime.shutdown();
            self.admission.shutdown();
        }
        self.accounts.detach_all()?;
        *detached = true;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum RemoteMutationLifecycleError {
    #[error("remote mutation lifecycle is stopped")]
    Stopped,
    #[error(transparent)]
    Admission(#[from] ProductionMutationAdmissionError),
    #[error(transparent)]
    Runtime(#[from] MutationRuntimeError),
    #[error(transparent)]
    Account(#[from] AccountRegistryError),
}
