use super::*;
use crate::adapters::{icloud_caldav, icloud_caldav::ICloudBinding};

pub(super) const MIGRATION: &str = r#"
CREATE TABLE caldav_account_bindings (
    account_id TEXT PRIMARY KEY REFERENCES accounts(account_id) ON DELETE CASCADE,
    bootstrap_url TEXT NOT NULL,
    principal_url TEXT NOT NULL,
    home_url TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0)
) STRICT;
ALTER TABLE calendars ADD COLUMN canonical_collection_url TEXT;
CREATE UNIQUE INDEX calendars_canonical_route ON calendars(account_id,canonical_collection_url)
    WHERE canonical_collection_url IS NOT NULL;
"#;

impl Database {
    pub(crate) fn icloud_binding_lease(
        &self,
        account_id: String,
    ) -> Result<Option<(ICloudBinding, Arc<std::sync::atomic::AtomicBool>)>, PersistenceError> {
        self.request(|respond_to| Request::ICloudBindingLease {
            account_id,
            respond_to,
        })
    }
    pub(crate) fn icloud_binding(
        &self,
        account_id: String,
    ) -> Result<Option<ICloudBinding>, PersistenceError> {
        self.request(|respond_to| Request::ICloudBinding {
            account_id,
            respond_to,
        })
    }
    pub(crate) fn replace_icloud_discovery(
        &self,
        account_id: String,
        calendars: Vec<DiscoveredCalendarRecord>,
        binding: ICloudBinding,
        observed_at_ms: i64,
    ) -> Result<(), PersistenceError> {
        self.request(|respond_to| Request::ReplaceDiscoveredCalendars {
            account_id,
            calendars,
            binding: Some(binding),
            observed_at_ms,
            respond_to,
        })
    }
}

pub(super) fn load(
    c: &Connection,
    account: &str,
) -> Result<Option<ICloudBinding>, PersistenceError> {
    Ok(c.query_row("SELECT bootstrap_url,principal_url,home_url,generation FROM caldav_account_bindings WHERE account_id=?1", [account], |r| Ok(ICloudBinding { bootstrap_url:r.get(0)?,principal_url:r.get(1)?,home_url:r.get(2)?,generation:r.get(3)? })).optional()?)
}

/// Called inside the same discovery transaction as the existing calendar
/// replacement. Preserve legacy IDs/routes; a canonical bridge carries origin.
pub(super) fn prepare(
    tx: &Transaction<'_>,
    account: &str,
    binding: &ICloudBinding,
    calendars: &mut [DiscoveredCalendarRecord],
) -> Result<(), PersistenceError> {
    let home = binding
        .validate()
        .map_err(|_| PersistenceError::UnsafeSyncStage)?;
    let old = load(tx, account)?;
    let changed = old.as_ref().is_none_or(|b| {
        b.home_url != binding.home_url
            || b.principal_url != binding.principal_url
            || b.bootstrap_url != binding.bootstrap_url
    });
    let existing = {
        let mut stmt =
            tx.prepare("SELECT calendar_id,collection_url FROM calendars WHERE account_id=?1")?;
        stmt.query_map([account], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    if changed
        && existing
            .iter()
            .any(|(id, _)| calendar_has_pending_mutation(tx, id).unwrap_or(true))
    {
        // No origin substitution of durable pending work, including legacy
        // relative routes whose originating account binding is unavailable.
        return Err(PersistenceError::UnsafeSyncStage);
    }
    let mut canonical_seen = HashSet::new();
    for item in calendars.iter_mut() {
        let canonical = binding
            .collection(&item.collection_url)
            .map_err(|_| PersistenceError::UnsafeSyncStage)?;
        if canonical.as_str() != item.collection_url
            || !canonical_seen.insert(canonical.to_string())
        {
            return Err(PersistenceError::UnsafeSyncStage);
        }
        let mut candidates = existing.iter().filter(|(_, route)| {
            if route.starts_with('/') && !route.starts_with("//") {
                icloud_caldav::resolve(&home, route).is_ok_and(|u| u == canonical)
            } else {
                route == canonical.as_str()
            }
        });
        if let Some((id, route)) = candidates.next() {
            if candidates.next().is_some() {
                return Err(PersistenceError::IdentityConflict);
            }
            item.calendar_id = id.clone();
            item.collection_url = route.clone();
        }
    }
    let generation = match old {
        Some(b) if !changed => b.generation,
        Some(b) => b
            .generation
            .checked_add(1)
            .ok_or(PersistenceError::UnsafeSyncStage)?,
        None => 1,
    };
    tx.execute("INSERT INTO caldav_account_bindings(account_id,bootstrap_url,principal_url,home_url,generation) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(account_id) DO UPDATE SET bootstrap_url=excluded.bootstrap_url,principal_url=excluded.principal_url,home_url=excluded.home_url,generation=excluded.generation",params![account,binding.bootstrap_url,binding.principal_url,binding.home_url,generation])?;
    Ok(())
}

pub(super) fn canonicalize(
    tx: &Transaction<'_>,
    account: &str,
    binding: &ICloudBinding,
) -> Result<(), PersistenceError> {
    let routes = {
        let mut s = tx.prepare(
            "SELECT calendar_id,collection_url FROM calendars WHERE account_id=?1 AND available=1",
        )?;
        s.query_map([account], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    for (id, route) in routes {
        let canonical = binding
            .collection(&route)
            .map_err(|_| PersistenceError::UnsafeSyncStage)?;
        tx.execute(
            "UPDATE calendars SET canonical_collection_url=?2 WHERE calendar_id=?1",
            params![id, canonical.as_str()],
        )?;
    }
    Ok(())
}
