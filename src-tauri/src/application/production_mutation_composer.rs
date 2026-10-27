use crate::application::{
    account_registry::{AccountRegistryError, AccountSourceRegistry},
    attachment_runtime::AttachmentRuntime,
    production_mutation_capability::{ConditionalTitleUpdateDenial, ConditionalTitleUpdateTarget},
    production_mutation_conflict::ReviewedMutationConflictSurface,
    production_mutation_lifecycle::ReviewedMutationLifecycleIntegration,
    production_mutation_policy::{
        ProductionMutationAdmission, ProductionMutationAdmissionError, ProductionMutationDenial,
    },
    production_mutation_transport::{
        MutationTransportOperation, ReviewedMutationTransportRegistration,
    },
    remote_mutation_facade::RemoteMutationApplication,
};
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use thiserror::Error;

/// Internal, fail-closed composition for one exact conditional title-update
/// target. It selects no transport and is deliberately absent from AppState.
pub struct ConditionalTitleUpdateAdmissionComposer {
    accounts: Arc<AccountSourceRegistry>,
    admission: ProductionMutationAdmission,
    transport_registration: ReviewedMutationTransportRegistration,
    lifecycle_integration: ReviewedMutationLifecycleIntegration,
    conflict_surface: ReviewedMutationConflictSurface,
}

/// Opaque proof that every production-mutation prerequisite was satisfied for
/// one exact target and attachment generation. It carries no execution handle.
pub struct ConditionalTitleUpdateAdmissionPermit {
    target: ConditionalTitleUpdateTarget,
    operation: MutationTransportOperation,
    revoked: Arc<AtomicBool>,
    transport_revoked: Arc<AtomicBool>,
    lifecycle_integration: ReviewedMutationLifecycleIntegration,
    conflict_surface: ReviewedMutationConflictSurface,
}

impl ConditionalTitleUpdateAdmissionPermit {
    fn new(
        target: ConditionalTitleUpdateTarget,
        revoked: Arc<AtomicBool>,
        transport_revoked: Arc<AtomicBool>,
        lifecycle_integration: ReviewedMutationLifecycleIntegration,
        conflict_surface: ReviewedMutationConflictSurface,
    ) -> Self {
        Self {
            target,
            operation: MutationTransportOperation::ConditionalTitleUpdate,
            revoked,
            transport_revoked,
            lifecycle_integration,
            conflict_surface,
        }
    }

    /// Checks current attachment and account state at this instant. Execution
    /// must still repeat admission under its own ownership and cancellation gate.
    pub fn matches_current_conditional_title_update_target(
        &self,
        accounts: &AccountSourceRegistry,
        account_id: &str,
        calendar_id: &str,
        resource_id: &str,
        attachment_generation: u64,
    ) -> bool {
        let Ok(requested) = ConditionalTitleUpdateTarget::new(
            account_id.to_owned(),
            calendar_id.to_owned(),
            resource_id.to_owned(),
            attachment_generation,
        ) else {
            return false;
        };
        !self.revoked.load(Ordering::SeqCst)
            && !self.transport_revoked.load(Ordering::SeqCst)
            && self.lifecycle_integration.verify()
            && self
                .conflict_surface
                .verify_for(accounts, account_id, attachment_generation)
            && self.target == requested
            && self.operation == MutationTransportOperation::ConditionalTitleUpdate
            && accounts.is_current_attachment(account_id, attachment_generation)
            && accounts.profile(account_id).is_ok_and(|profile| {
                profile.enabled && profile.credential_configured && profile.remote_editing_enabled
            })
    }
}

impl fmt::Debug for ConditionalTitleUpdateAdmissionPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConditionalTitleUpdateAdmissionPermit(REDACTED)")
    }
}

impl ConditionalTitleUpdateAdmissionComposer {
    pub fn unavailable(accounts: Arc<AccountSourceRegistry>) -> Self {
        Self {
            admission: ProductionMutationAdmission::unavailable(accounts.clone()),
            accounts,
            transport_registration: ReviewedMutationTransportRegistration::unavailable(),
            lifecycle_integration: ReviewedMutationLifecycleIntegration::unavailable(),
            conflict_surface: ReviewedMutationConflictSurface::unavailable(),
        }
    }

