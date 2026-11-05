//! Sessions for both populations, and the separation between them.
//!
//! The platform has two completely different sets of people signed in:
//!
//! * **accounts** — customers, at `/dashboard`. They install sites, attach
//!   domains, pay.
//! * **staff** — the operator and their colleagues, at `/console`. They
//!   choose the payment provider, see every tenant, suspend things.
//!
//! These are kept apart by three separate mechanisms, because any one of them
//! failing on its own should not be enough to cross the boundary:
//!
//! 1. different cookie *names* ([`ACCOUNT_COOKIE`], [`STAFF_COOKIE`]), so a
//!    browser signed into both never sends one where the other is expected;
//! 2. `subject_type` stored on the row and **required** by the lookup, so a
//!    stolen or guessed customer token presented to the console does not
//!    resolve;
//! 3. a role check on top, for staff.
//!
//! The second is the one that matters. A single `find_by_token` that returned
//! whatever subject owned the token would make the cookie name the only thing
//! standing between a customer and the console — and a cookie name is not a
//! security boundary, it is a label.
use pro_core::{ids, now_iso, DbErrorExt, ProError, Result};
use sqlx::{Row, SqlitePool};

/// Customer session cookie. `__Host-` locks it to this exact host over HTTPS
/// with no `Domain` attribute, which stops a compromised tenant subdomain
/// from writing a cookie the dashboard would read.
pub const ACCOUNT_COOKIE: &str = "__Host-pro_session";
/// Operator session cookie — a different name, so the two never collide.
pub const STAFF_COOKIE: &str = "__Host-pro_staff";

/// Cookie names used when the platform is served over plain HTTP.
///
/// The `__Host-` prefix is *ignored by the browser* unless the cookie is
/// `Secure`, which it cannot be on HTTP — so in local development the prefixed
/// name would be set and then silently dropped, and nobody could sign in.
/// These are the unprefixed fallbacks for that case only.
pub const ACCOUNT_COOKIE_INSECURE: &str = "pro_session";
pub const STAFF_COOKIE_INSECURE: &str = "pro_staff";

pub fn account_cookie_name(secure: bool) -> &'static str {
    if secure {
        ACCOUNT_COOKIE
    } else {
        ACCOUNT_COOKIE_INSECURE
    }
}

pub fn staff_cookie_name(secure: bool) -> &'static str {
    if secure {
        STAFF_COOKIE
    } else {
        STAFF_COOKIE_INSECURE
    }
}

/// Which population a session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Account,
    Staff,
}

impl Subject {
    pub fn as_str(self) -> &'static str {
        match self {
            Subject::Account => "account",
            Subject::Staff => "staff",
        }
    }
}

/// How long a session lasts. Customers get a long one because a publication
/// is something you come back to; staff get a short one because the console
/// can change how the platform takes money.
pub const ACCOUNT_TTL_DAYS: i64 = 30;
pub const STAFF_TTL_DAYS: i64 = 7;

