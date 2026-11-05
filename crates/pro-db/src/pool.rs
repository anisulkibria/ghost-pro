//! Opening the control-plane database, and the pragmas that make SQLite
//! behave under a web server.
use pro_core::{DbErrorExt, ProError, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

/// Open (creating if absent) and migrate the control-plane database.
///
/// The pragmas are not optional decoration — each one fixes a specific way
/// SQLite misbehaves when more than one request is in flight:
///
/// * **WAL** lets readers continue while a writer commits. Without it, the
///   dashboard blocks every time a tenant's `last_request_at` is stamped,
///   which on this platform is constantly.
/// * **busy_timeout** makes a contended write wait rather than fail outright.
///   SQLite's default is to return `SQLITE_BUSY` immediately, which surfaces
///   as a random 500 under load.
/// * **foreign_keys** is off by default in SQLite, so the `REFERENCES`
///   clauses in the schema would otherwise be documentation rather than
///   constraints — and a domain row could outlive its tenant.
/// * **synchronous = NORMAL** is the right trade with WAL on: a commit
///   survives a process crash, and only a power loss can lose the last
///   transactions. `FULL` costs an fsync per commit for a durability
///   guarantee a hosting control plane does not need.
pub async fn open(path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)
        .map_err(|e| ProError::Config(format!("bad database path {}: {e}", path.display())))?
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(10))
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

    let pool = SqlitePoolOptions::new()
        // A control plane is not the hot path — tenant processes serve the
        // actual sites — so a small pool is plenty, and a large one on SQLite
        // only increases write contention.
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect_with(opts)
        .await
        .db_err()?;

    migrate(&pool).await?;
    Ok(pool)
}

/// An in-memory database with the schema applied, for tests.
///
/// `max_connections(1)` matters: each connection to `:memory:` would
/// otherwise get its *own* empty database, so a test that wrote on one
/// connection and read on another would see nothing.
pub async fn open_in_memory() -> Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::from_str("sqlite::memory:")
                .map_err(|e| ProError::Db(e.to_string()))?
                .foreign_keys(true),
        )
        .await
        .db_err()?;
    migrate(&pool).await?;
    Ok(pool)
}

/// Apply the migrations compiled into the binary.
///
/// Embedded with `migrate!` rather than read from disk at runtime, so a
/// deployed binary cannot be out of step with the schema it expects — there is
/// no migrations directory to forget to copy.
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .map_err(|e| ProError::Db(format!("migration failed: {e}")))
}
