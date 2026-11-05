//! Single-use tokens: email verification, password resets, staff invites.
//!
//! One module for all three because the rules are identical, and a token that
//! can be replayed is the same bug each time. Every one of these is:
//!
//! * **hashed at rest** — the table holds SHA-256, so a database dump is not a
//!   set of live password-reset links;
//! * **dated** — expiry is checked on use, not swept;
//! * **single-use** — consuming is a conditional UPDATE, so two requests
//!   racing on one link produce one success.
use pro_core::{ids, now_iso, DbErrorExt, ProError, Result};
use serde_json::Value;
use crate::Db;
use sqlx::Row;

pub const VERIFY_EMAIL: &str = "verify_email";
pub const RESET_PASSWORD: &str = "reset_password";
pub const STAFF_INVITE: &str = "staff_invite";

/// How long each kind lasts.
///
/// A reset link is short because it is the most valuable: it is a standing
/// offer to take over an account, sitting in an inbox. A verification link is
/// long because the commonest reason one is used late is that somebody did not
/// check their email until tomorrow, and making them start again is pure
/// friction with no security gain.
pub fn ttl_hours(kind: &str) -> i64 {
    match kind {
        RESET_PASSWORD => 2,
        VERIFY_EMAIL => 72,
        STAFF_INVITE => 168,
        _ => 24,
    }
}

/// Issue a token, returning the value to put in the link.
///
/// Returned exactly once; only its hash is stored.
pub async fn issue(
    pool: &Db,
    kind: &str,
    subject_type: &str,
    subject_id: &str,
    meta: Value,
) -> Result<String> {
    let token = ids::secret_token();
    sqlx::query(
        "INSERT INTO tokens (id, kind, subject_type, subject_id, token_hash, expires_at, \
         meta, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ids::object_id())
    .bind(kind)
    .bind(subject_type)
    .bind(subject_id)
    .bind(ids::sha256_hex(&token))
    .bind(
        (chrono::Utc::now() + chrono::Duration::hours(ttl_hours(kind)))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    )
    .bind(serde_json::to_string(&meta)?)
    .bind(now_iso())
    .execute(pool)
    .await
    .db_err()?;
    Ok(token)
}

/// Replace any outstanding tokens of this kind for this subject, then issue a
/// fresh one.
///
/// This is what a "resend the link" button should call. Leaving the old ones
/// live would mean several working reset links per account, each an
/// independent chance of being intercepted — and the user only ever uses the
/// newest.
pub async fn reissue(
    pool: &Db,
    kind: &str,
    subject_type: &str,
    subject_id: &str,
    meta: Value,
) -> Result<String> {
    sqlx::query(
        "DELETE FROM tokens WHERE kind = ? AND subject_type = ? AND subject_id = ? \
         AND used_at IS NULL",
    )
    .bind(kind)
    .bind(subject_type)
    .bind(subject_id)
    .execute(pool)
    .await
    .db_err()?;
    issue(pool, kind, subject_type, subject_id, meta).await
}

/// What a consumed token authorised.
#[derive(Debug, Clone)]
pub struct Claim {
    pub subject_type: String,
    pub subject_id: String,
    pub meta: Value,
}

/// Spend a token, once.
///
/// The `used_at IS NULL` in the WHERE clause is the whole mechanism: two
/// requests arriving with the same link produce one UPDATE that matches and
/// one that does not. Checking first and updating after would let both
/// through, which for a reset link means two people setting the password.
pub async fn consume(pool: &Db, kind: &str, token: &str) -> Result<Claim> {
    if token.is_empty() {
        return Err(ProError::BadRequest(expired_message(kind)));
    }
    let hash = ids::sha256_hex(token);

    let row = sqlx::query(
        "SELECT id, subject_type, subject_id, meta FROM tokens \
         WHERE token_hash = ? AND kind = ? AND used_at IS NULL AND expires_at > ?",
    )
    .bind(&hash)
    .bind(kind)
    .bind(now_iso())
    .fetch_optional(pool)
    .await
    .db_err()?
    .ok_or_else(|| ProError::BadRequest(expired_message(kind)))?;

    let id: String = row.get("id");
    let spent = sqlx::query("UPDATE tokens SET used_at = ? WHERE id = ? AND used_at IS NULL")
        .bind(now_iso())
        .bind(&id)
        .execute(pool)
        .await
        .db_err()?;
    if spent.rows_affected() != 1 {
        // Another request spent it between the select and the update.
        return Err(ProError::BadRequest(expired_message(kind)));
    }

    Ok(Claim {
        subject_type: row.get("subject_type"),
        subject_id: row.get("subject_id"),
        meta: serde_json::from_str(&row.get::<String, _>("meta")).unwrap_or(Value::Null),
    })
}

/// One message for every way a token can be unusable — wrong, spent, expired,
/// or never issued.
///
/// Distinguishing them would tell somebody holding a stale link whether it was
/// ever real, which is a small oracle but a free one to close. It also happens
/// to be the more useful message: in all four cases the answer is the same.
fn expired_message(kind: &str) -> String {
    match kind {
        RESET_PASSWORD => "This password reset link has expired or already been used. \
                           Please request a new one."
            .into(),
        VERIFY_EMAIL => "This confirmation link has expired or already been used. \
                         Please request a new one."
            .into(),
        STAFF_INVITE => "This invitation has expired or already been accepted.".into(),
        _ => "This link is no longer valid.".into(),
    }
}

/// Whether a subject has an unspent token of this kind, for a "we have already
/// emailed you" message.
pub async fn has_outstanding(
    pool: &Db,
    kind: &str,
    subject_type: &str,
    subject_id: &str,
) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tokens WHERE kind = ? AND subject_type = ? AND subject_id = ? \
         AND used_at IS NULL AND expires_at > ?",
    )
    .bind(kind)
    .bind(subject_type)
    .bind(subject_id)
    .bind(now_iso())
    .fetch_one(pool)
    .await
    .db_err()?;
    Ok(count > 0)
}

