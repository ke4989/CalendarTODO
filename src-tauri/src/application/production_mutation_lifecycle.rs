use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Opaque evidence that every required mutation lifecycle hook is integrated.
/// It carries no coordinator, runtime, callback, transport, or account data.
#[derive(Clone)]
pub(crate) struct ReviewedMutationLifecycleIntegration {
    reviewed: bool,
    revoked: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
}

impl ReviewedMutationLifecycleIntegration {
    pub(crate) fn unavailable() -> Self {
        Self {
            reviewed: false,
            revoked: Arc::new(AtomicBool::new(true)),
            closing: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(crate) fn verify(&self) -> bool {
        self.reviewed
            && !self.revoked.load(Ordering::SeqCst)
            && !self.closing.load(Ordering::SeqCst)
    }

    pub(in crate::application) fn reviewed(
        _owner: &crate::application::attachment_runtime::CompleteAttachmentMutationLifecycle,
        revoked: Arc<AtomicBool>,
        closing: Arc<AtomicBool>,
    ) -> Self {
        Self {
            reviewed: true,
            revoked,
            closing,
        }
    }
}
