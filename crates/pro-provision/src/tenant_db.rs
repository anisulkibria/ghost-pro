//! Each tenant's own database, when tenants live on MySQL.
//!
//! A tenant is a whole Ghost site, and its data belongs to the customer, not
//! to the platform: posts, drafts, members, their own staff logins. So every
//! tenant gets a database of its own and a MySQL user of its own, and that
//! user is granted rights on that one database and nothing else.
//!
//! That is not tidiness. The alternative — one schema with a per-site table
//! prefix, or one shared login — makes a cross-customer data leak reachable
//! from a single missing `WHERE`, a prefix built from the wrong variable, or
//! any SQL injection in the CMS. With a grant per tenant, the *server*
//! refuses, so a bug in the CMS cannot read another customer's drafts even if
//! it tries. It is the same boundary the SQLite layout gets for free by
//! putting each site's data in a separate file.
//!
//! Nothing here runs on a local SQLite deployment; see
//! [`pro_core::TenantStore`].
use pro_core::{ProError, Result, TenantStore};
use sqlx::any::install_default_drivers;
use sqlx::{AnyConnection, Connection, Executor};

/// Everything a tenant's Ghost needs in order to reach its own database.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub database: String,
    pub user: String,
    pub password: String,
    /// `host:port` of the server, taken from the configured admin URL so the
    /// tenant is never told a different server than the one provisioned on.
    pub host: String,
    pub port: u16,
}

/// Open an admin connection to the tenant server.
///
/// A single connection rather than a pool: these operations happen once per
/// site, at provision and delete time, and an idle pool holding an account
/// with `CREATE USER` is a standing privilege this platform should not keep.
async fn connect(store: &TenantStore) -> Result<AnyConnection> {
    let TenantStore::MySqlServer { server, .. } = store else {
        return Err(ProError::Internal(
            "tenant databases are on SQLite; there is no server to connect to".into(),
        ));
    };
    install_default_drivers();

    // MySQL needs a database to connect *to*, and the configured URL
    // deliberately names none — `mysql` is the one schema guaranteed to exist
    // on every server, and the admin account can always see it.
    let url = format!("{server}/mysql");
    AnyConnection::connect(&url).await.map_err(|e| {
        ProError::Unavailable(format!(
            "could not reach the tenant database server at {}: {e}",
            pro_core::database::redact(server)
        ))
    })
}

/// Check the configured account can actually provision a tenant.
///
/// Run by `ghost-pro db` and `doctor` rather than discovered at the first
/// signup. The failure this prevents is the expensive one: a customer pays,
/// provisioning starts, and the platform finds out mid-sequence that it
/// cannot create a database — leaving a charged customer with a broken site.
///
/// Creates a throwaway database and drops it again, because the only reliable
/// test of a grant is to use it. `SHOW GRANTS` would have to be parsed, and
/// would still not account for a read-only server or a database quota.
pub async fn check_server(store: &TenantStore) -> Result<()> {
    let mut conn = connect(store).await?;
    let probe = format!("ghost_probe_{}", pro_core::ids::object_id());

    conn.execute(format!("CREATE DATABASE {}", quote_ident(&probe)?).as_str())
        .await
        .map_err(|e| {
            ProError::Config(format!(
                "this account cannot create a database: {e}. Grant it with: \
                 GRANT ALL PRIVILEGES ON `ghost_%`.* TO '<user>'@'%' WITH GRANT OPTION"
            ))
        })?;

    let dropped = conn
        .execute(format!("DROP DATABASE {}", quote_ident(&probe)?).as_str())
        .await
        .map_err(|e| {
            ProError::Config(format!(
                "created a test database but could not drop it ({e}). Remove \
                 `{probe}` by hand; deleting a site will not work until this \
                 account has DROP"
            ))
        });
    let _ = conn.close().await;
    dropped?;
    Ok(())
}

