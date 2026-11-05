//! Opening the control-plane database, on either engine.
//!
//! The platform runs on SQLite locally and MySQL in production, and the query
//! code above this module does not know which. That is done with sqlx's `Any`
//! driver, which picks a backend from the URL at runtime.
//!
//! Two things about `Any` are worth knowing before changing anything here.
//!
//! **It carries a narrow set of types**: `bool`, `i16`/`i32`/`i64`,
//! `f32`/`f64`, `String` and `Vec<u8>`. There is deliberately no timestamp
//! type in that list, and this schema does not need one — every date in the
//! control plane is stored as an ISO-8601 `TEXT` string (see
//! [`pro_core::now_iso`]), which both engines sort correctly and neither
//! reinterprets. A column added as a native `DATETIME` would not decode.
//!
//! **It reaches SQLite through `SqliteConnectOptions::from_url`**, and that
//! parser understands only `mode`, `cache`, `immutable` and `vfs`. The pragmas
//! this database depends on cannot be set the way they were before — they are
//! applied in [`after_connect`] instead, on every new connection. Dropping
//! them is not a performance regression but a correctness one, so each is
//! justified where it is set.
use pro_core::database::{Engine, Target};
use pro_core::{DbErrorExt, ProError, Result};
use sqlx::any::{AnyPoolOptions, install_default_drivers};
use sqlx::migrate::MigrateDatabase;
use sqlx::{Any, AnyPool, Executor};
use std::sync::atomic::{AtomicU8, Ordering};

/// The control plane's connection pool, whichever engine is behind it.
///
/// Named `Db` rather than `AnyPool` so that the crates above read as talking
/// to *this platform's* database and not to an arbitrary one.
pub type Db = AnyPool;

/// Which engine [`open`] last connected to.
///
/// A process has exactly one control-plane database, so this is a property of
/// the process and not of a value that has to be threaded through several
/// hundred query functions. It exists for the handful of statements that
/// cannot be written once for both engines — see [`crate::dialect`].
static ENGINE: AtomicU8 = AtomicU8::new(ENGINE_SQLITE);
const ENGINE_SQLITE: u8 = 0;
const ENGINE_MYSQL: u8 = 1;

/// The engine the control plane is on. SQLite until [`open`] says otherwise,
/// which is also the right answer for a test using an in-memory database.
pub fn engine() -> Engine {
    match ENGINE.load(Ordering::Relaxed) {
        ENGINE_MYSQL => Engine::MySql,
        _ => Engine::Sqlite,
    }
}

fn remember(engine: Engine) {
    ENGINE.store(
        match engine {
            Engine::Sqlite => ENGINE_SQLITE,
            Engine::MySql => ENGINE_MYSQL,
        },
        Ordering::Relaxed,
    );
}

/// Open (creating if absent) and migrate the control-plane database.
///
/// `pool_size` comes from
/// [`pro_core::config::DatabaseConfig::pool_size`], which defaults it per
/// engine — the two want opposite things.
pub async fn open(target: &Target, pool_size: u32) -> Result<Db> {
    // `Any` panics on first use if no driver has been registered, and the
    // registration is idempotent. Done here rather than in `main` so that no
    // caller — including a test, or a future binary — can forget it.
    install_default_drivers();
    remember(target.engine());

    match target.engine() {
        Engine::Sqlite => {
            // The parent has to exist before SQLite will create the file.
            if let Some(path) = target.sqlite_path() {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
            }
        }
        Engine::MySql => ensure_database(target).await?,
    }

    let engine = target.engine();
    let pool = AnyPoolOptions::new()
        .max_connections(pool_size)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .after_connect(move |conn, _| {
            Box::pin(async move { after_connect(engine, conn).await })
        })
        .connect(target.url())
        .await
        .map_err(|e| {
            // The URL carries a password, so the engine and host are named
            // from the redacted form. An operator debugging a refused
            // connection needs to know *which* database was refused.
            ProError::Db(format!("could not connect to {}: {e}", target.redacted()))
        })?;

    migrate(&pool).await?;
    Ok(pool)
}

/// Settings applied to every new connection.
///
/// `after_connect` rather than connect options because the `Any` driver
/// discards per-engine options; see the module docs.
async fn after_connect(
    engine: Engine,
    conn: &mut sqlx::AnyConnection,
) -> std::result::Result<(), sqlx::Error> {
    match engine {
        Engine::Sqlite => {
            // Each of these fixes a specific way SQLite misbehaves under a web
            // server, and each was previously a connect option:
            //
            // * **WAL** lets readers continue while a writer commits. Without
            //   it the dashboard blocks every time a tenant's
            //   `last_request_at` is stamped, which here is constantly.
            // * **busy_timeout** makes a contended write wait instead of
            //   failing. SQLite otherwise returns `SQLITE_BUSY` at once, which
            //   surfaces as a random 500 under load.
            // * **foreign_keys** is *off* by default in SQLite, so the
            //   `REFERENCES` clauses in the schema would be documentation
            //   rather than constraints — and a domain row could outlive its
            //   tenant.
            // * **synchronous = NORMAL** is the right trade with WAL on: a
            //   commit survives a process crash, and only power loss can lose
            //   the last transactions. `FULL` costs an fsync per commit for a
            //   guarantee a control plane does not need.
            conn.execute("PRAGMA journal_mode = WAL").await?;
            conn.execute("PRAGMA busy_timeout = 10000").await?;
            conn.execute("PRAGMA foreign_keys = ON").await?;
            conn.execute("PRAGMA synchronous = NORMAL").await?;
        }
        Engine::MySql => {
            // MySQL's session defaults are what differ most from SQLite's, and
            // all three of these would otherwise be silent behaviour changes
            // between an operator's laptop and their server:
            //
            // * **time_zone = UTC** so that any date MySQL computes itself
            //   agrees with the ISO-8601 UTC strings this platform writes. A
            //   server defaulting to local time would make a trial end at the
            //   wrong hour.
            // * **STRICT_ALL_TABLES** turns a value too long for its column
            //   into an error instead of a silent truncation. A silently
            //   truncated domain name or token is far worse than a refused
            //   insert.
            // * **NO_ENGINE_SUBSTITUTION** refuses to quietly swap the storage
            //   engine, which would cost us transactions.
            conn.execute("SET time_zone = '+00:00'").await?;
            conn.execute(
                "SET SESSION sql_mode = 'STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION'",
            )
            .await?;
        }
    }
    Ok(())
}

