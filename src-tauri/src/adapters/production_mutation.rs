use crate::{
    adapters::{
        caldav_http::{HttpDavConfig, HttpDavExecutor},
        caldav_transport::{
            DavAuthentication, DavTransportPolicy, classify_conditional_update_result,
        },
        persistence::Database,
    },
    application::{
        account_registry::AccountSourceRegistry,
        credentials::CredentialService,
        production_mutation_transport::{
            MutationTransportOperation, ReviewedMutationTransportRegistration,
        },
    },
    domain::remote_mutation::{
        RemoteDeletionClaim, RemoteDeletionReadBack, RemoteMutationClaim, RemoteReadBack,
        patch_single_vevent_title,
    },
    ports::{
        clock::Clock,
        credentials::CredentialRef,
        remote_mutation::{
            MutationTransportOwnership, RemoteDeletionTransport, RemoteMutationTransport,
            RemoteMutationTransportError,
        },
    },
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use url::Url;

const ICLOUD_CHINA_REGION: &str = "icloud-cn";

#[allow(dead_code)]
pub(crate) mod exact_probe;
#[allow(dead_code)]
pub(crate) mod probe_preflight;
#[allow(dead_code)] // Used only after the deliberately unwired owner is composed.
const MUTATION_TIMEOUT_MS: u64 = 30_000;

/// Credentialed conditional-title transport scoped to one current attachment
/// session. It is intentionally not registered in AppState yet.
#[allow(dead_code)] // Deliberately unwired until lifecycle and conflict owners compose it.
pub(crate) struct ProductionTitleMutationTransport {
    account_id: String,
    attachment_generation: u64,
    entry_url: Url,
    accounts: Arc<AccountSourceRegistry>,
    database: Database,
    credentials: CredentialService,
    clock: Arc<dyn Clock>,
    policy: DavTransportPolicy,
    executor: Arc<HttpDavExecutor>,
    cancelled: Arc<AtomicBool>,
}

#[allow(dead_code)] // Deliberately unwired until lifecycle and conflict owners compose it.
impl ProductionTitleMutationTransport {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        account_id: String,
        attachment_generation: u64,
        entry_url: Url,
        accounts: Arc<AccountSourceRegistry>,
        database: Database,
        credentials: CredentialService,
        clock: Arc<dyn Clock>,
        policy: DavTransportPolicy,
        executor: Arc<HttpDavExecutor>,
    ) -> Result<Self, RemoteMutationTransportError> {
        if account_id.is_empty()
            || attachment_generation == 0
            || !accounts.is_current_attachment(&account_id, attachment_generation)
        {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        policy
            .validate_destination(&entry_url)
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        Ok(Self {
            account_id,
            attachment_generation,
            entry_url,
            accounts,
            database,
            credentials,
            clock,
            policy,
            executor,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    fn authentication(
        &self,
        account_id: &str,
    ) -> Result<DavAuthentication, RemoteMutationTransportError> {
        if self.cancelled.load(Ordering::SeqCst)
            || account_id != self.account_id
            || !self
                .accounts
                .is_current_attachment(&self.account_id, self.attachment_generation)
        {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let profile = self
            .accounts
            .profile(&self.account_id)
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        if !profile.enabled || !profile.remote_editing_enabled {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        if !profile.credential_configured || profile.region_profile != ICLOUD_CHINA_REGION {
            return Err(RemoteMutationTransportError::Unauthorized);
        }
        let reference = CredentialRef::parse(&profile.credential_ref)
            .map_err(|_| RemoteMutationTransportError::Unauthorized)?;
        let credential = self
            .credentials
            .read(&reference)
            .map_err(|_| RemoteMutationTransportError::Unauthorized)?;
        Ok(DavAuthentication::from_credential(&credential))
    }

    fn send_still_allowed(
        &self,
        claim: &RemoteMutationClaim,
    ) -> Result<bool, RemoteMutationTransportError> {
        if self.cancelled.load(Ordering::SeqCst)
            || claim.account_id != self.account_id
            || !self
                .accounts
                .is_current_attachment(&self.account_id, self.attachment_generation)
        {
            return Ok(false);
        }
        let profile = self
            .accounts
            .profile(&self.account_id)
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        if !profile.enabled || !profile.credential_configured || !profile.remote_editing_enabled {
            return Ok(false);
        }
        let now = self
            .clock
            .now_utc_ms()
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        self.database
            .remote_mutation_send_allowed(claim.claim_token.clone(), now)
            .map_err(|_| RemoteMutationTransportError::Cancelled)
    }

    fn deletion_send_still_allowed(
        &self,
        claim: &RemoteDeletionClaim,
    ) -> Result<bool, RemoteMutationTransportError> {
        if self.cancelled.load(Ordering::SeqCst)
            || claim.account_id != self.account_id
            || !self
                .accounts
                .is_current_attachment(&self.account_id, self.attachment_generation)
        {
            return Ok(false);
        }
        let profile = self
            .accounts
            .profile(&self.account_id)
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        if !profile.enabled || !profile.credential_configured || !profile.remote_editing_enabled {
            return Ok(false);
        }
        let now = self
            .clock
            .now_utc_ms()
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        self.database
            .remote_deletion_send_allowed(claim.claim_token.clone(), now)
            .map_err(|_| RemoteMutationTransportError::Cancelled)
    }
    fn deletion_read_still_allowed(
        &self,
        claim: &RemoteDeletionClaim,
    ) -> Result<bool, RemoteMutationTransportError> {
        if self.cancelled.load(Ordering::SeqCst)
            || claim.account_id != self.account_id
            || !self
                .accounts
                .is_current_attachment(&self.account_id, self.attachment_generation)
        {
            return Ok(false);
        }
        let profile = self
            .accounts
            .profile(&self.account_id)
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        if !profile.enabled || !profile.credential_configured || !profile.remote_editing_enabled {
            return Ok(false);
        }
        let now = self
            .clock
            .now_utc_ms()
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        self.database
            .remote_deletion_read_allowed(claim.claim_token.clone(), now)
            .map_err(|_| RemoteMutationTransportError::Cancelled)
    }
}

impl RemoteMutationTransport for ProductionTitleMutationTransport {
    fn conditional_update(
        &self,
        _claim: &RemoteMutationClaim,
        _patched_ics: &[u8],
    ) -> Result<(), RemoteMutationTransportError> {
        Err(RemoteMutationTransportError::Cancelled)
    }

    fn conditional_update_guarded(
        &self,
        claim: &RemoteMutationClaim,
        patched_ics: &[u8],
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<(), RemoteMutationTransportError> {
        if !ownership.current_for(&self.accounts, &self.account_id, self.attachment_generation) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let request = self
            .policy
            .prepare_claimed_title_update(&self.entry_url, claim, MUTATION_TIMEOUT_MS)
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let exact = patch_single_vevent_title(&claim.base_raw_ics, &claim.title)
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        if request.body() != exact || patched_ics != exact {
            return Err(RemoteMutationTransportError::UnsafeData);
        }
        let authentication = self.authentication(&claim.account_id)?;
        if !self.send_still_allowed(claim)? {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        classify_conditional_update_result(self.executor.execute_conditional_guarded(
            request,
            &authentication,
            &|| {
                ownership.current_for(&self.accounts, &self.account_id, self.attachment_generation)
                    && self.send_still_allowed(claim).unwrap_or(false)
                    && ownership.current_for(
                        &self.accounts,
                        &self.account_id,
                        self.attachment_generation,
                    )
                    && !self.cancelled.load(Ordering::SeqCst)
            },
        ))
    }

    fn read_back(
        &self,
        _claim: &RemoteMutationClaim,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        Err(RemoteMutationTransportError::Cancelled)
    }

    fn read_back_guarded(
        &self,
        claim: &RemoteMutationClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        if !ownership.current_for(&self.accounts, &self.account_id, self.attachment_generation) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let request = self
            .policy
            .prepare_claimed_read_back(&self.entry_url, claim, MUTATION_TIMEOUT_MS)
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let authentication = self.authentication(&claim.account_id)?;
        if !self
            .accounts
            .is_current_attachment(&self.account_id, self.attachment_generation)
        {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        request.execute_guarded(self.executor.as_ref(), &authentication, &|| {
            ownership.current_for(&self.accounts, &self.account_id, self.attachment_generation)
                && self.send_still_allowed(claim).unwrap_or(false)
                && ownership.current_for(
                    &self.accounts,
                    &self.account_id,
                    self.attachment_generation,
                )
                && !self.cancelled.load(Ordering::SeqCst)
        })
    }

    fn cancel_in_flight(&self, account_id: &str) -> bool {
        if account_id != self.account_id {
            return false;
        }
        let first = !self.cancelled.swap(true, Ordering::SeqCst);
        self.executor.cancel();
        first
    }
}

impl RemoteDeletionTransport for ProductionTitleMutationTransport {
    fn conditional_delete(
        &self,
        _claim: &RemoteDeletionClaim,
    ) -> Result<(), RemoteMutationTransportError> {
        Err(RemoteMutationTransportError::Cancelled)
    }

    fn conditional_delete_guarded(
        &self,
        claim: &RemoteDeletionClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<(), RemoteMutationTransportError> {
        let scoped = || {
            ownership.event_scope().is_none_or(|scope| {
                scope.matches_delete(claim) && self.database.scoped_event_status(scope).is_ok()
            })
        };
        if !scoped() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        if !ownership.current_for(&self.accounts, &self.account_id, self.attachment_generation) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let request = self
            .policy
            .prepare_claimed_delete(&self.entry_url, claim, MUTATION_TIMEOUT_MS)
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let authentication = self.authentication(&claim.account_id)?;
        if !self.deletion_send_still_allowed(claim)? {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        classify_conditional_update_result(self.executor.execute_conditional_delete_guarded(
            request,
            &authentication,
            &|| {
                ownership.current_for(&self.accounts, &self.account_id, self.attachment_generation)
                    && scoped()
                    && self.deletion_send_still_allowed(claim).unwrap_or(false)
                    && scoped()
                    && ownership.current_for(
                        &self.accounts,
                        &self.account_id,
                        self.attachment_generation,
                    )
                    && !self.cancelled.load(Ordering::SeqCst)
            },
        ))
    }

    fn read_back_deletion(
        &self,
        _claim: &RemoteDeletionClaim,
    ) -> Result<RemoteDeletionReadBack, RemoteMutationTransportError> {
        Err(RemoteMutationTransportError::Cancelled)
    }

    fn read_back_deletion_guarded(
        &self,
        claim: &RemoteDeletionClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteDeletionReadBack, RemoteMutationTransportError> {
        let scoped = || {
            ownership.event_scope().is_none_or(|scope| {
                scope.matches_delete(claim) && self.database.scoped_event_status(scope).is_ok()
            })
        };
        if !scoped() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        if !ownership.current_for(&self.accounts, &self.account_id, self.attachment_generation) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let request = self
            .policy
            .prepare_claimed_deletion_read_back(&self.entry_url, claim, MUTATION_TIMEOUT_MS)
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let authentication = self.authentication(&claim.account_id)?;
        if !self
            .accounts
            .is_current_attachment(&self.account_id, self.attachment_generation)
        {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        request.execute_guarded(self.executor.as_ref(), &authentication, &|| {
            ownership.current_for(&self.accounts, &self.account_id, self.attachment_generation)
                && scoped()
                && self.deletion_read_still_allowed(claim).unwrap_or(false)
                && scoped()
                && ownership.current_for(
                    &self.accounts,
                    &self.account_id,
                    self.attachment_generation,
                )
                && !self.cancelled.load(Ordering::SeqCst)
        })
    }

    fn cancel_deletion_in_flight(&self, account_id: &str) -> bool {
        self.cancel_in_flight(account_id)
    }
}

impl std::fmt::Debug for ProductionTitleMutationTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProductionTitleMutationTransport(REDACTED)")
    }
}

/// Account-keyed owner for attachment-generation-bound writer sessions.
/// Construction and registration do not read credentials or send requests.
/// It remains deliberately absent from AppState until lifecycle and conflict
/// owners are composed around it.
#[allow(dead_code)]
pub(crate) struct ProductionMutationSessions {
    accounts: Arc<AccountSourceRegistry>,
    database: Database,
    credentials: CredentialService,
    clock: Arc<dyn Clock>,
    sessions: Mutex<SessionState>,
    creation_owner: Arc<()>,
    creation_closed: Arc<AtomicBool>,
}

/// Unforgeable outside this adapter: possession proves that registration
/// evidence came from the live production session owner rather than a caller.
pub(crate) struct CompleteProductionMutationTransportRegistration(Arc<AtomicBool>);
/// Only this actual session owner can mint a consuming CREATE envelope.
pub(crate) struct CompleteCreationDispatchAuthorization(());

/// Independent v2 marker. Its constructor is adapter-private.
pub(crate) struct CompleteCreationV2DispatchAuthorization(());

/// Separate private minting marker for inert CREATE session metadata only.
pub(crate) struct CompleteCreationSessionRegistration(
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    std::sync::Weak<()>,
);
impl CompleteCreationSessionRegistration {
    pub(crate) fn revocation_state(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
    pub(crate) fn closure_state(&self) -> Arc<AtomicBool> {
        self.1.clone()
    }
    pub(crate) fn owner_liveness(&self) -> std::sync::Weak<()> {
        self.2.clone()
    }
}

impl CompleteProductionMutationTransportRegistration {
    pub(crate) fn revocation_state(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}

#[derive(Default)]
struct SessionState {
    stopped: bool,
    entries: HashMap<String, Arc<ProductionTitleMutationTransport>>,
}

#[allow(dead_code)]
impl ProductionMutationSessions {
    pub(crate) fn matches_captured_owners(
        &self,
        accounts: &Arc<AccountSourceRegistry>,
        actor: &std::sync::Weak<()>,
        clock: &Arc<dyn Clock>,
    ) -> bool {
        Arc::ptr_eq(&self.accounts, accounts)
            && std::sync::Weak::ptr_eq(&self.database.actor_provenance(), actor)
            && Arc::ptr_eq(&self.clock, clock)
            && !self.creation_closed.load(Ordering::SeqCst)
            && self.sessions.lock().is_ok_and(|state| !state.stopped)
    }
    pub(crate) fn compose_execution_closed_under_gate(
        self: &Arc<Self>,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
        creation_surface: &crate::application::remote_creation_facade::RemoteCreationApplication,
        conflict_surface: &crate::application::remote_mutation_facade::RemoteMutationApplication,
        account: &str,
    ) -> crate::application::production_crud_composition::ClosedCompositionResult {
        let attachments = gate.attachments();
        let owners =
            crate::application::production_creation_owner::CreationReservationComposer::from_owners(
                attachments,
                creation_surface,
                account,
            )?;
        let generation = owners.generation();
        let conflicts = conflict_surface.scoped_conflict_surface(gate, account)?;
        if !conflicts.verify_for(&self.accounts, account, generation) {
            return Err(());
        }
        // Every typed service is private-minted from this exact installed owner.
        let services =
            self.versioned_services_under_gate(gate, creation_surface, conflicts.clone(), account)?;
        let runtime = crate::application::remote_mutation_runtime::RemoteMutationRuntime::with_versioned_services_closed(
            self.accounts.clone(),
            crate::application::remote_mutation::RemoteMutationService::new(self.database.clone(), self.clock.clone(), self.clone()),
            crate::application::remote_deletion::RemoteDeletionService::new(self.database.clone(), self.clock.clone(), self.clone()),
            vec![services.clone()],
        ).map_err(|_| ())?;
        crate::application::production_crud_composition::ExecutionClosedProductionMutationComposition::from_versioned_parts(
            gate, account.to_owned(), generation, self.accounts.clone(), services, runtime, conflicts,
        )
    }

    pub(crate) fn creation_current(
        &self,
        owners: &crate::application::production_creation_owner::CreationReservationComposer,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> bool {
        self.creation_session_evidence(owners.account())
            .is_ok_and(|e| owners.accepts_session(&e))
            && request.current_for(owners.registry(), owners.account(), owners.generation())
    }

    pub(crate) fn create_once_guarded(
        &self,
        owners: &crate::application::production_creation_owner::CreationReservationComposer,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
        claim: &crate::domain::remote_creation::RemoteCreationClaim,
        attempt: crate::domain::remote_creation::RemoteCreationAttemptEvidence,
    ) -> Result<(), RemoteMutationTransportError> {
        let gate = |evidence: &crate::domain::remote_creation::RemoteCreationAttemptEvidence| {
            claim.account_id == owners.account()
                && self.creation_current(owners, request)
                && self
                    .database
                    .remote_creation_send_allowed(claim.clone(), evidence)
                    .unwrap_or(false)
                && self.creation_current(owners, request)
        };
        if !gate(&attempt) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let session = self.session(owners.account())?;
        let registration = self.creation_session_evidence(owners.account())?;
        if !owners.accepts_session(&registration) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let shaped = crate::adapters::caldav_transport::ConditionalCreateRequest::from_attempt(
            &session.policy,
            &session.entry_url,
            claim,
            attempt,
            registration,
            &CompleteCreationDispatchAuthorization(()),
        )
        .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let authentication = session.authentication(owners.account())?;
        // The original evidence is consumed into the private envelope. Executor
        // repeats this real-owner and actor gate immediately before socket send.
        session
            .executor
            .execute_creation_guarded(shaped, &authentication, &|attempt, session| {
                owners.accepts_session(session) && gate(attempt)
            })
            .map(|_| ())
            .map_err(|e| match e {
                crate::adapters::caldav_transport::DavExecutionError::Cancelled => {
                    RemoteMutationTransportError::Cancelled
                }
                _ => RemoteMutationTransportError::Transient,
            })
    }

    pub(crate) fn read_creation_guarded(
        &self,
        owners: &crate::application::production_creation_owner::CreationReservationComposer,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
        claim: &crate::domain::remote_creation::RemoteCreationClaim,
    ) -> Result<crate::domain::remote_creation::RemoteCreationReadBack, RemoteMutationTransportError>
    {
        let gate = || {
            claim.account_id == owners.account()
                && self.creation_current(owners, request)
                && self
                    .database
                    .remote_creation_read_allowed(claim.clone())
                    .unwrap_or(false)
                && self.creation_current(owners, request)
        };
        if !gate() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let session = self.session(owners.account())?;
        let authentication = session.authentication(owners.account())?;
        crate::adapters::caldav_transport::read_creation_exact_guarded(
            &session.policy,
            &session.entry_url,
            claim,
            &session.executor,
            &authentication,
            &gate,
        )
    }

    #[cfg(test)]
    pub(crate) fn configure_creation_fixture(
        &self,
        account: &str,
        executor: Arc<HttpDavExecutor>,
        credentials: CredentialService,
    ) {
        let mut entries = self.sessions.lock().unwrap();
        let session = Arc::get_mut(entries.entries.get_mut(account).unwrap())
            .expect("no active transport handles");
        session.executor = executor;
        session.credentials = credentials;
    }
    #[cfg(test)]
    pub(crate) fn poison_session_registry_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.sessions.lock().unwrap();
            panic!("poison production session registry for composition test");
        }));
    }
    pub(crate) fn new(
        accounts: Arc<AccountSourceRegistry>,
        database: Database,
        credentials: CredentialService,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            accounts,
            database,
            credentials,
            clock,
            sessions: Mutex::new(SessionState::default()),
            creation_owner: Arc::new(()),
            creation_closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn register_current(
        &self,
        account_id: &str,
    ) -> Result<u64, RemoteMutationTransportError> {
        let generation = self
            .accounts
            .attachment_generation(account_id)
            .ok_or(RemoteMutationTransportError::Cancelled)?;
        self.register_attachment(account_id, generation)
    }

    pub(crate) fn register_attachment(
        &self,
        account_id: &str,
        generation: u64,
    ) -> Result<u64, RemoteMutationTransportError> {
        self.register_attachment_cancellation(account_id, generation, None)
    }

    #[cfg(test)]
    pub(crate) fn register_probe_fixture(
        &self,
        account_id: &str,
    ) -> Result<u64, RemoteMutationTransportError> {
        self.register_probe_harness(
            account_id,
            crate::adapters::caldav_transport::DavCancellation::new(),
        )
    }

    pub(in crate::adapters) fn register_probe_harness(
        &self,
        account_id: &str,
        cancellation: crate::adapters::caldav_transport::DavCancellation,
    ) -> Result<u64, RemoteMutationTransportError> {
        // Diagnostic-only fixed owner. The Probe harness never runs product
        // discovery, so it keeps the historical observed shard instead of
        // claiming a persisted account binding it does not have.
        const PROBE_ENTRY: &str = "https://p235-caldav.icloud.com.cn/";
        const PROBE_SHARD: &str = "p235-caldav.icloud.com.cn";
        let generation = self
            .accounts
            .attachment_generation(account_id)
            .ok_or(RemoteMutationTransportError::Cancelled)?;
        let profile = self
            .accounts
            .profile(account_id)
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        if !profile.enabled
            || !profile.credential_configured
            || profile.region_profile != ICLOUD_CHINA_REGION
            || !self.accounts.is_current_attachment(account_id, generation)
        {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let policy = DavTransportPolicy::new(std::collections::BTreeSet::from([
            "caldav.icloud.com".to_owned(),
            PROBE_SHARD.to_owned(),
        ]))
        .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let executor =
            HttpDavExecutor::probe_harness(HttpDavConfig::default(), cancellation.clone())
                .map_err(|_| RemoteMutationTransportError::Transient)?;
        let mut transport = ProductionTitleMutationTransport::new(
            account_id.to_owned(),
            generation,
            Url::parse(PROBE_ENTRY).map_err(|_| RemoteMutationTransportError::UnsafeData)?,
            self.accounts.clone(),
            self.database.clone(),
            self.credentials.clone(),
            self.clock.clone(),
            policy,
            Arc::new(executor),
        )?;
        transport.cancelled = Arc::new(AtomicBool::new(false));
        let transport = Arc::new(transport);
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        if sessions.stopped || !self.accounts.is_current_attachment(account_id, generation) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        if let Some(previous) = sessions.entries.insert(account_id.to_owned(), transport) {
            previous.cancel_in_flight(account_id);
        }
        Ok(generation)
    }

    fn register_attachment_cancellation(
        &self,
        account_id: &str,
        generation: u64,
        cancellation: Option<crate::adapters::caldav_transport::DavCancellation>,
    ) -> Result<u64, RemoteMutationTransportError> {
        let profile = self
            .accounts
            .profile(account_id)
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        if !profile.enabled
            || !profile.credential_configured
            || profile.region_profile != ICLOUD_CHINA_REGION
        {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        if !self.accounts.is_current_attachment(account_id, generation) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let (binding, binding_cancelled) = self
            .database
            .icloud_binding_lease(account_id.to_owned())
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?
            .ok_or(RemoteMutationTransportError::UnsafeData)?;
        let routes = self
            .database
            .available_calendar_ids_by_url(account_id.to_owned())
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let policy = DavTransportPolicy::icloud_bound(&binding, routes.into_keys())
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let executor = match cancellation {
            Some(token) => HttpDavExecutor::probe_harness(HttpDavConfig::default(), token),
            None => HttpDavExecutor::new(HttpDavConfig::default()),
        }
        .map_err(|_| RemoteMutationTransportError::Transient)?;
        let mut transport = ProductionTitleMutationTransport::new(
            account_id.to_owned(),
            generation,
            Url::parse(&binding.home_url).map_err(|_| RemoteMutationTransportError::UnsafeData)?,
            self.accounts.clone(),
            self.database.clone(),
            self.credentials.clone(),
            self.clock.clone(),
            policy,
            Arc::new(executor),
        )?;
        transport.cancelled = binding_cancelled;
        let transport = Arc::new(transport);
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        if sessions.stopped || !self.accounts.is_current_attachment(account_id, generation) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        if let Some(previous) = sessions.entries.insert(account_id.to_owned(), transport) {
            previous.cancel_in_flight(account_id);
        }
        Ok(generation)
    }

    pub(crate) fn reviewed_title_registration(
        &self,
        account_id: &str,
    ) -> Result<ReviewedMutationTransportRegistration, RemoteMutationTransportError> {
        let session = self.session(account_id)?;
        if session.cancelled.load(Ordering::SeqCst)
            || !self
                .accounts
                .is_current_attachment(account_id, session.attachment_generation)
        {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        Ok(ReviewedMutationTransportRegistration::reviewed(
            account_id,
            session.attachment_generation,
            MutationTransportOperation::ConditionalTitleUpdate,
            &CompleteProductionMutationTransportRegistration(session.cancelled.clone()),
        ))
    }

    pub(crate) fn creation_session_evidence(
        &self,
        account: &str,
    ) -> Result<
        crate::application::production_creation_owner::CreationSessionEvidence,
        RemoteMutationTransportError,
    > {
        let session = self.session(account)?;
        let (registry, actor) = self.accounts.creation_owner_provenance();
        if self.creation_closed.load(Ordering::SeqCst)
            || session.cancelled.load(Ordering::SeqCst)
            || !self
                .accounts
                .is_current_attachment(account, session.attachment_generation)
            || !std::sync::Weak::ptr_eq(&actor, &self.database.actor_provenance())
            || !self.accounts.profile(account).is_ok_and(|p| {
                p.enabled
                    && p.credential_configured
                    && p.region_profile == ICLOUD_CHINA_REGION
                    && p.provider_kind == crate::domain::account::ProviderKind::CalDav
            })
        {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        Ok(
            crate::application::production_creation_owner::CreationSessionEvidence::from_session(
                account.to_owned(),
                session.attachment_generation,
                registry,
                actor,
                &CompleteCreationSessionRegistration(
                    session.cancelled.clone(),
                    self.creation_closed.clone(),
                    Arc::downgrade(&self.creation_owner),
                ),
            ),
        )
    }

    /// Read-only joint check against this owner's exact captured concrete session.
    /// No registration, authentication, executor entry or request is performed.
    pub(crate) fn probe_completion_session_matches(
        &self,
        account: &str,
        route: &str,
        captured: &crate::application::production_creation_owner::CreationSessionEvidence,
    ) -> bool {
        self.creation_session_evidence(account)
            .is_ok_and(|fresh| fresh.same_owner(captured))
            && self.session(account).is_ok_and(|session| {
                !session.cancelled.load(Ordering::SeqCst)
                    && session
                        .policy
                        .probe_collection_matches(&session.entry_url, route)
                    && self
                        .creation_session_evidence(account)
                        .is_ok_and(|fresh| fresh.same_owner(captured))
            })
    }

    pub(crate) fn revoke(&self, account_id: &str) -> bool {
        let mut sessions = match self.sessions.lock() {
            Ok(sessions) => sessions,
            Err(poisoned) => poisoned.into_inner(),
        };
        sessions
            .entries
            .remove(account_id)
            .is_some_and(|session| session.cancel_in_flight(account_id))
    }

    pub(crate) fn shutdown(&self) {
        self.creation_closed.store(true, Ordering::SeqCst);
        let mut sessions = match self.sessions.lock() {
            Ok(sessions) => sessions,
            Err(poisoned) => poisoned.into_inner(),
        };
        sessions.stopped = true;
        for (account_id, session) in sessions.entries.drain() {
            session.cancel_in_flight(&account_id);
        }
    }

    /// Actor-safe ownership snapshot: no profile/SQLite/credential reads.
    pub(crate) fn connected_owner_current(&self, account: &str, route: Option<&str>) -> bool {
        !self.creation_closed.load(Ordering::SeqCst)
            && self.session(account).is_ok_and(|session| {
                !session.cancelled.load(Ordering::SeqCst)
                    && self
                        .accounts
                        .is_current_attachment(account, session.attachment_generation)
                    && route.is_none_or(|route| {
                        session
                            .policy
                            .probe_collection_matches(&session.entry_url, route)
                    })
            })
    }
    fn session(
        &self,
        account_id: &str,
    ) -> Result<Arc<ProductionTitleMutationTransport>, RemoteMutationTransportError> {
        self.sessions
            .lock()
            .map_err(|_| {
                self.creation_closed.store(true, Ordering::SeqCst);
                RemoteMutationTransportError::Cancelled
            })?
            .entries
            .get(account_id)
            .cloned()
            .ok_or(RemoteMutationTransportError::Cancelled)
    }
}

impl crate::application::attachment_runtime::AttachmentMutationSessions
    for ProductionMutationSessions
{
    fn creation_session_evidence(
        &self,
        account: &str,
    ) -> Result<crate::application::production_creation_owner::CreationSessionEvidence, ()> {
        ProductionMutationSessions::creation_session_evidence(self, account).map_err(|_| ())
    }
    fn register(&self, account_id: &str, generation: u64) -> Result<(), ()> {
        self.register_attachment(account_id, generation)
            .map(|_| ())
            .map_err(|_| ())
    }
    fn revoke(&self, account_id: &str) {
        ProductionMutationSessions::revoke(self, account_id);
    }
    fn shutdown(&self) {
        ProductionMutationSessions::shutdown(self);
    }
    fn reviewed_title_registration(
        &self,
        account_id: &str,
    ) -> Result<ReviewedMutationTransportRegistration, ()> {
        ProductionMutationSessions::reviewed_title_registration(self, account_id).map_err(|_| ())
    }
}

#[allow(dead_code)]
pub(crate) struct ProductionCreationV2Transport {
    sessions: std::sync::Weak<ProductionMutationSessions>,
    session: Arc<ProductionTitleMutationTransport>,
    registration: crate::application::production_creation_owner::CreationSessionEvidence,
    conflicts:
        Option<crate::application::production_mutation_conflict::ReviewedMutationConflictSurface>,
}
#[allow(dead_code)]
impl ProductionMutationSessions {
    pub(crate) fn creation_v2_transport(
        self: &Arc<Self>,
        owners: &crate::application::production_creation_owner::CreationReservationComposer,
    ) -> Result<ProductionCreationV2Transport, RemoteMutationTransportError> {
        let registration = self.creation_session_evidence(owners.account())?;
        if !owners.accepts_session(&registration) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        Ok(ProductionCreationV2Transport {
            sessions: Arc::downgrade(self),
            session: self.session(owners.account())?,
            registration,
            conflicts: None,
        })
    }
}
#[allow(dead_code)]
impl ProductionCreationV2Transport {
    pub(crate) fn bind_collection_conflicts(
        &mut self,
        conflicts: crate::application::production_mutation_conflict::ReviewedMutationConflictSurface,
    ) {
        self.conflicts = Some(conflicts);
    }
    pub(crate) fn current(
        &self,
        owners: &crate::application::production_creation_owner::CreationReservationComposer,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> bool {
        let s = &self.session;
        self.conflicts.as_ref().is_none_or(|conflicts| {
            conflicts.verify_for(&s.accounts, &s.account_id, s.attachment_generation)
        }) && owners.account() == s.account_id
            && owners.accepts_session(&self.registration)
            && !s.cancelled.load(Ordering::SeqCst)
            && request.current_for(&s.accounts, &s.account_id, s.attachment_generation)
            && self.sessions.upgrade().is_some_and(|sessions| {
                sessions
                    .session(&s.account_id)
                    .is_ok_and(|current| Arc::ptr_eq(&current, s))
            })
    }
    pub(crate) fn create_once(
        &self,
        owners: &crate::application::production_creation_owner::CreationReservationComposer,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
        claim: &crate::domain::remote_creation::RemoteCreationClaim,
        attempt: crate::domain::remote_creation::RemoteCreationAttemptEvidence,
    ) -> Result<(), RemoteMutationTransportError> {
        let s = &self.session;
        let gate = |evidence: &crate::domain::remote_creation::RemoteCreationAttemptEvidence| {
            claim.codec_version == "utc-v2"
                && claim.account_id == s.account_id
                && request.event_scope().is_none_or(|scope| {
                    scope.matches_create(claim) && s.database.scoped_event_status(scope).is_ok()
                })
                && self.current(owners, request)
                && s.database
                    .remote_creation_send_allowed(claim.clone(), evidence)
                    .unwrap_or(false)
                && request.event_scope().is_none_or(|scope| {
                    scope.matches_create(claim) && s.database.scoped_event_status(scope).is_ok()
                })
                && self.current(owners, request)
        };
        if !gate(&attempt) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let mint = CompleteCreationV2DispatchAuthorization(());
        let envelope = crate::adapters::caldav_transport::ConditionalCreateV2Request::from_attempt(
            &s.policy,
            &s.entry_url,
            claim,
            attempt,
            self.registration.clone(),
            &mint,
        )
        .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let auth = s.authentication(&s.account_id)?;
        s.executor
            .execute_creation_v2_guarded(envelope, &auth, &mint, &|attempt, registration| {
                self.registration.same_owner(registration)
                    && owners.accepts_session(registration)
                    && gate(attempt)
            })
            .map(|_| ())
            .map_err(|error| match error {
                crate::adapters::caldav_transport::DavExecutionError::Cancelled => {
                    RemoteMutationTransportError::Cancelled
                }
                _ => RemoteMutationTransportError::Transient,
            })
    }
    pub(crate) fn get(
        &self,
        owners: &crate::application::production_creation_owner::CreationReservationComposer,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
        claim: &crate::domain::remote_creation::RemoteCreationClaim,
    ) -> Result<crate::domain::remote_creation::RemoteCreationReadBack, RemoteMutationTransportError>
    {
        let s = &self.session;
        let gate = || {
            claim.codec_version == "utc-v2"
                && claim.account_id == s.account_id
                && request.event_scope().is_none_or(|scope| {
                    scope.matches_create(claim) && s.database.scoped_event_status(scope).is_ok()
                })
                && self.current(owners, request)
                && s.database
                    .remote_creation_read_allowed(claim.clone())
                    .unwrap_or(false)
                && request.event_scope().is_none_or(|scope| {
                    scope.matches_create(claim) && s.database.scoped_event_status(scope).is_ok()
                })
                && self.current(owners, request)
        };
        if !gate() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let auth = s.authentication(&s.account_id)?;
        let result = crate::adapters::caldav_transport::read_creation_v2_exact_guarded(
            &s.policy,
            &s.entry_url,
            claim,
            &s.executor,
            &auth,
            &CompleteCreationV2DispatchAuthorization(()),
            &gate,
        )?;
        if !gate() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        Ok(result)
    }
}

/// Sealed collection minted only by the same concrete session owner under the
/// attachment request gate. This is internal execution-closed graph ownership.
pub(crate) struct VersionedMutationServices {
    pub(crate) creation:
        Arc<crate::application::remote_creation_dispatch::RemoteCreationDispatchService>,
    pub(crate) creation_v2:
        Arc<crate::application::remote_creation_v2_dispatch::RemoteCreationV2DispatchService>,
    pub(crate) fields: Arc<crate::application::remote_fields::RemoteFieldsService>,
    pub(crate) event: Arc<crate::application::remote_event::RemoteEventService>,
    field_transport: Arc<OwnedFieldsTransport>,
}
impl VersionedMutationServices {
    pub(crate) fn account(&self) -> &str {
        self.creation.account()
    }
    pub(crate) fn current(&self) -> bool {
        self.belongs_to(self.field_transport.owners.registry())
    }
    pub(crate) fn belongs_to(&self, accounts: &AccountSourceRegistry) -> bool {
        self.creation.belongs_to(accounts)
            && self.creation_v2.belongs_to(accounts)
            && self.event.belongs_to(accounts)
            && self.event.shares_session(&self.field_transport.transport)
            && self.field_transport.owners.current()
            && self.field_transport.conflicts.verify_for(
                accounts,
                self.account(),
                self.field_transport.owners.generation(),
            )
            && self.field_transport.session_current()
    }
    pub(crate) fn matches_gate(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
    ) -> bool {
        self.creation.matches_gate(gate)
            && self.creation_v2.matches_gate(gate)
            && self.field_transport.owners.matches_gate(gate)
            && self.event.matches_gate(gate)
    }
    pub(crate) fn current_under_gate(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
    ) -> bool {
        self.matches_gate(gate)
            && self.creation.current_under_gate(gate)
            && self.creation_v2.current_under_gate(gate)
            && self.event.current_under_gate(gate)
            && self.field_transport.owners.current_under_gate(gate)
            && self.field_transport.session_current()
    }
    pub(crate) fn cancel_fields(&self, account: &str) -> bool {
        account == self.account()
            && self
                .field_transport
                .transport
                .session
                .cancel_in_flight(account)
    }
}
struct OwnedFieldsTransport {
    transport: Arc<ProductionFieldsTransport>,
    owners: crate::application::production_creation_owner::CreationReservationComposer,
    conflicts: crate::application::production_mutation_conflict::ReviewedMutationConflictSurface,
}
impl OwnedFieldsTransport {
    fn session_current(&self) -> bool {
        self.transport.owners.upgrade().is_some_and(|sessions| {
            sessions
                .session(self.owners.account())
                .is_ok_and(|session| Arc::ptr_eq(&session, &self.transport.session))
        }) && !self.transport.session.cancelled.load(Ordering::SeqCst)
    }
    fn owner_current(&self) -> bool {
        self.owners.current()
            && self.session_current()
            && self.conflicts.verify_for(
                self.owners.registry(),
                self.owners.account(),
                self.owners.generation(),
            )
    }
}
impl crate::ports::remote_mutation::RemoteFieldsTransport for OwnedFieldsTransport {
    fn account_id(&self) -> &str {
        self.owners.account()
    }
    fn current(&self, ownership: &MutationTransportOwnership<'_>) -> bool {
        self.owner_current() && self.transport.current(ownership)
    }
    fn put(
        &self,
        claim: &crate::domain::remote_mutation::RemoteFieldsMutationClaim,
        snapshot: crate::domain::remote_mutation::RemoteFieldsSendSnapshot,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<(), RemoteMutationTransportError> {
        let owner = || self.owner_current();
        self.transport
            .put(claim, snapshot, &ownership.with_session_check(&owner))
    }
    fn get(
        &self,
        claim: &crate::domain::remote_mutation::RemoteFieldsMutationClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        let owner = || self.owner_current();
        self.transport
            .get(claim, &ownership.with_session_check(&owner))
    }
}
impl ProductionMutationSessions {
    pub(crate) fn versioned_services_under_gate(
        self: &Arc<Self>,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
        creation_surface: &crate::application::remote_creation_facade::RemoteCreationApplication,
        conflicts: crate::application::production_mutation_conflict::ReviewedMutationConflictSurface,
        account: &str,
    ) -> Result<Arc<VersionedMutationServices>, ()> {
        use crate::application::production_creation_owner::CreationReservationComposer;
        if !conflicts.matches_gate(gate) {
            return Err(());
        }
        let creation = Arc::new(
            crate::application::remote_creation_dispatch::RemoteCreationDispatchService::new(
                self.database.clone(),
                CreationReservationComposer::from_owners(
                    gate.attachments(),
                    creation_surface,
                    account,
                )?,
                self.clone(),
            )?,
        );
        let creation_v2 = Arc::new(
            crate::application::remote_creation_v2_dispatch::RemoteCreationV2DispatchService::new_for_collection(
                self.database.clone(),
                CreationReservationComposer::from_owners(
                    gate.attachments(),
                    creation_surface,
                    account,
                )?,
                self.clone(),
                conflicts.clone(),
            )?,
        );
        let event =
            self.event_service_under_gate(gate, creation_surface, conflicts.clone(), account)?;
        let field_transport = Arc::new(OwnedFieldsTransport {
            transport: self.fields_transport(account).map_err(|_| ())?,
            owners: CreationReservationComposer::from_owners(
                gate.attachments(),
                creation_surface,
                account,
            )?,
            conflicts,
        });
        let fields = Arc::new(crate::application::remote_fields::RemoteFieldsService::new(
            self.database.clone(),
            field_transport.clone(),
        ));
        let services = Arc::new(VersionedMutationServices {
            creation,
            creation_v2,
            fields,
            event,
            field_transport,
        });
        if !services.belongs_to(&self.accounts) || !services.current_under_gate(gate) {
            return Err(());
        }
        Ok(services)
    }
}

/// Captures one concrete session across PUT, recovery GET and publication.
/// Registered in the execution-closed worker through its scoped owner wrapper.
#[allow(dead_code)]
pub(crate) struct ProductionFieldsTransport {
    owners: std::sync::Weak<ProductionMutationSessions>,
    session: Arc<ProductionTitleMutationTransport>,
}
/// Adapter-private mint evidence; an actor snapshot alone cannot open an exchange.
pub(crate) struct CompleteFieldsTransportAuthorization(());
#[allow(dead_code)]
impl ProductionMutationSessions {
    pub(crate) fn fields_transport(
        self: &Arc<Self>,
        account: &str,
    ) -> Result<Arc<ProductionFieldsTransport>, RemoteMutationTransportError> {
        Ok(Arc::new(ProductionFieldsTransport {
            owners: Arc::downgrade(self),
            session: self.session(account)?,
        }))
    }
}
impl crate::ports::remote_mutation::RemoteFieldsTransport for ProductionFieldsTransport {
    fn account_id(&self) -> &str {
        &self.session.account_id
    }
    fn current(&self, ownership: &MutationTransportOwnership<'_>) -> bool {
        let s = &self.session;
        !s.cancelled.load(Ordering::SeqCst)
            && ownership.current_for(&s.accounts, &s.account_id, s.attachment_generation)
            && self.owners.upgrade().is_some_and(|owners| {
                owners
                    .session(&s.account_id)
                    .is_ok_and(|current| Arc::ptr_eq(&current, s))
            })
    }
    fn put(
        &self,
        claim: &crate::domain::remote_mutation::RemoteFieldsMutationClaim,
        snapshot: crate::domain::remote_mutation::RemoteFieldsSendSnapshot,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<(), RemoteMutationTransportError> {
        if !self.current(ownership) || claim.account_id != self.session.account_id {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let s = &self.session;
        let current = s
            .database
            .remote_fields_send_snapshot(claim)
            .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        if current.operation_id != snapshot.operation_id
            || current.account_id != snapshot.account_id
            || current.calendar_id != snapshot.calendar_id
            || current.resource_id != snapshot.resource_id
            || current.collection_href != snapshot.collection_href
            || current.exact_href != snapshot.exact_href
            || current.base_etag != snapshot.base_etag
            || current.expected_raw_ics != snapshot.expected_raw_ics
        {
            return Err(RemoteMutationTransportError::UnsafeData);
        }
        let request =
            crate::adapters::caldav_transport::ConditionalFieldsUpdateRequest::from_snapshot(
                &s.policy,
                &s.entry_url,
                snapshot,
                &CompleteFieldsTransportAuthorization(()),
            )
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let auth = s.authentication(&claim.account_id)?;
        classify_conditional_update_result(s.executor.execute_fields_guarded(
            request,
            &auth,
            &CompleteFieldsTransportAuthorization(()),
            &|| {
                self.current(ownership)
                    && s.database.remote_fields_send_snapshot(claim).is_ok()
                    && self.current(ownership)
            },
        ))
    }
    fn get(
        &self,
        claim: &crate::domain::remote_mutation::RemoteFieldsMutationClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        let s = &self.session;
        if !self.current(ownership)
            || claim.account_id != s.account_id
            || !s
                .database
                .remote_fields_read_allowed(claim)
                .unwrap_or(false)
        {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let auth = s.authentication(&claim.account_id)?;
        let remote = crate::adapters::caldav_transport::read_fields_exact_guarded(
            &s.policy,
            &s.entry_url,
            claim,
            &s.executor,
            &auth,
            &CompleteFieldsTransportAuthorization(()),
            &|| {
                self.current(ownership)
                    && s.database
                        .remote_fields_read_allowed(claim)
                        .unwrap_or(false)
                    && self.current(ownership)
            },
        )?;
        if !self.current(ownership) {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        Ok(remote)
    }
}

impl RemoteMutationTransport for ProductionMutationSessions {
    fn conditional_update(
        &self,
        _claim: &RemoteMutationClaim,
        _patched_ics: &[u8],
    ) -> Result<(), RemoteMutationTransportError> {
        Err(RemoteMutationTransportError::Cancelled)
    }

    fn conditional_update_guarded(
        &self,
        claim: &RemoteMutationClaim,
        patched_ics: &[u8],
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<(), RemoteMutationTransportError> {
        let session = self.session(&claim.account_id)?;
        let exact_session = || {
            self.session(&claim.account_id)
                .is_ok_and(|current| Arc::ptr_eq(&current, &session))
        };
        let captured = ownership.with_session_check(&exact_session);
        session.conditional_update_guarded(claim, patched_ics, &captured)
    }

    fn read_back(
        &self,
        _claim: &RemoteMutationClaim,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        Err(RemoteMutationTransportError::Cancelled)
    }

    fn read_back_guarded(
        &self,
        claim: &RemoteMutationClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        let session = self.session(&claim.account_id)?;
        let exact_session = || {
            self.session(&claim.account_id)
                .is_ok_and(|current| Arc::ptr_eq(&current, &session))
        };
        let captured = ownership.with_session_check(&exact_session);
        session.read_back_guarded(claim, &captured)
    }

    fn cancel_in_flight(&self, account_id: &str) -> bool {
        let sessions = match self.sessions.lock() {
            Ok(sessions) => sessions,
            Err(poisoned) => poisoned.into_inner(),
        };
        sessions
            .entries
            .get(account_id)
            .cloned()
            .is_some_and(|session| session.cancel_in_flight(account_id))
    }
}

impl RemoteDeletionTransport for ProductionMutationSessions {
    fn conditional_delete(
        &self,
        _claim: &RemoteDeletionClaim,
    ) -> Result<(), RemoteMutationTransportError> {
        Err(RemoteMutationTransportError::Cancelled)
    }

    fn conditional_delete_guarded(
        &self,
        claim: &RemoteDeletionClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<(), RemoteMutationTransportError> {
        let session = self.session(&claim.account_id)?;
        let exact_session = || {
            self.session(&claim.account_id)
                .is_ok_and(|current| Arc::ptr_eq(&current, &session))
        };
        let captured = ownership.with_session_check(&exact_session);
        session.conditional_delete_guarded(claim, &captured)
    }

    fn read_back_deletion(
        &self,
        _claim: &RemoteDeletionClaim,
    ) -> Result<RemoteDeletionReadBack, RemoteMutationTransportError> {
        Err(RemoteMutationTransportError::Cancelled)
    }

    fn read_back_deletion_guarded(
        &self,
        claim: &RemoteDeletionClaim,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteDeletionReadBack, RemoteMutationTransportError> {
        let session = self.session(&claim.account_id)?;
        let exact_session = || {
            self.session(&claim.account_id)
                .is_ok_and(|current| Arc::ptr_eq(&current, &session))
        };
        let captured = ownership.with_session_check(&exact_session);
        session.read_back_deletion_guarded(claim, &captured)
    }

    fn cancel_deletion_in_flight(&self, account_id: &str) -> bool {
        self.cancel_in_flight(account_id)
    }
}

impl std::fmt::Debug for ProductionMutationSessions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProductionMutationSessions(REDACTED)")
    }
}

mod remote_event_transport;
pub(crate) use remote_event_transport::{
    CompleteEventTransportAuthorization, ProductionEventTransport,
};