/// Create a tenant's database and its own scoped login.
///
/// Idempotent on the database and the user, so a retried provision does not
/// fail on a half-finished one — but the password is reissued each time, which
/// is why the caller must write the returned credentials into the tenant's
/// config before starting it.
pub async fn create(store: &TenantStore, tenant_id: &str) -> Result<Credentials> {
    let (Some(database), Some(user)) =
        (store.database_name(tenant_id), store.user_name(tenant_id))
    else {
        return Err(ProError::Internal(
            "tenant databases are on SQLite; nothing to create".into(),
        ));
    };
    let TenantStore::MySqlServer { server, .. } = store else { unreachable!() };
    let (host, port) = split_host_port(server)?;

    // 256 bits from the OS, hex-encoded: this is a credential, and it is never
    // shown to the customer or derived from the tenant id.
    let password = pro_core::ids::secret_token();
    let db_ident = quote_ident(&database)?;
    let user_lit = quote_literal(&user);
    let pass_lit = quote_literal(&password);

    let mut conn = connect(store).await?;

    // Collation matched to Ghost's own: utf8mb4 so a post can hold an emoji,
    // and the same default collation upstream Ghost's schema uses, so a
    // database exported from here imports into stock Ghost unchanged.
    let steps: Vec<String> = vec![
        format!(
            "CREATE DATABASE IF NOT EXISTS {db_ident} \
             CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci"
        ),
        format!("CREATE USER IF NOT EXISTS {user_lit}@'%' IDENTIFIED BY {pass_lit}"),
        // Re-set on every provision so a retry recovers a user whose password
        // was lost with a failed attempt.
        format!("ALTER USER {user_lit}@'%' IDENTIFIED BY {pass_lit}"),
        // The grant is this tenant's database and no other. Deliberately not
        // `ON *.*`, and deliberately not including GRANT OPTION — a tenant
        // process must not be able to widen its own access.
        format!("GRANT ALL PRIVILEGES ON {db_ident}.* TO {user_lit}@'%'"),
        "FLUSH PRIVILEGES".to_string(),
    ];

    for sql in steps {
        if let Err(e) = conn.execute(sql.as_str()).await {
            let _ = conn.close().await;
            // The statement is not included in the error: it carries the
            // generated password.
            return Err(ProError::Db(format!(
                "could not provision the database for site {tenant_id}: {e}"
            )));
        }
    }
    let _ = conn.close().await;

    Ok(Credentials { database, user, password, host, port })
}

/// Remove a tenant's database and its login.
///
/// Called when a site's files are deleted, and for the same reason: a database
/// left behind after a site is gone is a copy of a former customer's content
/// that nobody is looking after, and it still counts against the server.
///
/// Errors are returned rather than ignored, but the caller treats this as
/// best-effort — a site whose files are gone must not be stuck undeleted
/// because the database server was briefly unreachable.
pub async fn drop(store: &TenantStore, tenant_id: &str) -> Result<()> {
    let (Some(database), Some(user)) =
        (store.database_name(tenant_id), store.user_name(tenant_id))
    else {
        return Ok(());
    };

    let mut conn = connect(store).await?;
    let result = async {
        conn.execute(format!("DROP DATABASE IF EXISTS {}", quote_ident(&database)?).as_str())
            .await
            .map_err(|e| ProError::Db(format!("could not drop {database}: {e}")))?;
        conn.execute(format!("DROP USER IF EXISTS {}@'%'", quote_literal(&user)).as_str())
            .await
            .map_err(|e| ProError::Db(format!("could not drop user {user}: {e}")))?;
        Ok::<(), ProError>(())
    }
    .await;
    let _ = conn.close().await;
    result
}

/// Quote an identifier for a context where MySQL takes no parameter.
///
/// `CREATE DATABASE ?` is not a thing — database and user names have to be
/// interpolated, which is exactly where an injection would live. So rather
/// than escaping, this *refuses* anything that is not already a safe name.
/// Every value reaching it is built from a 24-hex tenant id and a configured
/// prefix, so a rejection means a programming error, not bad user input.
fn quote_ident(name: &str) -> Result<String> {
    let safe = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !safe {
        return Err(ProError::Internal(format!(
            "refusing to use {name:?} as a database identifier: only letters, \
             digits and underscore are allowed"
        )));
    }
    Ok(format!("`{name}`"))
}

