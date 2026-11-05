//! Durable two-slot exact-route witnesses; no route/calendar creation or authority.

pub(super) const MIGRATION: &str = r#"
CREATE TABLE probe_exact_route_witness (
 selection INTEGER PRIMARY KEY CHECK(selection IN (1,2)),
 account_id TEXT NOT NULL CHECK(length(account_id)=36),
 calendar_id TEXT NOT NULL CHECK(length(calendar_id)=36),
 route TEXT NOT NULL CHECK(length(CAST(route AS BLOB)) BETWEEN 2 AND 2048),
 target TEXT NOT NULL UNIQUE CHECK(length(CAST(target AS BLOB)) BETWEEN 2 AND 4096),
 revision TEXT NOT NULL CHECK(length(CAST(revision AS BLOB)) BETWEEN 3 AND 256),
 UNIQUE(account_id,calendar_id)
) STRICT;
"#;