/// Create the MySQL database named in the URL if it is not there yet.
///
/// This is what makes pointing a fresh deployment at a MySQL server a
/// one-liner: the operator creates an account, and the platform creates its
/// own schema on first boot exactly as it does with a SQLite file. Without it,
/// MySQL would be the one engine that needed a manual `CREATE DATABASE` before
/// the platform would start.
///
/// The error is deliberately specific about privileges, because "access
/// denied" from MySQL does not tell an operator which grant is missing.
async fn ensure_database(target: &Target) -> Result<()> {
    match Any::database_exists(target.url()).await {
        Ok(true) => Ok(()),
        Ok(false) => Any::create_database(target.url()).await.map_err(|e| {
            ProError::Db(format!(
                "the database in {} does not exist and could not be created: {e}. \
                 Either create it by hand, or grant CREATE to this account",
                target.redacted()
            ))
        }),
        // Not fatal: an account can be allowed to use a database without being
        // allowed to list the server's schemas, which is the correct, locked
        // down setup for an app user. Let the connection itself be the test.
        Err(e) => {
            tracing::debug!("could not check whether the database exists: {e}");
            Ok(())
        }
    }
}

/// An in-memory database with the schema applied, for tests.
///
/// `max_connections(1)` matters: each connection to `:memory:` would otherwise
/// get its *own* empty database, so a test that wrote on one connection and
/// read on another would see nothing.
pub async fn open_in_memory() -> Result<Db> {
    install_default_drivers();
    remember(Engine::Sqlite);

    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move { after_connect(Engine::Sqlite, conn).await })
        })
        .connect(Target::sqlite_memory().url())
        .await
        .db_err()?;

    migrate(&pool).await?;
    Ok(pool)
}

/// Apply the migrations compiled into the binary, for the engine in use.
///
/// Embedded with `migrate!` rather than read from disk, so a deployed binary
/// cannot be out of step with the schema it expects — there is no migrations
/// directory to forget to copy.
///
/// Two directories rather than one set of portable DDL, because the
/// differences are not cosmetic: MySQL cannot index a `TEXT` primary key
/// without a prefix length, has no `CREATE INDEX IF NOT EXISTS`, and spells
/// the types differently. Attempting one dialect for both engines would mean
/// writing to the lowest common denominator of each — and a schema nobody can
/// read is a schema that grows bugs. The two files are kept in step by
/// [`crate::tests::both_engines_declare_the_same_tables`].
pub async fn migrate(pool: &Db) -> Result<()> {
    let result = match engine() {
        Engine::Sqlite => sqlx::migrate!("../../migrations/sqlite").run(pool).await,
        Engine::MySql => sqlx::migrate!("../../migrations/mysql").run(pool).await,
    };
    result.map_err(|e| ProError::Db(format!("migration failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The in-memory helper has to produce a usable, migrated database, since
    /// every other test in this crate is built on it.
    #[tokio::test]
    async fn an_in_memory_database_is_migrated() {
        let pool = open_in_memory().await.unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tenants")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Foreign keys are off by default in SQLite, and the schema's
    /// `REFERENCES` clauses are only constraints if the pragma is applied to
    /// every connection. This is the test that catches `after_connect` being
    /// dropped or reordered.
    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let pool = open_in_memory().await.unwrap();
        let (on,): (i64,) =
            sqlx::query_as("PRAGMA foreign_keys").fetch_one(&pool).await.unwrap();
        assert_eq!(on, 1, "foreign keys are not being enforced");
    }

    #[tokio::test]
    async fn a_file_database_is_created_on_open() {
        let dir = std::env::temp_dir().join(format!("pro-pool-{}", pro_core::ids::object_id()));
        let target = Target::sqlite_file(&dir.join("nested/pro.db"));
        let pool = open(&target, 2).await.unwrap();
        sqlx::query("SELECT 1").fetch_one(&pool).await.unwrap();
        assert!(dir.join("nested/pro.db").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_engine_defaults_to_sqlite() {
        // A process that has not opened a database still has to answer, and
        // the local file is the right assumption.
        assert_eq!(engine(), Engine::Sqlite);
    }
}