/// Quote a string literal — a username or a password — for MySQL.
///
/// Usernames are constrained the same way identifiers are, but a password is
/// 64 hex characters from [`pro_core::ids::secret_token`] and still goes
/// through here so that the escaping is not something a future change to token
/// generation can quietly invalidate.
fn quote_literal(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

/// Split `mysql://user:pass@host:port` into the host and port a tenant is told
/// to connect to.
///
/// 3306 when no port is given, which is MySQL's default and what a connection
/// string without one means.
fn split_host_port(server: &str) -> Result<(String, u16)> {
    let authority = server.split_once("://").map(|(_, r)| r).unwrap_or(server);
    // The credential may itself contain '@', so the host is after the last one.
    let hostish = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let hostish = hostish.trim_end_matches('/');

    // An IPv6 literal is bracketed, and its colons are not the port separator.
    if let Some(rest) = hostish.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| {
            ProError::Config(format!("unbalanced [ in database.tenantUrl host {hostish}"))
        })?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| {
                ProError::Config(format!("{p} is not a port in database.tenantUrl"))
            })?,
            None => 3306,
        };
        return Ok((format!("[{host}]"), port));
    }

    match hostish.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse().map_err(|_| {
                ProError::Config(format!("{port} is not a port in database.tenantUrl"))
            })?;
            Ok((host.to_string(), port))
        }
        None => Ok((hostish.to_string(), 3306)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TenantStore {
        TenantStore::MySqlServer {
            server: "mysql://root:secret@db.internal:3307".into(),
            prefix: "ghost_".into(),
        }
    }

    #[test]
    fn an_identifier_is_backquoted() {
        assert_eq!(quote_ident("ghost_abc123").unwrap(), "`ghost_abc123`");
    }

    /// The whole defence for a name that cannot be a bound parameter: refuse
    /// rather than escape. A backtick or a space in a database name is a
    /// programming error, and must not become a statement.
    #[test]
    fn a_dangerous_identifier_is_refused() {
        for bad in ["ghost`; DROP DATABASE pro; --", "ghost db", "", "a-b", "ghost'"] {
            assert!(quote_ident(bad).is_err(), "{bad:?} was accepted");
        }
    }

    /// MySQL caps an identifier at 64 characters, and a silently truncated
    /// database name would point a site at the wrong data.
    #[test]
    fn an_overlong_identifier_is_refused() {
        assert!(quote_ident(&"a".repeat(65)).is_err());
        assert!(quote_ident(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn a_literal_escapes_quotes_and_backslashes() {
        assert_eq!(quote_literal("ab"), "'ab'");
        assert_eq!(quote_literal("a'b"), "'a\\'b'");
        assert_eq!(quote_literal("a\\b"), "'a\\\\b'");
    }

    #[test]
    fn the_host_and_port_come_from_the_server_url() {
        assert_eq!(split_host_port("mysql://root:s@db.internal:3307").unwrap(),
                   ("db.internal".to_string(), 3307));
    }

    /// No port means MySQL's default, not an error.
    #[test]
    fn a_missing_port_defaults_to_3306() {
        assert_eq!(split_host_port("mysql://root:s@db.internal").unwrap(),
                   ("db.internal".to_string(), 3306));
    }

    /// A password containing '@' must not be mistaken for the host.
    #[test]
    fn an_at_sign_in_the_password_is_handled() {
        assert_eq!(split_host_port("mysql://root:p@ss@db.internal:3306").unwrap(),
                   ("db.internal".to_string(), 3306));
    }

    /// An IPv6 literal's colons are not a port separator.
    #[test]
    fn an_ipv6_host_keeps_its_brackets() {
        assert_eq!(split_host_port("mysql://root:s@[2001:db8::1]:3307").unwrap(),
                   ("[2001:db8::1]".to_string(), 3307));
        assert_eq!(split_host_port("mysql://root:s@[::1]").unwrap(),
                   ("[::1]".to_string(), 3306));
    }

    /// The names a tenant is provisioned under must be derived from its id, so
    /// that renaming a site cannot move its data.
    #[test]
    fn names_come_from_the_tenant_id() {
        let s = store();
        assert_eq!(s.database_name("c32a6ef5d844be317eca55e2").unwrap(),
                   "ghost_c32a6ef5d844be317eca55e2");
        assert!(quote_ident(&s.database_name("c32a6ef5d844be317eca55e2").unwrap()).is_ok());
    }

    /// Calling any of this on a SQLite deployment is a programming error on
    /// `create`, and a no-op on `drop` — a site with no MySQL database to
    /// remove must still be deletable.
    #[tokio::test]
    async fn a_sqlite_deployment_has_nothing_to_drop() {
        assert!(drop(&TenantStore::SqliteFile, "abc").await.is_ok());
        assert!(create(&TenantStore::SqliteFile, "abc").await.is_err());
    }
}