/// Start a session and return the token to put in the cookie.
///
/// The token is returned exactly once and never stored — only its SHA-256 is.
/// A dump of the sessions table is therefore a list of useless hashes rather
/// than a set of live logins.
pub async fn create(
    pool: &SqlitePool,
    subject: Subject,
    subject_id: &str,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<String> {
    let token = ids::secret_token();
    let ttl = match subject {
        Subject::Account => ACCOUNT_TTL_DAYS,
        Subject::Staff => STAFF_TTL_DAYS,
    };
    sqlx::query(
        "INSERT INTO sessions (id, subject_type, subject_id, token_hash, expires_at, \
         created_at, last_seen_at, ip, user_agent) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ids::object_id())
    .bind(subject.as_str())
    .bind(subject_id)
    .bind(ids::sha256_hex(&token))
    .bind(pro_core::days_from_now(ttl))
    .bind(now_iso())
    .bind(now_iso())
    .bind(ip)
    // Truncated: a header is attacker-controlled and unbounded, and this
    // column exists to help somebody recognise their own session, not to
    // archive whatever was sent.
    .bind(user_agent.map(|ua| ua.chars().take(255).collect::<String>()))
    .execute(pool)
    .await
    .db_err()?;
    Ok(token)
}

/// Resolve a token to the id it authenticates, **within one population**.
///
/// `subject` is not a hint — it is part of the lookup. A customer's token
/// presented to the console finds nothing, even though the row exists.
pub async fn resolve(pool: &SqlitePool, subject: Subject, token: &str) -> Result<String> {
    if token.is_empty() {
        return Err(ProError::Unauthorized("Not signed in.".into()));
    }
    let row = sqlx::query(
        "SELECT id, subject_id FROM sessions \
         WHERE token_hash = ? AND subject_type = ? AND expires_at > ?",
    )
    .bind(ids::sha256_hex(token))
    .bind(subject.as_str())
    .bind(now_iso())
    .fetch_optional(pool)
    .await
    .db_err()?
    .ok_or_else(|| ProError::Unauthorized("Your session has expired. Please sign in again.".into()))?;

    let id: String = row.get("id");
    let subject_id: String = row.get("subject_id");

    // Best-effort liveness stamp. A failure here must not fail the request —
    // the session is valid either way, and this column is only used to show
    // somebody their own active sessions.
    let _ = sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
        .bind(now_iso())
        .bind(&id)
        .execute(pool)
        .await;

    Ok(subject_id)
}

/// End one session — signing out.
pub async fn destroy(pool: &SqlitePool, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(ids::sha256_hex(token))
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// End every session for one subject.
///
/// Called on a password change and when an account is blocked — a password
/// reset that left old sessions alive would not actually lock anybody out,
/// which is the entire point of resetting it.
pub async fn destroy_all(pool: &SqlitePool, subject: Subject, subject_id: &str) -> Result<u64> {
    let r = sqlx::query("DELETE FROM sessions WHERE subject_type = ? AND subject_id = ?")
        .bind(subject.as_str())
        .bind(subject_id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(r.rows_affected())
}

/// Delete expired rows. Run on a timer; nothing depends on it for
/// correctness, since every lookup already filters on `expires_at`.
pub async fn purge_expired(pool: &SqlitePool) -> Result<u64> {
    let r = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now_iso())
        .execute(pool)
        .await
        .db_err()?;
    Ok(r.rows_affected())
}

/// A session as shown to its owner on a "where am I signed in" screen.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

pub async fn list_for(
    pool: &SqlitePool,
    subject: Subject,
    subject_id: &str,
) -> Result<Vec<SessionInfo>> {
    let rows = sqlx::query(
        "SELECT id, created_at, last_seen_at, ip, user_agent FROM sessions \
         WHERE subject_type = ? AND subject_id = ? AND expires_at > ? \
         ORDER BY last_seen_at DESC",
    )
    .bind(subject.as_str())
    .bind(subject_id)
    .bind(now_iso())
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows
        .iter()
        .map(|r| SessionInfo {
            id: r.get("id"),
            created_at: r.get("created_at"),
            last_seen_at: r.get("last_seen_at"),
            ip: r.get("ip"),
            user_agent: r.get("user_agent"),
        })
        .collect())
}

/// Revoke one of a subject's own sessions by id.
///
/// Scoped by subject so a caller can only ever revoke their own — passing
/// somebody else's session id finds nothing.
pub async fn revoke(
    pool: &SqlitePool,
    subject: Subject,
    subject_id: &str,
    session_id: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE id = ? AND subject_type = ? AND subject_id = ?")
        .bind(session_id)
        .bind(subject.as_str())
        .bind(subject_id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::pool::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn round_trips_a_session() {
        let pool = pool().await;
        let token = create(&pool, Subject::Account, "acc1", Some("1.2.3.4"), Some("Firefox"))
            .await
            .unwrap();
        assert_eq!(resolve(&pool, Subject::Account, &token).await.unwrap(), "acc1");
    }

    /// The token is never stored in the clear, so a leaked database is not a
    /// set of live logins.
    #[tokio::test]
    async fn stores_only_a_hash() {
        let pool = pool().await;
        let token = create(&pool, Subject::Account, "acc1", None, None).await.unwrap();
        let stored: String = sqlx::query_scalar("SELECT token_hash FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_ne!(stored, token);
        assert_eq!(stored, ids::sha256_hex(&token));
    }

    /// The crux of the whole module: a customer's session must not open the
    /// operator console, even with the right token.
    #[tokio::test]
    async fn a_customer_token_does_not_resolve_as_staff() {
        let pool = pool().await;
        let token = create(&pool, Subject::Account, "acc1", None, None).await.unwrap();
        let err = resolve(&pool, Subject::Staff, &token).await.unwrap_err();
        assert_eq!(err.status(), 401);
    }

    /// And the reverse, so an operator's token is not a customer's either.
    #[tokio::test]
    async fn a_staff_token_does_not_resolve_as_an_account() {
        let pool = pool().await;
        let token = create(&pool, Subject::Staff, "staff1", None, None).await.unwrap();
        assert!(resolve(&pool, Subject::Account, &token).await.is_err());
        assert_eq!(resolve(&pool, Subject::Staff, &token).await.unwrap(), "staff1");
    }

    /// The two populations use different cookie names, so a browser signed
    /// into both never sends one where the other is read.
    #[tokio::test]
    async fn cookie_names_do_not_collide() {
        assert_ne!(ACCOUNT_COOKIE, STAFF_COOKIE);
        assert_ne!(account_cookie_name(true), staff_cookie_name(true));
        assert_ne!(account_cookie_name(false), staff_cookie_name(false));
        // `__Host-` is ignored by browsers without Secure, so HTTP gets the
        // unprefixed names or nobody can sign in locally.
        assert!(account_cookie_name(true).starts_with("__Host-"));
        assert!(!account_cookie_name(false).starts_with("__Host-"));
    }

    #[tokio::test]
    async fn an_expired_session_is_rejected() {
        let pool = pool().await;
        let token = create(&pool, Subject::Account, "acc1", None, None).await.unwrap();
        sqlx::query("UPDATE sessions SET expires_at = ?")
            .bind(pro_core::days_from_now(-1))
            .execute(&pool)
            .await
            .unwrap();
        assert!(resolve(&pool, Subject::Account, &token).await.is_err());

        assert_eq!(purge_expired(&pool).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn an_unknown_or_empty_token_is_rejected() {
        let pool = pool().await;
        assert!(resolve(&pool, Subject::Account, "").await.is_err());
        assert!(resolve(&pool, Subject::Account, &ids::secret_token()).await.is_err());
    }

    #[tokio::test]
    async fn signing_out_ends_that_session_only() {
        let pool = pool().await;
        let a = create(&pool, Subject::Account, "acc1", None, None).await.unwrap();
        let b = create(&pool, Subject::Account, "acc1", None, None).await.unwrap();
        destroy(&pool, &a).await.unwrap();
        assert!(resolve(&pool, Subject::Account, &a).await.is_err());
        assert!(resolve(&pool, Subject::Account, &b).await.is_ok(), "the other device stays in");
    }

    /// A password reset that left old sessions alive would not lock anybody
    /// out, which is the entire point of resetting it.
    #[tokio::test]
    async fn destroying_all_locks_every_device_out() {
        let pool = pool().await;
        let a = create(&pool, Subject::Account, "acc1", None, None).await.unwrap();
        let b = create(&pool, Subject::Account, "acc1", None, None).await.unwrap();
        let other = create(&pool, Subject::Account, "acc2", None, None).await.unwrap();

        assert_eq!(destroy_all(&pool, Subject::Account, "acc1").await.unwrap(), 2);
        assert!(resolve(&pool, Subject::Account, &a).await.is_err());
        assert!(resolve(&pool, Subject::Account, &b).await.is_err());
        assert!(resolve(&pool, Subject::Account, &other).await.is_ok(), "a different account");
    }

    /// Revoking is scoped to the owner, so one customer cannot end another's
    /// session by guessing its id.
    #[tokio::test]
    async fn revoking_is_scoped_to_the_owner() {
        let pool = pool().await;
        create(&pool, Subject::Account, "victim", None, None).await.unwrap();
        let victim_session = list_for(&pool, Subject::Account, "victim").await.unwrap();
        let id = &victim_session[0].id;

        revoke(&pool, Subject::Account, "attacker", id).await.unwrap();
        assert_eq!(
            list_for(&pool, Subject::Account, "victim").await.unwrap().len(),
            1,
            "another account's revoke must not touch it"
        );

        revoke(&pool, Subject::Account, "victim", id).await.unwrap();
        assert!(list_for(&pool, Subject::Account, "victim").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn lists_sessions_for_their_owner() {
        let pool = pool().await;
        create(&pool, Subject::Account, "acc1", Some("10.0.0.1"), Some("Safari")).await.unwrap();
        let list = list_for(&pool, Subject::Account, "acc1").await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(list[0].user_agent.as_deref(), Some("Safari"));
        // Staff sessions are a separate population and must not appear here.
        create(&pool, Subject::Staff, "acc1", None, None).await.unwrap();
        assert_eq!(list_for(&pool, Subject::Account, "acc1").await.unwrap().len(), 1);
    }

    /// A user-agent header is unbounded and attacker-controlled.
    #[tokio::test]
    async fn truncates_a_huge_user_agent() {
        let pool = pool().await;
        create(&pool, Subject::Account, "acc1", None, Some(&"x".repeat(10_000))).await.unwrap();
        let ua = list_for(&pool, Subject::Account, "acc1").await.unwrap()[0]
            .user_agent
            .clone()
            .unwrap();
        assert_eq!(ua.len(), 255);
    }

    /// Staff sessions are shorter-lived, because the console can change how
    /// the platform takes money.
    #[tokio::test]
    async fn staff_sessions_are_shorter() {
        assert!(STAFF_TTL_DAYS < ACCOUNT_TTL_DAYS);
    }
}
