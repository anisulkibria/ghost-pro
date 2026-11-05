//! The few statements that cannot be written once for both engines.
//!
//! Almost all of this crate's SQL is already portable, and that is not luck:
//! every date is an ISO-8601 `TEXT` string, every id is application-generated
//! so nothing depends on `AUTO_INCREMENT`, and both engines take `?`
//! placeholders. What is left is this module — three constructs where SQLite
//! and MySQL genuinely disagree.
//!
//! Keeping them here, rather than as `if engine() == ...` at each call site,
//! means the list of things that differ between engines is a file you can
//! read rather than something you have to grep for. If you add a fourth, add
//! it here.
use pro_core::database::Engine;

use crate::pool::engine;

/// Quote an identifier that collides with a reserved word.
///
/// Backticks because they are the one quoting style *both* engines accept:
/// MySQL's native form, and supported by SQLite explicitly for MySQL
/// compatibility. The case that forces this is `platform_settings.key` —
/// `KEY` is reserved in MySQL, so an unquoted reference is a syntax error
/// there and fine in SQLite, which is exactly the kind of difference that
/// would otherwise be found in production.
pub fn quote(ident: &str) -> String {
    debug_assert!(
        !ident.contains('`'),
        "identifier {ident} contains a backtick, which cannot be quoted this way"
    );
    format!("`{ident}`")
}

/// The tail of an upsert: insert the row, or update it if the key is taken.
///
/// `conflict` names the unique columns that decide "already there", and `set`
/// names the columns to overwrite with the values just supplied.
///
/// The two engines spell this quite differently:
///
/// * SQLite — `ON CONFLICT(key) DO UPDATE SET value = excluded.value`, where
///   `excluded` is the row that failed to insert.
/// * MySQL — `ON DUPLICATE KEY UPDATE value = VALUES(value)`, which has no
///   equivalent of naming *which* constraint conflicted; it reacts to any
///   unique key on the table.
///
/// That last point is the one to be careful about. On a table with two unique
/// constraints the MySQL form can update on a conflict the SQLite form would
/// have let fail. Every caller here upserts on a single-key table, which makes
/// the two equivalent — a caller on a multi-key table must not use this.
///
/// `VALUES()` is used rather than MySQL 8.0.19's `AS new` row alias because
/// `VALUES()` also works on MySQL 5.7 and on MariaDB, which an operator may
/// well be running. It is marked deprecated in 8.0.20 but not removed.
pub fn upsert(conflict: &[&str], set: &[&str]) -> String {
    upsert_for(engine(), conflict, set)
}

/// [`upsert`] for a named engine, so both branches are testable on a machine
/// that only has SQLite.
fn upsert_for(engine: Engine, conflict: &[&str], set: &[&str]) -> String {
    match engine {
        Engine::Sqlite => {
            let keys =
                conflict.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ");
            let assigns = set
                .iter()
                .map(|c| format!("{q} = excluded.{q}", q = quote(c)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("ON CONFLICT({keys}) DO UPDATE SET {assigns}")
        }
        Engine::MySql => {
            let assigns = set
                .iter()
                .map(|c| format!("{q} = VALUES({q})", q = quote(c)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("ON DUPLICATE KEY UPDATE {assigns}")
        }
    }
}

/// `LIMIT`/`OFFSET` for "every row after the first `n`".
///
/// SQLite accepts a negative limit to mean "no limit", which MySQL rejects.
/// MySQL's documented idiom is the largest `BIGINT UNSIGNED`, which is ugly
/// but is what its own manual recommends; there is no `LIMIT ALL` outside
/// Postgres.
///
/// The offset is still bound as a parameter, so neither form interpolates a
/// caller's number into SQL.
pub fn skip_first() -> &'static str {
    skip_first_for(engine())
}

fn skip_first_for(engine: Engine) -> &'static str {
    match engine {
        Engine::Sqlite => "LIMIT -1 OFFSET ?",
        Engine::MySql => "LIMIT 18446744073709551615 OFFSET ?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sqlite_upsert_names_the_conflicting_key() {
        let sql = upsert_for(Engine::Sqlite, &["key"], &["value", "updated_at"]);
        assert_eq!(
            sql,
            "ON CONFLICT(`key`) DO UPDATE SET `value` = excluded.`value`, \
             `updated_at` = excluded.`updated_at`"
        );
    }

    /// MySQL has no `excluded` and no way to name the conflicting constraint.
    #[test]
    fn the_mysql_upsert_reuses_the_supplied_values() {
        let sql = upsert_for(Engine::MySql, &["key"], &["value", "updated_at"]);
        assert_eq!(
            sql,
            "ON DUPLICATE KEY UPDATE `value` = VALUES(`value`), \
             `updated_at` = VALUES(`updated_at`)"
        );
        assert!(!sql.contains("excluded"), "excluded is SQLite-only");
    }

    /// Whichever engine is in use, the live `engine()` path must produce one
    /// of the two forms and never a blend of them.
    #[test]
    fn the_live_upsert_matches_its_engine() {
        assert_eq!(upsert(&["key"], &["value"]), upsert_for(engine(), &["key"], &["value"]));
    }

    /// `key` is reserved in MySQL, so it has to come back quoted or the
    /// statement is a syntax error there.
    #[test]
    fn a_reserved_column_is_quoted() {
        assert_eq!(quote("key"), "`key`");
        assert!(upsert(&["key"], &["value"]).contains("`key`"));
    }

    #[test]
    fn skipping_rows_still_binds_the_offset() {
        assert!(skip_first().ends_with("OFFSET ?"));
    }

    /// A negative limit is SQLite-only; MySQL rejects it outright, which is
    /// why the two forms exist at all.
    #[test]
    fn only_sqlite_takes_a_negative_limit() {
        assert_eq!(skip_first_for(Engine::Sqlite), "LIMIT -1 OFFSET ?");
        let mysql = skip_first_for(Engine::MySql);
        assert!(!mysql.contains("-1"), "{mysql}");
        assert!(mysql.contains(&u64::MAX.to_string()), "{mysql}");
    }
}
