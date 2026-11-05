//! Which database this platform runs on, and how to reach it.
//!
//! One module because the answer is needed in four places that must not each
//! invent it: the server opens a pool, `doctor` reports on it, `pro-provision`
//! hands a connection to every tenant it creates, and the backup path has to
//! know whether a tenant's data is a file to copy or a schema to dump.
//!
//! ## The rule
//!
//! Nothing configured means SQLite, in a file, next to everything else this
//! platform writes. That is the local default and it needs no setup at all —
//! the first `serve` creates it.
//!
//! A `database.url` beginning `mysql://` means MySQL, which is what a
//! production deployment wants, for a reason worth stating: a SQLite control
//! plane is one file on one disk, so it pins the whole platform to a single
//! machine. MySQL is what allows a second app node, a managed backup, and a
//! failover that does not involve copying a file.
//!
//! The choice is made by the presence of a URL rather than by `PRO_ENV`, so
//! that staging can be tested against MySQL without pretending to be
//! production, and so that a production box with no MySQL yet still starts.
use std::path::{Path, PathBuf};

use crate::error::{ProError, Result};

/// The database engines this platform can run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Sqlite,
    MySql,
}

impl Engine {
    /// The name used in config, logs and `doctor` output.
    pub fn name(self) -> &'static str {
        match self {
            Engine::Sqlite => "sqlite",
            Engine::MySql => "mysql",
        }
    }

    /// Whether this engine is safe to run a production platform on.
    ///
    /// SQLite is not unsafe in itself — it is a correct, durable database.
    /// What it cannot do is let two machines serve the same platform, which
    /// is the thing production eventually needs.
    pub fn is_single_machine(self) -> bool {
        matches!(self, Engine::Sqlite)
    }
}

/// A database this platform can open: an engine plus the URL to reach it.
///
/// Construct through [`Target::sqlite_file`] or [`Target::parse`] so that the
/// URL and the engine can never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    engine: Engine,
    url: String,
}

impl Target {
    /// A SQLite database in a file, created on first open.
    pub fn sqlite_file(path: &Path) -> Self {
        // `?mode=rwc` is what creates the file when it is absent. It has to be
        // in the URL rather than set on the options struct, because the `Any`
        // driver reaches SQLite through `SqliteConnectOptions::from_url` and
        // discards anything that was not expressible as a URL parameter.
        Target {
            engine: Engine::Sqlite,
            url: format!("sqlite://{}?mode=rwc", path.display()),
        }
    }

    /// A private SQLite database that exists only while it is open, for tests.
    pub fn sqlite_memory() -> Self {
        Target { engine: Engine::Sqlite, url: "sqlite::memory:".into() }
    }

    /// Read a configured URL, rejecting anything this platform cannot open.
    ///
    /// The error names the schemes that do work, because the failure this
    /// guards against is a `postgres://` URL in `pro.production.json` — which
    /// sqlx would accept at the type level and then fail to connect with,
    /// long after boot.
    pub fn parse(url: &str) -> Result<Self> {
        let url = url.trim();
        let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());

