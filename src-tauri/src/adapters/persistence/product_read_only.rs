//! Exclusive read-only configuration selection; no calendar/transport authority.
use super::db_owner::{
    DatabaseOwner, LockFallbackObservation, OwnerDetail, OwnerError, OwnerFailure, OwnerStep,
    ParentAccessRequests,
};
use crate::{
    adapters::caldav_transport::DavCancellation,
    application::credentials::CredentialService,
    ports::credentials::{CredentialRecord, CredentialRef},
};
use rusqlite::{Connection, OpenFlags};
use std::{path::Path, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationAvailability {
    Available,
    OwnerUnavailable,
    DatabaseMissing,
    SidecarPresent,
    SchemaMismatch,
    NoEligibleProfile,
    MultipleEligibleProfiles,
    InvalidProfileBinding,
    ProfileChanged,
    OtherUnavailable,
    Cancelled,
}
#[derive(Clone, Copy)]
pub(in crate::adapters) struct ConfigurationCheck {
    pub(in crate::adapters) availability: ConfigurationAvailability,
    pub(in crate::adapters) owner_detail: Option<OwnerDetail>,
    pub(in crate::adapters) owner_step: Option<OwnerStep>,
    pub(in crate::adapters) fallback_detail: Option<LockFallbackObservation>,
    pub(in crate::adapters) parent_access_requests: Option<ParentAccessRequests>,
}
impl From<ConfigurationAvailability> for ConfigurationCheck {
    fn from(availability: ConfigurationAvailability) -> Self {
        Self {
            availability,
            owner_detail: None,
            owner_step: None,
            fallback_detail: None,
            parent_access_requests: None,
        }
    }
}
fn configuration_owner_failure(failure: ProductReadOnlyFailure) -> ConfigurationCheck {
    match failure {
        ProductReadOnlyFailure::Ownership(owner) => ConfigurationCheck {
            availability: ConfigurationAvailability::OwnerUnavailable,
            owner_detail: Some(owner.detail()),
            owner_step: owner.diagnostic_step(),
            fallback_detail: owner.diagnostic_fallback(),
            parent_access_requests: owner.diagnostic_parent_access(),
        },
        other => configuration_open_failure(other).into(),
    }
}
fn configuration_open_failure(failure: ProductReadOnlyFailure) -> ConfigurationAvailability {
    match failure {
        ProductReadOnlyFailure::Ownership(_) => ConfigurationAvailability::OwnerUnavailable,
        ProductReadOnlyFailure::RecoveryRequired => ConfigurationAvailability::SidecarPresent,
        ProductReadOnlyFailure::SchemaMismatch => ConfigurationAvailability::SchemaMismatch,
        ProductReadOnlyFailure::Unavailable => ConfigurationAvailability::OtherUnavailable,
    }
}
fn configuration_selection_failure(failure: SelectionFailure) -> ConfigurationAvailability {
    match failure {
        SelectionFailure::NoMatch => ConfigurationAvailability::NoEligibleProfile,
        SelectionFailure::Ambiguous => ConfigurationAvailability::MultipleEligibleProfiles,
        SelectionFailure::InvalidMetadata | SelectionFailure::Bounds => {
            ConfigurationAvailability::InvalidProfileBinding
        }
        SelectionFailure::Unavailable => ConfigurationAvailability::OtherUnavailable,
    }
}
pub(in crate::adapters) fn check_configured_probe_profile(
    cancellation: &DavCancellation,
) -> ConfigurationCheck {
    configuration_check_with(cancellation, || {
        ProductReadOnlyPermit::acquire_standard_for_configuration(cancellation)
    })
}
fn configuration_check_with(
    cancellation: &DavCancellation,
    acquire: impl FnOnce() -> Result<ProductReadOnlyPermit, ProductReadOnlyFailure>,
) -> ConfigurationCheck {
    if cancellation.is_cancelled() {
        return ConfigurationAvailability::Cancelled.into();
    }
    let selection = acquire()
        .and_then(ProductReadOnlyPermit::open)
        .map_err(configuration_owner_failure)
        .and_then(|database| {
            configured_selection(database).map_err(|failure| {
                ConfigurationCheck::from(configuration_selection_failure(failure))
            })
        });
    if cancellation.is_cancelled() {
        return ConfigurationAvailability::Cancelled.into();
    }
    match selection {
        Ok(selection) => selection.consume_preview(cancellation).into(),
        Err(failure) => failure,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::adapters) enum SelectionFailure {
    Unavailable,
    NoMatch,
    Ambiguous,
    InvalidMetadata,
    Bounds,
}

#[derive(PartialEq, Eq)]
struct ConfiguredRow {
    account: String,
    reference: String,
    updated: i64,
}

// Non-cloneable/non-formatting. Owns the no-writer main-file pin through the
// final account/schema/uniqueness recheck and the single exact credential read.
pub(in crate::adapters) struct ConfiguredProfileSelection {
    database: GuardedProductReadOnly,
    row: ConfiguredRow,
}
struct FreshCheckFailure {
    diagnostic: ConfigurationAvailability,
    execution: crate::adapters::probe_live_harness::Failure,
}

pub(in crate::adapters) fn select_configured_probe_profile()
-> Result<ConfiguredProfileSelection, SelectionFailure> {
    let database = ProductReadOnlyPermit::acquire_standard()
        .and_then(ProductReadOnlyPermit::open)
        .map_err(|_| SelectionFailure::Unavailable)?;
    configured_selection(database)
}

fn configured_selection(
    database: GuardedProductReadOnly,
) -> Result<ConfiguredProfileSelection, SelectionFailure> {
    let row = configured_row(&database)?;
    Ok(ConfiguredProfileSelection { database, row })
}

fn configured_row(database: &GuardedProductReadOnly) -> Result<ConfiguredRow, SelectionFailure> {
    let c = database
        .connection
        .as_ref()
        .ok_or(SelectionFailure::Unavailable)?;
    let mut statement = c.prepare("SELECT CASE WHEN length(CAST(account_id AS BLOB))=36 THEN account_id END, CASE WHEN length(CAST(credential_ref AS BLOB))=58 THEN credential_ref END, updated_at_ms FROM accounts WHERE provider_kind='caldav' AND region_profile='icloud-cn' AND enabled=1 AND credential_configured=1 AND remote_editing_enabled=1 AND removed_at_ms IS NULL LIMIT 2").map_err(|_| SelectionFailure::Unavailable)?;
    let mut rows = statement
        .query([])
        .map_err(|_| SelectionFailure::Unavailable)?;
    let Some(row) = rows.next().map_err(|_| SelectionFailure::Unavailable)? else {
        return Err(SelectionFailure::NoMatch);
    };
    let account: Option<String> = row.get(0).map_err(|_| SelectionFailure::InvalidMetadata)?;
    let reference: Option<String> = row.get(1).map_err(|_| SelectionFailure::InvalidMetadata)?;
    let updated: i64 = row.get(2).map_err(|_| SelectionFailure::InvalidMetadata)?;
    let account = account.ok_or(SelectionFailure::Bounds)?;
    let reference = reference.ok_or(SelectionFailure::Bounds)?;
    if !uuid::Uuid::parse_str(&account).is_ok_and(|id| !id.is_nil() && id.to_string() == account)
        || reference != format!("calendar-todo/account/{account}")
        || updated < 1
    {
        return Err(SelectionFailure::InvalidMetadata);
    }
    if rows
        .next()
        .map_err(|_| SelectionFailure::Unavailable)?
        .is_some()
    {
        return Err(SelectionFailure::Ambiguous);
    }
    Ok(ConfiguredRow {
        account,
        reference,
        updated,
    })
}

impl ConfiguredProfileSelection {
    fn fresh_transaction(&self) -> Result<rusqlite::Transaction<'_>, FreshCheckFailure> {
        use crate::adapters::probe_live_harness::Failure;
        reject_recovery_files(self.database._owner.path()).map_err(|failure| {
            FreshCheckFailure {
                diagnostic: configuration_open_failure(failure),
                execution: Failure::SavedReferenceUnavailable,
            }
        })?;
        let unavailable = || FreshCheckFailure {
            diagnostic: ConfigurationAvailability::OtherUnavailable,
            execution: Failure::SavedReferenceUnavailable,
        };
        let c = self.database.connection.as_ref().ok_or_else(unavailable)?;
        let tx = c.unchecked_transaction().map_err(|_| unavailable())?;
        let schema = c
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .map_err(|_| unavailable())?;
        if schema != crate::adapters::persistence::SCHEMA_VERSION {
            return Err(FreshCheckFailure {
                diagnostic: ConfigurationAvailability::SchemaMismatch,
                execution: Failure::SavedReferenceInvalidMetadata,
            });
        }
        let row = configured_row(&self.database).map_err(|failure| FreshCheckFailure {
            diagnostic: configuration_selection_failure(failure),
            execution: Failure::SavedReferenceInvalidMetadata,
        })?;
        if row != self.row {
            return Err(FreshCheckFailure {
                diagnostic: ConfigurationAvailability::ProfileChanged,
                execution: Failure::SavedReferenceInvalidMetadata,
            });
        }
        Ok(tx)
    }
    fn consume_preview(self, cancellation: &DavCancellation) -> ConfigurationAvailability {
        if cancellation.is_cancelled() {
            return ConfigurationAvailability::Cancelled;
        }
        let tx = match self.fresh_transaction() {
            Ok(tx) => tx,
            Err(failure) => return failure.diagnostic,
        };
        if cancellation.is_cancelled() {
            return ConfigurationAvailability::Cancelled;
        }
        if tx.commit().is_err() {
            return ConfigurationAvailability::OtherUnavailable;
        }
        if cancellation.is_cancelled() {
            ConfigurationAvailability::Cancelled
        } else {
            ConfigurationAvailability::Available
        }
    }
    pub(in crate::adapters) fn consume_read(
        self,
        credentials: &CredentialService,
        cancellation: &DavCancellation,
    ) -> Result<
        (CredentialRef, CredentialRecord),
        (crate::adapters::probe_live_harness::Failure, bool),
    > {
        use crate::adapters::probe_live_harness::Failure;
        let mut attempted = false;
        let result = (|| {
            if cancellation.is_cancelled() {
                return Err(Failure::OwnerLostOrCancelled);
            }
            let tx = self
                .fresh_transaction()
                .map_err(|failure| failure.execution)?;
            let reference = CredentialRef::parse(&self.row.reference)
                .map_err(|_| Failure::SavedReferenceInvalidMetadata)?;
            if cancellation.is_cancelled() {
                return Err(Failure::OwnerLostOrCancelled);
            }
            attempted = true;
            let record = credentials.read(&reference).map_err(|error| match error {
                crate::application::credentials::CredentialServiceError::NotFound => {
                    Failure::CredentialNotFound
                }
                crate::application::credentials::CredentialServiceError::InvalidInput => {
                    Failure::CredentialInvalid
                }
                crate::application::credentials::CredentialServiceError::Unavailable => {
                    Failure::CredentialUnavailable
                }
            })?;
            if cancellation.is_cancelled() {
                return Err(Failure::OwnerLostOrCancelled);
            }
            tx.commit()
                .map_err(|_| Failure::SavedReferenceUnavailable)?;
            Ok((reference, record))
        })();
        result.map_err(|failure| (failure, attempted))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProductReadOnlyFailure {
    #[error("database ownership failed: {0}")]
    Ownership(OwnerFailure),
    #[error("read-only database is unavailable")]
    Unavailable,
    #[error("read-only database requires recovery or checkpoint")]
    RecoveryRequired,
    #[error("read-only database schema is incompatible")]
    SchemaMismatch,
}

// Private, non-cloneable/non-formatting consuming permit. Production has no
// caller-selected path; synthetic fixtures can construct one only inside tests.
struct ProductReadOnlyPermit(DatabaseOwner);

impl ProductReadOnlyPermit {
    fn acquire_standard() -> Result<Self, ProductReadOnlyFailure> {
        Self::acquire_standard_with(None)
    }
    fn acquire_standard_for_configuration(
        cancellation: &DavCancellation,
    ) -> Result<Self, ProductReadOnlyFailure> {
        Self::acquire_standard_with(Some(&|| cancellation.is_cancelled()))
    }
    fn acquire_standard_with(
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<Self, ProductReadOnlyFailure> {
        let path = std::env::var_os("APPDATA")
            .map(std::path::PathBuf::from)
            .ok_or(ProductReadOnlyFailure::Ownership(
                OwnerError::Missing.at(OwnerStep::CanonicalParentResolution),
            ))?
            .join("com.calendartodo.desktop")
            .join("calendar-todo.sqlite3");
        // An environment-relative identity must not resolve through cwd.
        if !path.is_absolute() {
            return Err(ProductReadOnlyFailure::Ownership(
                OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation),
            ));
        }
        match cancelled {
            Some(cancelled) => {
                DatabaseOwner::acquire_existing_read_only_for_configuration(&path, cancelled)
            }
            None => DatabaseOwner::acquire_existing_read_only(&path),
        }
        .map(Self)
        .map_err(ProductReadOnlyFailure::Ownership)
    }

    fn open(self) -> Result<GuardedProductReadOnly, ProductReadOnlyFailure> {
        reject_recovery_files(self.0.path())?;
        // immutable disables SQLite journal recovery and WAL/SHM creation.
        // It is safe only under our cooperating exclusive owner and after
        // rejecting all residual journals, including empty sidecars. It never
        // claims the main file includes any unapplied WAL contents.
        let mut uri = url::Url::from_file_path(self.0.path())
            .map_err(|_| ProductReadOnlyFailure::Unavailable)?;
        uri.set_query(Some("mode=ro&immutable=1"));
        let connection = Connection::open_with_flags(
            uri.as_str(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|_| ProductReadOnlyFailure::Unavailable)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .and_then(|()| connection.pragma_update(None, "query_only", true))
            .map_err(|_| ProductReadOnlyFailure::Unavailable)?;
        let schema: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|_| ProductReadOnlyFailure::Unavailable)?;
        // Read only the schema version, never run migrations/recovery/rebuild.
        if schema != crate::adapters::persistence::SCHEMA_VERSION {
            return Err(ProductReadOnlyFailure::SchemaMismatch);
        }
        Ok(GuardedProductReadOnly {
            connection: Some(connection),
            _owner: self.0,
        })
    }
}

fn reject_recovery_files(path: &Path) -> Result<(), ProductReadOnlyFailure> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        match std::fs::symlink_metadata(Path::new(&sidecar)) {
            Ok(_) => return Err(ProductReadOnlyFailure::RecoveryRequired),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ProductReadOnlyFailure::Unavailable),
        }
    }
    Ok(())
}

