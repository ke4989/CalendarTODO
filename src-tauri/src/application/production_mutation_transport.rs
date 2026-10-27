/// The only mutation operation whose persistence, patching, and capability
/// evidence exist. Naming it here does not register or select a transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationTransportOperation {
    ConditionalTitleUpdate,
}

/// Opaque authorization metadata for a future reviewed transport. It carries
/// no transport handle, credential, URL, request data, or lifecycle authority.
#[derive(Clone)]
pub(crate) enum ReviewedMutationTransportRegistration {
    Unavailable,
    Reviewed {
        account_id: String,
        attachment_generation: u64,
        operation: MutationTransportOperation,
        revoked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
}

impl ReviewedMutationTransportRegistration {
    pub(crate) fn unavailable() -> Self {
        Self::Unavailable
    }

    pub(crate) fn reviewed(
        account_id: &str,
        attachment_generation: u64,
        operation: MutationTransportOperation,
        _owner: &crate::adapters::production_mutation::CompleteProductionMutationTransportRegistration,
    ) -> Self {
        Self::Reviewed {
            account_id: account_id.to_owned(),
            attachment_generation,
            operation,
            revoked: _owner.revocation_state(),
        }
    }

    pub(crate) fn verify(
        &self,
        _account_id: &str,
        _attachment_generation: u64,
        _operation: MutationTransportOperation,
    ) -> bool {
        match self {
            Self::Unavailable => false,
            Self::Reviewed {
                account_id: registered_account,
                attachment_generation: registered_generation,
                operation: registered_operation,
                revoked,
            } => {
                !revoked.load(std::sync::atomic::Ordering::SeqCst)
                    && registered_account == _account_id
                    && *registered_generation == _attachment_generation
                    && *registered_operation == _operation
            }
        }
    }

    pub(crate) fn revocation_state(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        match self {
            Self::Unavailable => None,
            Self::Reviewed { revoked, .. } => Some(revoked.clone()),
        }
    }
}