    /// Composes only opaque evidence from the reviewed owners. This still
    /// carries no transport or dispatch capability and remains absent from AppState.
    #[allow(dead_code)]
    pub(crate) fn with_reviewed_evidence(
        accounts: Arc<AccountSourceRegistry>,
        transport_registration: ReviewedMutationTransportRegistration,
        lifecycle_integration: ReviewedMutationLifecycleIntegration,
        conflict_surface: ReviewedMutationConflictSurface,
    ) -> Self {
        Self {
            admission: ProductionMutationAdmission::unavailable(accounts.clone()),
            accounts,
            transport_registration,
            lifecycle_integration,
            conflict_surface,
        }
    }

    /// Pure admission composition from the actual installed owners. Transport
    /// and lifecycle evidence come from the same attachment-runtime session
    /// owner; the returned composer still owns no executor or dispatch path.
    #[allow(dead_code)]
    pub(crate) fn from_reviewed_owners(
        attachments: &AttachmentRuntime,
        conflicts: &RemoteMutationApplication,
        account_id: &str,
    ) -> Result<Self, ReviewedOwnerCompositionError> {
        let (accounts, transport_registration, lifecycle_integration) = attachments
            .reviewed_title_update_evidence(account_id)
            .map_err(|_| ReviewedOwnerCompositionError::Unavailable)?;
        let conflict_surface = attachments
            .reviewed_conflict_evidence(conflicts, account_id)
            .map_err(|_| ReviewedOwnerCompositionError::Unavailable)?;
        Ok(Self::with_reviewed_evidence(
            accounts,
            transport_registration,
            lifecycle_integration,
            conflict_surface,
        ))
    }

    pub fn admit(
        &self,
        account_id: &str,
        calendar_id: &str,
        resource_id: &str,
    ) -> Result<ConditionalTitleUpdateAdmissionPermit, ConditionalTitleUpdateAdmissionError> {
        let generation = self.accounts.attachment_generation(account_id).ok_or(
            ProductionMutationAdmissionError::Denied(ProductionMutationDenial::SourceDetached),
        )?;
        let target = ConditionalTitleUpdateTarget::new(
            account_id.to_owned(),
            calendar_id.to_owned(),
            resource_id.to_owned(),
            generation,
        )?;
        let evidence = self
            .accounts
            .conditional_title_update_evidence(target.clone())?;
        // Admission re-reads registry ownership after the actor query, closing
        // the replacement/detach race without trusting a caller generation.
        self.admission.admit_with_evidence(
            &target,
            &evidence,
            &self.transport_registration,
            &self.lifecycle_integration,
            &self.conflict_surface,
        )?;
        let transport_revoked = self
            .transport_registration
            .revocation_state()
            .ok_or(ProductionMutationDenial::TransportUnavailable)?;
        Ok(ConditionalTitleUpdateAdmissionPermit::new(
            target,
            self.admission.revocation_state(),
            transport_revoked,
            self.lifecycle_integration.clone(),
            self.conflict_surface.clone(),
        ))
    }

    pub fn shutdown(&self) {
        self.admission.shutdown();
    }
}

impl Drop for ConditionalTitleUpdateAdmissionComposer {
    fn drop(&mut self) {
        self.admission.shutdown();
    }
}

#[derive(Debug, Error)]
pub enum ConditionalTitleUpdateAdmissionError {
    #[error(transparent)]
    Account(#[from] AccountRegistryError),
    #[error(transparent)]
    Target(#[from] ConditionalTitleUpdateDenial),
    #[error(transparent)]
    Admission(#[from] ProductionMutationAdmissionError),
}

impl From<ProductionMutationDenial> for ConditionalTitleUpdateAdmissionError {
    fn from(value: ProductionMutationDenial) -> Self {
        Self::Admission(ProductionMutationAdmissionError::Denied(value))
    }
}

#[allow(dead_code)]
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ReviewedOwnerCompositionError {
    #[error("reviewed production mutation owners are unavailable")]
    Unavailable,
}
