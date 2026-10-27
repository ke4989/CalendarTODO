use crate::application::account_registry::{AccountRegistryError, AccountSourceRegistry};
use crate::application::{
    production_mutation_capability::{
        ConditionalTitleUpdateEvidence, ConditionalTitleUpdateTarget,
    },
    production_mutation_conflict::ReviewedMutationConflictSurface,
    production_mutation_lifecycle::ReviewedMutationLifecycleIntegration,
    production_mutation_transport::{
        MutationTransportOperation, ReviewedMutationTransportRegistration,
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use thiserror::Error;

/// Fail-closed prerequisites for selecting a future credentialed mutation
/// transport. This policy has no credential, URL, HTTP, persistence, or Tauri
/// dependency and is deliberately not wired into production composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductionMutationPrerequisites {
    pub account_enabled: bool,
    pub credential_configured: bool,
    pub user_remote_editing_opt_in: bool,
    pub source_attached: bool,
    pub attachment_generation_current: bool,
    pub provider_conditional_update_verified: bool,
    pub reviewed_transport_registered: bool,
    pub lifecycle_hooks_integrated: bool,
    pub conflict_resolution_surface_ready: bool,
}

impl ProductionMutationPrerequisites {
    pub fn admit(self) -> Result<(), ProductionMutationDenial> {
        let checks = [
            (
                self.account_enabled,
                ProductionMutationDenial::AccountDisabled,
            ),
            (
                self.credential_configured,
                ProductionMutationDenial::CredentialActionRequired,
            ),
            (
                self.user_remote_editing_opt_in,
                ProductionMutationDenial::UserOptInRequired,
            ),
            (
                self.source_attached,
                ProductionMutationDenial::SourceDetached,
            ),
            (
                self.attachment_generation_current,
                ProductionMutationDenial::StaleAttachment,
            ),
            (
                self.provider_conditional_update_verified,
                ProductionMutationDenial::ProviderCapabilityUnavailable,
            ),
            (
                self.reviewed_transport_registered,
                ProductionMutationDenial::TransportUnavailable,
            ),
            (
                self.lifecycle_hooks_integrated,
                ProductionMutationDenial::LifecycleIncomplete,
            ),
            (
                self.conflict_resolution_surface_ready,
                ProductionMutationDenial::ConflictSurfaceUnavailable,
            ),
        ];
        checks
            .into_iter()
            .find_map(|(ready, denial)| (!ready).then_some(denial))
            .map_or(Ok(()), Err)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ProductionMutationDenial {
    #[error("remote editing account is disabled")]
    AccountDisabled,
    #[error("remote editing requires credential action")]
    CredentialActionRequired,
    #[error("remote editing requires explicit user opt-in")]
    UserOptInRequired,
    #[error("remote editing source is detached")]
    SourceDetached,
    #[error("remote editing attachment ownership is stale")]
    StaleAttachment,
    #[error("provider conditional update capability is unavailable")]
    ProviderCapabilityUnavailable,
    #[error("reviewed remote mutation transport is unavailable")]
    TransportUnavailable,
    #[error("remote mutation lifecycle integration is incomplete")]
    LifecycleIncomplete,
    #[error("remote conflict resolution surface is unavailable")]
    ConflictSurfaceUnavailable,
}

/// Internal fail-closed composition of durable account authorization and
/// process-local attachment ownership. This object selects no transport and is
/// deliberately absent from Tauri application state.
pub struct ProductionMutationAdmission {
    accounts: Arc<AccountSourceRegistry>,
    stopped: Arc<AtomicBool>,
}

impl ProductionMutationAdmission {
    pub fn unavailable(accounts: Arc<AccountSourceRegistry>) -> Self {
        Self {
            accounts,
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn admit(
        &self,
        account_id: &str,
        claimed_attachment_generation: u64,
    ) -> Result<(), ProductionMutationAdmissionError> {
        self.prerequisites(account_id, claimed_attachment_generation)?
            .admit()
            .map_err(Into::into)
    }

    pub(crate) fn admit_with_evidence(
        &self,
        target: &ConditionalTitleUpdateTarget,
        provider: &ConditionalTitleUpdateEvidence,
        transport: &ReviewedMutationTransportRegistration,
        lifecycle: &ReviewedMutationLifecycleIntegration,
        conflict_surface: &ReviewedMutationConflictSurface,
    ) -> Result<(), ProductionMutationAdmissionError> {
        let mut prerequisites =
            self.prerequisites(target.account_id(), target.attachment_generation())?;
        prerequisites.provider_conditional_update_verified = provider.verify(target).is_ok();
        prerequisites.reviewed_transport_registered = transport.verify(
            target.account_id(),
            target.attachment_generation(),
            MutationTransportOperation::ConditionalTitleUpdate,
        );
        prerequisites.lifecycle_hooks_integrated =
            lifecycle.verify() && !self.stopped.load(Ordering::SeqCst);
        prerequisites.conflict_resolution_surface_ready = conflict_surface.verify_for(
            &self.accounts,
            target.account_id(),
            target.attachment_generation(),
        );
        prerequisites.admit().map_err(Into::into)
    }

    fn prerequisites(
        &self,
        account_id: &str,
        claimed_attachment_generation: u64,
    ) -> Result<ProductionMutationPrerequisites, ProductionMutationAdmissionError> {
        let profile = self.accounts.profile(account_id)?;
        let current_generation = self.accounts.attachment_generation(account_id);
        Ok(ProductionMutationPrerequisites {
            account_enabled: profile.enabled,
            credential_configured: profile.credential_configured,
            user_remote_editing_opt_in: profile.remote_editing_enabled,
            source_attached: current_generation.is_some(),
            attachment_generation_current: current_generation
                .is_some_and(|generation| generation == claimed_attachment_generation),
            provider_conditional_update_verified: false,
            reviewed_transport_registered: false,
            lifecycle_hooks_integrated: false,
            conflict_resolution_surface_ready: false,
        })
    }

    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub(crate) fn revocation_state(&self) -> Arc<AtomicBool> {
        self.stopped.clone()
    }
}

#[derive(Debug, Error)]
pub enum ProductionMutationAdmissionError {
    #[error(transparent)]
    Account(#[from] AccountRegistryError),
    #[error(transparent)]
    Denied(#[from] ProductionMutationDenial),
}