/// Drop spent and expired rows. Nothing depends on this for correctness.
pub async fn purge(pool: &Db) -> Result<u64> {
    let r = sqlx::query("DELETE FROM tokens WHERE used_at IS NOT NULL OR expires_at <= ?")
        .bind(now_iso())
        .execute(pool)
        .await
        .db_err()?;
    Ok(r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn pool() -> Db {
        crate::pool::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn issues_and_consumes() {
        let pool = pool().await;
        let token = issue(&pool, VERIFY_EMAIL, "account", "a1", json!({"email": "x@y.z"}))
            .await
            .unwrap();
        let claim = consume(&pool, VERIFY_EMAIL, &token).await.unwrap();
        assert_eq!(claim.subject_type, "account");
        assert_eq!(claim.subject_id, "a1");
        assert_eq!(claim.meta["email"], "x@y.z");
    }

    /// Only the hash is stored, so a database dump is not a set of live reset
    /// links.
    #[tokio::test]
    async fn stores_only_a_hash() {
        let pool = pool().await;
        let token = issue(&pool, RESET_PASSWORD, "account", "a1", json!({})).await.unwrap();
        let stored: String =
            sqlx::query_scalar("SELECT token_hash FROM tokens").fetch_one(&pool).await.unwrap();
        assert_ne!(stored, token);
        assert_eq!(stored, ids::sha256_hex(&token));
    }

    /// The point of the module: a link works once. Two people clicking the
    /// same reset link must not both set the password.
    #[tokio::test]
    async fn a_token_is_spent_once() {
        let pool = pool().await;
        let token = issue(&pool, RESET_PASSWORD, "account", "a1", json!({})).await.unwrap();
        assert!(consume(&pool, RESET_PASSWORD, &token).await.is_ok());
        assert!(consume(&pool, RESET_PASSWORD, &token).await.is_err(), "already spent");
    }

    /// A token is bound to its purpose: a verification link must not reset a
    /// password.
    #[tokio::test]
    async fn a_token_is_bound_to_its_kind() {
        let pool = pool().await;
        let token = issue(&pool, VERIFY_EMAIL, "account", "a1", json!({})).await.unwrap();
        assert!(consume(&pool, RESET_PASSWORD, &token).await.is_err());
        assert!(consume(&pool, VERIFY_EMAIL, &token).await.is_ok(), "still unspent");
    }

    #[tokio::test]
    async fn an_expired_token_is_refused() {
        let pool = pool().await;
        let token = issue(&pool, RESET_PASSWORD, "account", "a1", json!({})).await.unwrap();
        sqlx::query("UPDATE tokens SET expires_at = ?")
            .bind(pro_core::days_from_now(-1))
            .execute(&pool)
            .await
            .unwrap();
        assert!(consume(&pool, RESET_PASSWORD, &token).await.is_err());
    }

    #[tokio::test]
    async fn an_unknown_or_empty_token_is_refused() {
        let pool = pool().await;
        assert!(consume(&pool, VERIFY_EMAIL, "").await.is_err());
        assert!(consume(&pool, VERIFY_EMAIL, &ids::secret_token()).await.is_err());
    }

    /// Every failure mode reads the same, so a stale link does not reveal
    /// whether it was ever real.
    #[tokio::test]
    async fn failures_are_indistinguishable() {
        let pool = pool().await;
        let token = issue(&pool, RESET_PASSWORD, "account", "a1", json!({})).await.unwrap();
        consume(&pool, RESET_PASSWORD, &token).await.unwrap();

        let spent = consume(&pool, RESET_PASSWORD, &token).await.unwrap_err();
        let never = consume(&pool, RESET_PASSWORD, &ids::secret_token()).await.unwrap_err();
        assert_eq!(spent.to_string(), never.to_string());
    }

    /// "Resend" must invalidate the previous link, not add a second working
    /// one.
    #[tokio::test]
    async fn reissuing_invalidates_the_previous_link() {
        let pool = pool().await;
        let first = issue(&pool, RESET_PASSWORD, "account", "a1", json!({})).await.unwrap();
        let second = reissue(&pool, RESET_PASSWORD, "account", "a1", json!({})).await.unwrap();

        assert!(consume(&pool, RESET_PASSWORD, &first).await.is_err(), "superseded");
        assert!(consume(&pool, RESET_PASSWORD, &second).await.is_ok());
    }

    /// Reissuing one subject's token must not touch anybody else's.
    #[tokio::test]
    async fn reissuing_is_scoped_to_one_subject() {
        let pool = pool().await;
        let other = issue(&pool, RESET_PASSWORD, "account", "a2", json!({})).await.unwrap();
        reissue(&pool, RESET_PASSWORD, "account", "a1", json!({})).await.unwrap();
        assert!(consume(&pool, RESET_PASSWORD, &other).await.is_ok());
    }

    #[tokio::test]
    async fn reports_an_outstanding_token() {
        let pool = pool().await;
        assert!(!has_outstanding(&pool, VERIFY_EMAIL, "account", "a1").await.unwrap());
        let token = issue(&pool, VERIFY_EMAIL, "account", "a1", json!({})).await.unwrap();
        assert!(has_outstanding(&pool, VERIFY_EMAIL, "account", "a1").await.unwrap());
        consume(&pool, VERIFY_EMAIL, &token).await.unwrap();
        assert!(!has_outstanding(&pool, VERIFY_EMAIL, "account", "a1").await.unwrap());
    }

    /// A reset link is the most valuable of the three and lasts the least
    /// time.
    #[test]
    fn reset_links_are_the_shortest_lived() {
        assert!(ttl_hours(RESET_PASSWORD) < ttl_hours(VERIFY_EMAIL));
        assert!(ttl_hours(VERIFY_EMAIL) < ttl_hours(STAFF_INVITE));
    }

    #[tokio::test]
    async fn purges_spent_and_expired() {
        let pool = pool().await;
        let spent = issue(&pool, VERIFY_EMAIL, "account", "a1", json!({})).await.unwrap();
        consume(&pool, VERIFY_EMAIL, &spent).await.unwrap();
        issue(&pool, VERIFY_EMAIL, "account", "a2", json!({})).await.unwrap();

        assert_eq!(purge(&pool).await.unwrap(), 1);
        assert!(has_outstanding(&pool, VERIFY_EMAIL, "account", "a2").await.unwrap());
    }
}