        match scheme.as_deref() {
            Some("mysql") | Some("mariadb") => {
                Ok(Target { engine: Engine::MySql, url: url.to_string() })
            }
            Some("sqlite") => Ok(Target { engine: Engine::Sqlite, url: url.to_string() }),
            // `sqlite::memory:` has no `://`.
            _ if url.starts_with("sqlite:") => {
                Ok(Target { engine: Engine::Sqlite, url: url.to_string() })
            }
            Some(other) => Err(ProError::Config(format!(
                "database.url names {other}, which this platform cannot open. \
                 Use mysql:// for a server, sqlite:// for a file, or leave it \
                 unset to keep the local file database"
            ))),
            None => Err(ProError::Config(format!(
                "database.url needs a scheme: {url} — mysql://user:pass@host/db \
                 for a server, or leave it unset to keep the local file database"
            ))),
        }
    }

    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// The URL to hand to sqlx. Carries the password; never log this.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The URL with the password replaced, for logs, `doctor` and the console.
    ///
    /// Every path that shows an operator which database they are on goes
    /// through here. A connection URL is the one config value that routinely
    /// contains a credential, and a platform that prints it into `server.log`
    /// has leaked it to anyone who can read a log — including, on this
    /// platform, into a support bundle.
    pub fn redacted(&self) -> String {
        redact(&self.url)
    }

    /// The file a SQLite target lives in, or `None` for a server or memory.
    ///
    /// Used by the paths that treat the database as a file: the backup
    /// snapshot, and the `doctor` check that reports its size.
    pub fn sqlite_path(&self) -> Option<PathBuf> {
        if self.engine != Engine::Sqlite {
            return None;
        }
        let rest = self.url.strip_prefix("sqlite://").or_else(|| self.url.strip_prefix("sqlite:"))?;
        let path = rest.split('?').next().unwrap_or(rest);
        (!path.is_empty() && path != ":memory:").then(|| PathBuf::from(path))
    }
}

/// Replace the password in a connection URL with `***`.
///
/// Deliberately conservative: anything that does not look like
/// `scheme://user:pass@host` is returned untouched, because a URL this does
/// not understand is better shown in full than mangled into something an
/// operator cannot recognise. A SQLite path has no credential to hide.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    // Only the authority section can hold a credential; a password is never
    // after the first `/`.
    let (authority, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    let user = userinfo.split_once(':').map(|(u, _)| u).unwrap_or(userinfo);
    if userinfo.contains(':') {
        format!("{scheme}://{user}:***@{host}{tail}")
    } else {
        url.to_string()
    }
}

/// Where a tenant's own database lives.
///
/// A tenant is a whole Ghost site, and its data is not the platform's: posts,
/// members, staff logins. Each tenant therefore gets a database of its own
/// under both engines, and the platform never puts two tenants in one schema
/// — a prefix bug in a shared schema would be a cross-customer data leak,
/// which is the one failure this architecture is built to make impossible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantStore {
    /// `content/data/ghost.db` inside the tenant's own directory.
    SqliteFile,
    /// A database per tenant on a shared MySQL server.
    MySqlServer {
        /// `mysql://admin:pass@host:port` with no database path — the server
        /// tenant databases are created on.
        server: String,
        /// Prepended to every tenant database and user name.
        prefix: String,
    },
}

impl TenantStore {
    pub fn engine(&self) -> Engine {
        match self {
            TenantStore::SqliteFile => Engine::Sqlite,
            TenantStore::MySqlServer { .. } => Engine::MySql,
        }
    }

    /// The database name for a tenant, from its id.
    ///
    /// The tenant id is used rather than its slug because a slug can be
    /// changed by its owner and a database cannot be renamed underneath a
    /// running site. Ids here are 24-character hex, so the result is always a
    /// legal MySQL identifier and always within the 64-character limit.
    pub fn database_name(&self, tenant_id: &str) -> Option<String> {
        match self {
            TenantStore::SqliteFile => None,
            TenantStore::MySqlServer { prefix, .. } => Some(format!("{prefix}{tenant_id}")),
        }
    }

