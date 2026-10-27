use crate::domain::remote_mutation::{
    RemoteDeletionClaim, RemoteDeletionReadBack, RemoteMutationClaim, RemoteReadBack,
};
use thiserror::Error;

/// Opaque, borrowed ownership metadata. Only the real mutation runtime can
/// supply request evidence; callback-only service callers cannot authorize a
/// production exchange. It retains no sender, transport or execution handle.
pub struct MutationTransportOwnership<'a> {
    request: Option<&'a crate::application::remote_mutation_runtime::MutationRequestGate>,
    accept: Option<&'a dyn Fn() -> bool>,
}

impl<'a> MutationTransportOwnership<'a> {
    pub(crate) fn request(
        &self,
    ) -> Option<&'a crate::application::remote_mutation_runtime::MutationRequestGate> {
        self.request
    }
    pub(crate) fn event_scope(
        &self,
    ) -> Option<&crate::adapters::persistence::scoped_event::ScopedEventSnapshot> {
        self.request.and_then(|request| request.event_scope())
    }
    pub(crate) fn from_request(
        request: &'a crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Self {
        Self {
            request: Some(request),
            accept: None,
        }
    }

    pub(crate) fn from_callback(accept: &'a dyn Fn() -> bool) -> Self {
        Self {
            request: None,
            accept: Some(accept),
        }
    }

    pub(crate) fn current(&self) -> bool {
        self.accept.is_none_or(|accept| accept())
            && self
                .request
                .map_or_else(|| self.accept.is_some(), |request| request.current())
    }

    pub(crate) fn current_for(
        &self,
        registry: &crate::application::account_registry::AccountSourceRegistry,
        account: &str,
        generation: u64,
    ) -> bool {
        self.accept.is_none_or(|accept| accept())
            && self
                .request
                .is_some_and(|request| request.current_for(registry, account, generation))
    }

    /// Adapter-only narrowing: exact captured session liveness supplements,
    /// and can never replace, the original real request evidence.
    pub(crate) fn with_session_check(&'a self, accept: &'a dyn Fn() -> bool) -> Self {
        Self {
            request: self.request,
            accept: Some(accept),
        }
    }
}

impl std::fmt::Debug for MutationTransportOwnership<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MutationTransportOwnership(<redacted>)")
    }
}

/// Internal GET-only creation recovery boundary. No production implementation
/// is registered. Implementations must call `accept` immediately before GET
/// and read only the actor claim's exact target, never search other calendars.
pub trait RemoteCreationReader: Send + Sync {
    fn read_creation(
        &self,
        claim: &crate::domain::remote_creation::RemoteCreationClaim,
        accept: &dyn Fn() -> bool,
    ) -> Result<crate::domain::remote_creation::RemoteCreationReadBack, RemoteMutationTransportError>;

    fn cancel_creation_in_flight(&self, _account_id: &str) -> bool {
        false
    }
}

/// Internal mutation transport. Production startup deliberately registers no
/// implementation; deterministic tests inject fakes through this boundary.
pub trait RemoteMutationTransport: Send + Sync {
    fn conditional_update_guarded(
        &self,
        claim: &RemoteMutationClaim,
        patched_ics: &[u8],
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<(), RemoteMutationTransportError> {
        if !ownership.current() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        self.conditional_update(claim, patched_ics)
    }

    fn read_back_guarded(
        &self,
        claim: &RemoteMutationClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        if !ownership.current() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        self.read_back(claim)
    }
    fn conditional_update(
        &self,
        claim: &RemoteMutationClaim,
        patched_ics: &[u8],
    ) -> Result<(), RemoteMutationTransportError>;

    fn read_back(
        &self,
        claim: &RemoteMutationClaim,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError>;

    /// Best-effort interruption for the account's current fake/internal
    /// exchange. Generation checks remain authoritative after cancellation.
    fn cancel_in_flight(&self, _account_id: &str) -> bool {
        false
    }
}

/// Separate transport capability for exact-resource deletion. Keeping this
/// distinct prevents the title runtime from dispatching a DELETE claim.
pub trait RemoteDeletionTransport: Send + Sync {
    fn conditional_delete_guarded(
        &self,
        claim: &RemoteDeletionClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<(), RemoteMutationTransportError> {
        if !ownership.current() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        self.conditional_delete(claim)
    }

    fn read_back_deletion_guarded(
        &self,
        claim: &RemoteDeletionClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteDeletionReadBack, RemoteMutationTransportError> {
        if !ownership.current() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        self.read_back_deletion(claim)
    }
    fn conditional_delete(
        &self,
        claim: &RemoteDeletionClaim,
    ) -> Result<(), RemoteMutationTransportError>;

    fn read_back_deletion(
        &self,
        claim: &RemoteDeletionClaim,
    ) -> Result<RemoteDeletionReadBack, RemoteMutationTransportError>;

    fn cancel_deletion_in_flight(&self, _account_id: &str) -> bool {
        false
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMutationTransportError {
    // Failure classes describe recovery/diagnostic needs, not non-delivery proof.
    #[error("remote mutation encountered a transient failure")]
    Transient,
    #[error("remote mutation outcome is ambiguous")]
    Ambiguous,
    #[error("remote mutation requires credential action")]
    Unauthorized,
    #[error("remote mutation precondition failed")]
    PreconditionFailed,
    #[error("remote mutation response is unsafe")]
    UnsafeData,
    #[error("remote mutation was cancelled")]
    Cancelled,
}

/// Independent v2 boundary; default methods deny all exchanges.
/// Production requires real request and exact captured session evidence.
pub(crate) trait RemoteFieldsTransport: Send + Sync {
    fn account_id(&self) -> &str;
    fn current(&self, ownership: &MutationTransportOwnership<'_>) -> bool;
    fn put(
        &self,
        _claim: &crate::domain::remote_mutation::RemoteFieldsMutationClaim,
        _snapshot: crate::domain::remote_mutation::RemoteFieldsSendSnapshot,
        _ownership: &MutationTransportOwnership<'_>,
    ) -> Result<(), RemoteMutationTransportError> {
        Err(RemoteMutationTransportError::Cancelled)
    }
    fn get(
        &self,
        _claim: &crate::domain::remote_mutation::RemoteFieldsMutationClaim,
        _ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        Err(RemoteMutationTransportError::Cancelled)
    }
}