// No deref, raw getter, public callback or movable Connection. Future private
// profile selection/revalidation must stay inside this owned scope.
struct GuardedProductReadOnly {
    connection: Option<Connection>,
    _owner: DatabaseOwner,
}
impl Drop for GuardedProductReadOnly {
    fn drop(&mut self) {
        drop(self.connection.take());
    }
}

/// Historical diagnostic lookup only. Fixed product path, bounded two-row
/// legacy query; this is not trusted configured-profile selection evidence.
/// No ownership, connection, account authorization or transport permit escapes.
pub fn read_legacy_diagnostic_profiles() -> Result<Vec<(String, String)>, ProductReadOnlyFailure> {
    let database = ProductReadOnlyPermit::acquire_standard()?.open()?;
    read_legacy_profiles(&database)
}

fn read_legacy_profiles(
    database: &GuardedProductReadOnly,
) -> Result<Vec<(String, String)>, ProductReadOnlyFailure> {
    let connection = database
        .connection
        .as_ref()
        .ok_or(ProductReadOnlyFailure::Unavailable)?;
    let mut statement = connection
        .prepare("SELECT account_id,credential_ref FROM accounts WHERE region_profile='icloud-cn' AND enabled=1 AND credential_configured=1 AND removed_at_ms IS NULL LIMIT 2")
        .map_err(|_| ProductReadOnlyFailure::Unavailable)?;
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .and_then(|rows| rows.collect())
        .map_err(|_| ProductReadOnlyFailure::Unavailable)
}
