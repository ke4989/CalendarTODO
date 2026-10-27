use std::sync::{Arc, Weak};

/// Opaque, weak ownership metadata for the bounded conflict surface.
/// It carries no facade, store, emitter, conflict content, intent, or execution handle.
#[derive(Clone)]
pub(crate) enum ReviewedMutationConflictSurface {
    Unavailable,
    Scoped(ScopedConflictSurface),
}

impl ReviewedMutationConflictSurface {
    pub(crate) fn matches_gate(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
    ) -> bool {
        matches!(self, Self::Scoped(surface) if surface.attachment.matches_gate(gate) && surface.verify())
    }
    pub(crate) fn unavailable() -> Self {
        Self::Unavailable
    }

    pub(crate) fn verify(&self) -> bool {
        match self {
            Self::Unavailable => false,

            Self::Scoped(surface) => surface.verify(),
        }
    }

    pub(crate) fn verify_for(
        &self,
        accounts: &crate::application::account_registry::AccountSourceRegistry,
        account: &str,
        generation: u64,
    ) -> bool {
        match self {
            Self::Scoped(surface) => surface.verify_for(accounts, account, generation),

            _ => false,
        }
    }

    pub(in crate::application) fn scoped(
        _: &crate::application::remote_mutation_facade::CompleteMutationConflictSurface,
        owner: &Arc<()>,
        actor: Weak<()>,
        registry: Weak<()>,
        attachment: crate::application::attachment_runtime::ConflictAttachmentOwner,
    ) -> Self {
        Self::Scoped(ScopedConflictSurface {
            owner: Arc::downgrade(owner),
            actor,
            registry,
            attachment,
        })
    }
}

#[derive(Clone)]
pub(crate) struct ScopedConflictSurface {
    owner: Weak<()>,
    actor: Weak<()>,
    registry: Weak<()>,
    attachment: crate::application::attachment_runtime::ConflictAttachmentOwner,
}
impl ScopedConflictSurface {
    fn verify(&self) -> bool {
        self.owner.upgrade().is_some()
            && self.actor.upgrade().is_some()
            && self.registry.upgrade().is_some()
            && self.attachment.current(&self.registry, &self.actor)
    }
    fn verify_for(
        &self,
        accounts: &crate::application::account_registry::AccountSourceRegistry,
        account: &str,
        generation: u64,
    ) -> bool {
        let (registry, actor) = accounts.creation_owner_provenance();
        Weak::ptr_eq(&registry, &self.registry)
            && Weak::ptr_eq(&actor, &self.actor)
            && self.attachment.matches(account, generation)
            && self.verify()
    }
}
impl std::fmt::Debug for ReviewedMutationConflictSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReviewedMutationConflictSurface(<redacted>)")
    }
}
