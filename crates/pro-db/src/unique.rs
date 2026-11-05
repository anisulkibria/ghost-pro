//! Recognising a unique-constraint violation, and which one.
//!
//! This exists because SQLite words the error two different ways depending on
//! how the index was declared, and getting it wrong is silent:
//!
//! * an index on plain columns names the **columns** —
//!   `UNIQUE constraint failed: webhook_events.provider, webhook_events.event_id`
//! * an index on an *expression*, such as `lower(slug)`, names the **index** —
//!   `UNIQUE constraint failed: index 'tenants_slug_unique'`
//!
//! Matching only on the index name therefore works for the case-insensitive
//! indexes in this schema and quietly fails for every other one. That failure
//! mode is nasty: the duplicate is reported as an internal error instead of a
//! conflict, so a webhook retry gets a 500 and the provider keeps redelivering
//! it forever, and a port collision reads as a database fault rather than as
//! the race it is.
//!
//! So a caller names *both* forms and this matches either.

/// Whether `error` is a unique violation involving any of `markers`.
///
/// Each marker is matched as a substring, so a caller passes the index name
/// and the qualified column names and does not care which SQLite chose:
///
/// ```ignore
/// if is_unique_violation(&msg, &["webhook_events_unique", "webhook_events.event_id"]) { … }
/// ```
pub fn is_unique_violation(error: &str, markers: &[&str]) -> bool {
    // Both wordings start the same way; checking it first means a marker that
    // happens to appear in some other error cannot produce a false positive.
    if !error.contains("UNIQUE constraint failed") {
        return false;
    }
    markers.iter().any(|m| error.contains(m))
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