    /// The MySQL user a tenant's own process connects as.
    ///
    /// A user per tenant, not one shared account: the grant it holds covers
    /// that tenant's database and nothing else, so a compromised or buggy
    /// tenant process cannot read another customer's posts even though both
    /// are on one server. MySQL usernames are capped at 32 characters, which
    /// a 24-hex id plus a short prefix fits.
    pub fn user_name(&self, tenant_id: &str) -> Option<String> {
        match self {
            TenantStore::SqliteFile => None,
            TenantStore::MySqlServer { prefix, .. } => {
                let name = format!("{prefix}{tenant_id}");
                Some(name.chars().take(32).collect())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_configured_is_a_local_file() {
        let t = Target::sqlite_file(Path::new("/srv/pro/data/pro.db"));
        assert_eq!(t.engine(), Engine::Sqlite);
        assert_eq!(t.sqlite_path().unwrap(), PathBuf::from("/srv/pro/data/pro.db"));
    }

    /// `mode=rwc` is what creates the file, and it must survive into the URL
    /// because the `Any` driver cannot be told any other way.
    #[test]
    fn a_file_target_creates_itself() {
        let t = Target::sqlite_file(Path::new("data/pro.db"));
        assert!(t.url().contains("mode=rwc"), "{}", t.url());
    }

    #[test]
    fn a_mysql_url_selects_mysql() {
        let t = Target::parse("mysql://pro:secret@db.internal:3306/pro").unwrap();
        assert_eq!(t.engine(), Engine::MySql);
        assert!(t.sqlite_path().is_none());
    }

    /// MariaDB speaks the MySQL protocol, and an operator who writes its own
    /// scheme should not be told their database is unsupported.
    #[test]
    fn mariadb_is_mysql() {
        assert_eq!(Target::parse("mariadb://a:b@h/d").unwrap().engine(), Engine::MySql);
    }

    #[test]
    fn an_unsupported_engine_is_refused_by_name() {
        let err = Target::parse("postgres://a:b@h/d").unwrap_err().to_string();
        assert!(err.contains("postgres"), "{err}");
        assert!(err.contains("mysql://"), "{err}");
    }

    #[test]
    fn a_url_without_a_scheme_is_refused() {
        assert!(Target::parse("db.internal/pro").is_err());
    }

    #[test]
    fn memory_is_not_a_path() {
        assert!(Target::sqlite_memory().sqlite_path().is_none());
    }

    /// The whole point of `redacted`: this string ends up in `server.log`.
    #[test]
    fn a_password_never_reaches_a_log() {
        let t = Target::parse("mysql://pro:s3cret@db.internal:3306/pro").unwrap();
        assert_eq!(t.redacted(), "mysql://pro:***@db.internal:3306/pro");
        assert!(!t.redacted().contains("s3cret"));
    }

    /// A password containing an `@` must not fool the split — the host is
    /// after the *last* `@`, not the first.
    #[test]
    fn an_at_sign_in_a_password_is_handled() {
        assert_eq!(
            redact("mysql://pro:p@ss@db.internal/pro"),
            "mysql://pro:***@db.internal/pro"
        );
    }

    #[test]
    fn a_url_with_no_credential_is_left_alone() {
        assert_eq!(redact("mysql://db.internal/pro"), "mysql://db.internal/pro");
        assert_eq!(redact("sqlite://data/pro.db?mode=rwc"), "sqlite://data/pro.db?mode=rwc");
    }

    #[test]
    fn sqlite_is_known_to_be_one_machine() {
        assert!(Engine::Sqlite.is_single_machine());
        assert!(!Engine::MySql.is_single_machine());
    }

    #[test]
    fn a_tenant_database_is_named_from_its_id() {
        let store = TenantStore::MySqlServer {
            server: "mysql://root:r@127.0.0.1:3306".into(),
            prefix: "ghost_".into(),
        };
        let id = "c32a6ef5d844be317eca55e2";
        assert_eq!(store.database_name(id).unwrap(), "ghost_c32a6ef5d844be317eca55e2");
    }

    /// MySQL refuses a username over 32 characters, so the name is capped
    /// rather than being rejected by the server at provision time.
    #[test]
    fn a_tenant_user_fits_mysqls_limit() {
        let store = TenantStore::MySqlServer {
            server: "mysql://root:r@127.0.0.1:3306".into(),
            prefix: "ghost_platform_".into(),
        };
        let user = store.user_name("c32a6ef5d844be317eca55e2").unwrap();
        assert!(user.len() <= 32, "{user} is {} chars", user.len());
    }

    #[test]
    fn a_file_store_has_no_server_names() {
        assert!(TenantStore::SqliteFile.database_name("abc").is_none());
        assert!(TenantStore::SqliteFile.user_name("abc").is_none());
    }
}
