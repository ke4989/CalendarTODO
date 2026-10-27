use crate::{
    adapters::persistence::{Database, PersistenceError},
    domain::remote_creation::{
        QueueRemoteEventCreate, RemoteCreationAccountView, RemoteCreationSummary,
    },
};
use std::{
    fmt,
    sync::{Arc, Weak},
};

/// Actual actor-backed admission/status owner; no worker, IPC or dispatch.
#[derive(Clone)]
pub struct RemoteCreationApplication {
    database: Database,
    owner: Arc<()>,
}

impl RemoteCreationApplication {
    pub fn without_execution(database: Database) -> Self {
        Self {
            database,
            owner: Arc::new(()),
        }
    }

    pub fn queue(
        &self,
        input: QueueRemoteEventCreate,
    ) -> Result<RemoteCreationSummary, PersistenceError> {
        self.database.queue_remote_event_create(input)
    }

    pub fn view(&self, account_id: &str) -> Result<RemoteCreationAccountView, PersistenceError> {
        self.database
            .remote_creation_account_view(account_id.to_owned())
    }

    /// Foundation liveness only, never complete production CREATE readiness.
    #[allow(dead_code)]
    pub(crate) fn admission_status_evidence(&self) -> CreationAdmissionStatusEvidence {
        CreationAdmissionStatusEvidence::from_owner(
            &CompleteCreationAdmissionStatus(()),
            &self.owner,
            self.database.actor_provenance(),
        )
    }
}

impl fmt::Debug for RemoteCreationApplication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteCreationApplication(<redacted>)")
    }
}

struct CompleteCreationAdmissionStatus(());

/// Does not keep the facade/store alive and cannot authorize execution.
#[derive(Clone)]
pub(crate) struct CreationAdmissionStatusEvidence(Weak<()>, Weak<()>);

#[allow(dead_code)]
impl CreationAdmissionStatusEvidence {
    fn from_owner(_: &CompleteCreationAdmissionStatus, owner: &Arc<()>, actor: Weak<()>) -> Self {
        Self(Arc::downgrade(owner), actor)
    }
    pub(crate) fn is_live(&self) -> bool {
        self.0.upgrade().is_some() && self.1.upgrade().is_some()
    }
    pub(crate) fn matches_actor(&self, actor: &Weak<()>) -> bool {
        self.is_live() && actor.upgrade().is_some() && Weak::ptr_eq(&self.1, actor)
    }
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        self.is_live() && other.is_live() && Weak::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for CreationAdmissionStatusEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CreationAdmissionStatusEvidence(<redacted>)")
    }
}
