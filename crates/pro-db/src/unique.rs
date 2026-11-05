//! Recognising a unique-constraint violation, and which one.
//!
//! This exists because the engines word the error three different ways, and
//! getting it wrong is silent.
//!
//! SQLite has two wordings, depending on how the index was declared:
//!
//! * an index on plain columns names the **columns** —
//!   `UNIQUE constraint failed: webhook_events.provider, webhook_events.event_id`
//! * an index on an *expression*, such as `lower(slug)`, names the **index** —
//!   `UNIQUE constraint failed: index 'tenants_slug_unique'`
//!
//! MySQL has a third, and only ever names the **index**:
//!
//! * `Duplicate entry 'acme' for key 'tenants.tenants_slug_unique'`
//!   (MySQL 8 qualifies the key with the table; 5.7 and MariaDB do not)
//!
//! Matching only one of these works for some indexes on one engine and
//! quietly fails everywhere else. That failure mode is nasty: the duplicate
//! is reported as an internal error instead of a conflict, so a webhook retry
//! gets a 500 and the provider keeps redelivering it forever, and a port
//! collision reads as a database fault rather than as the race it is.
//!
//! So a caller names the index *and* the qualified columns, and this matches
//! any of the three wordings. Callers do not know which engine they are on,
//! which is the point.

/// Whether `error` is a unique violation involving any of `markers`.
///
/// Each marker is matched as a substring, so a caller passes the index name
/// and the qualified column names and does not care which form the engine
/// chose:
///
/// ```ignore
/// if is_unique_violation(&msg, &["webhook_events_unique", "webhook_events.event_id"]) { … }
/// ```
pub fn is_unique_violation(error: &str, markers: &[&str]) -> bool {
    // The prefix is checked first so that a marker which happens to appear in
    // some unrelated error cannot produce a false positive — a `NOT NULL`
    // failure also names `tenants.slug`.
    if !is_unique_error(error) {
        return false;
    }
    markers.iter().any(|m| error.contains(m))
}

/// Whether this error is a unique violation at all, on either engine.
fn is_unique_error(error: &str) -> bool {
    error.contains("UNIQUE constraint failed") || error.contains("Duplicate entry")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An index on plain columns: SQLite names the columns.
    #[test]
    fn matches_the_column_wording() {
        let err = "error returned from database: (code: 2067) UNIQUE constraint failed: \
                   webhook_events.provider, webhook_events.event_id";
        assert!(is_unique_violation(err, &["webhook_events_unique", "webhook_events.event_id"]));
    }

    /// An index on an expression, such as `lower(slug)`: SQLite names the
    /// index instead.
    #[test]
    fn matches_the_index_wording() {
        let err = "error returned from database: (code: 2067) UNIQUE constraint failed: \
                   index 'tenants_slug_unique'";
        assert!(is_unique_violation(err, &["tenants_slug_unique", "tenants.slug"]));
    }

    /// A different unique violation must not be mistaken for this one — a
    /// port collision is not a slug collision.
    #[test]
    fn does_not_match_another_constraint() {
        let err = "UNIQUE constraint failed: tenants.port";
        assert!(!is_unique_violation(err, &["tenants_slug_unique", "tenants.slug"]));
        assert!(is_unique_violation(err, &["tenants_port_unique", "tenants.port"]));
    }

    /// MySQL names the index and nothing else, so the index-name marker every
    /// caller already passes is what has to match.
    #[test]
    fn matches_mysqls_wording() {
        let err = "error returned from database: 1062 (23000): Duplicate entry \
                   'acme' for key 'tenants.tenants_slug_unique'";
        assert!(is_unique_violation(err, &["tenants_slug_unique", "tenants.slug"]));
    }

    /// MySQL 5.7 and MariaDB do not qualify the key with the table name.
    #[test]
    fn matches_mysqls_unqualified_wording() {
        let err = "Duplicate entry '21000' for key 'tenants_port_unique'";
        assert!(is_unique_violation(err, &["tenants_port_unique", "tenants.port"]));
    }

    /// A duplicate on one index must not read as a duplicate on another, on
    /// either engine — a port collision is not a slug collision.
    #[test]
    fn does_not_match_another_constraint_on_mysql() {
        let err = "Duplicate entry '21000' for key 'tenants.tenants_port_unique'";
        assert!(!is_unique_violation(err, &["tenants_slug_unique", "tenants.slug"]));
    }

    /// Any other failure is not a conflict, even if it mentions the table.
    #[test]
    fn does_not_match_unrelated_errors() {
        assert!(!is_unique_violation("database is locked", &["tenants.slug"]));
        assert!(!is_unique_violation(
            "NOT NULL constraint failed: tenants.slug",
            &["tenants.slug"]
        ));
        assert!(!is_unique_violation("", &["tenants.slug"]));
    }
}
