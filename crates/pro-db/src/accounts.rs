//! Customer accounts, and the password handling that goes with them.
use pro_core::{ids, looks_like_email, now_iso, Account, DbErrorExt, ProError, Result};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// bcrypt cost.
///
/// 12 is roughly a quarter-second per hash on current hardware. That is slow
/// enough to make offline cracking of a stolen hash expensive and fast enough
/// that a login does not feel broken. The cost is stored inside the hash, so
/// raising it later rehashes gradually as people sign in rather than
/// invalidating every password at once.
pub const BCRYPT_COST: u32 = 12;

pub fn hash_password(password: &str) -> Result<String> {
    // bcrypt silently truncates at 72 bytes, so a longer passphrase would
    // have its tail ignored — surprising, and worth refusing rather than
    // quietly accepting a weaker password than the user typed.
    if password.len() > 72 {
        return Err(ProError::BadRequest(
            "Password must be 72 characters or fewer.".into(),
        ));
    }
    bcrypt::hash(password, BCRYPT_COST).map_err(|e| ProError::internal(e))
}

/// Check a password against a stored hash.
///
/// A malformed hash verifies as `false` rather than erroring: that is a
/// corrupt row, and the right behaviour is to refuse the login, not to hand
/// the caller a 500 that distinguishes this account from any other.
pub fn verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

fn map(row: &SqliteRow) -> Account {
    Account {
        id: row.get("id"),
        email: row.get("email"),
        name: row.get("name"),
        password_hash: row.get("password_hash"),
        status: row.get("status"),
        email_verified_at: row.get("email_verified_at"),
        billing_provider: row.get("billing_provider"),
        billing_customer_id: row.get("billing_customer_id"),
        created_at: row.get("created_at"),
        last_login_at: row.get("last_login_at"),
    }
}

const COLS: &str = "id, email, name, password_hash, status, email_verified_at, \
                    billing_provider, billing_customer_id, created_at, last_login_at";

pub async fn create(
    pool: &SqlitePool,
    email: &str,
    name: &str,
    password_hash: &str,
) -> Result<Account> {
    let email = email.trim().to_lowercase();
    if !looks_like_email(&email) {
        return Err(ProError::BadRequest("That does not look like an email address.".into()));
    }
    let id = ids::object_id();
    let now = now_iso();
    let result = sqlx::query(
        "INSERT INTO accounts (id, email, name, password_hash, status, created_at) \
         VALUES (?, ?, ?, ?, 'active', ?)",
    )
    .bind(&id)
    .bind(&email)
    .bind(name.trim())
    .bind(password_hash)
    .bind(&now)
    .execute(pool)
    .await;

    if let Err(e) = result {
        let msg = e.to_string();
        if crate::unique::is_unique_violation(&msg, &["accounts_email_unique", "accounts.email"]) {
            // Phrased so it does not confirm whether an address is registered
            // to somebody who is only guessing — the signup form shows this
            // and offers a sign-in link.
            return Err(ProError::Conflict(
                "An account with that email already exists.".into(),
            ));
        }
        return Err(ProError::Db(msg));
    }
    get(pool, &id).await
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Account> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM accounts WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .db_err()?
        .ok_or_else(|| ProError::NotFound(format!("no account {id}")))?;
    Ok(map(&row))
}

pub async fn find_by_email(pool: &SqlitePool, email: &str) -> Result<Option<Account>> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM accounts WHERE lower(email) = lower(?)"))
        .bind(email.trim())
        .fetch_optional(pool)
        .await
        .db_err()?;
    Ok(row.as_ref().map(map))
}

/// Find the account a provider's customer id belongs to.
///
/// Scoped by provider because two providers can mint the same id, and a
/// webhook that resolved to the wrong account would move somebody else's
/// subscription.
pub async fn find_by_customer(
    pool: &SqlitePool,
    provider: &str,
    customer_id: &str,
) -> Result<Option<Account>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM accounts WHERE billing_provider = ? AND billing_customer_id = ?"
    ))
    .bind(provider)
    .bind(customer_id)
    .fetch_optional(pool)
    .await
    .db_err()?;
    Ok(row.as_ref().map(map))
}

/// Authenticate, in a way that does not leak which accounts exist.
///
/// An unknown address still costs a bcrypt verification against a dummy hash.
/// Without that, "no such account" returns in microseconds and a real account
/// with a wrong password takes a quarter-second — which is a reliable oracle
/// for enumerating a platform's customers.
pub async fn authenticate(pool: &SqlitePool, email: &str, password: &str) -> Result<Account> {
    // bcrypt hash of a value nothing can match, at the same cost as a real one.
    const DUMMY: &str = "$2b$12$C6UzMDM.H6dfI/f/IKcEeO3Wz1W4J1mP6RhJ0BCGcmJjK0KjCIWxK";

    let found = find_by_email(pool, email).await?;
    let hash = found.as_ref().map(|a| a.password_hash.as_str()).unwrap_or(DUMMY);
    let ok = verify_password(password, hash);

    match found {
        Some(account) if ok => {
            if !account.is_active() {
                return Err(ProError::Forbidden(
                    "This account has been suspended. Please contact support.".into(),
                ));
            }
            touch_login(pool, &account.id).await?;
            Ok(account)
        }
        // The same message either way, for the same reason.
        _ => Err(ProError::Unauthorized("Email or password is incorrect.".into())),
    }
}

pub async fn touch_login(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("UPDATE accounts SET last_login_at = ? WHERE id = ?")
        .bind(now_iso())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

pub async fn mark_verified(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("UPDATE accounts SET email_verified_at = ? WHERE id = ? AND email_verified_at IS NULL")
        .bind(now_iso())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

pub async fn set_password(pool: &SqlitePool, id: &str, password_hash: &str) -> Result<()> {
    sqlx::query("UPDATE accounts SET password_hash = ? WHERE id = ?")
        .bind(password_hash)
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// Record which provider this customer pays through and their id there.
pub async fn set_billing_customer(
    pool: &SqlitePool,
    id: &str,
    provider: &str,
    customer_id: &str,
) -> Result<()> {
    sqlx::query("UPDATE accounts SET billing_provider = ?, billing_customer_id = ? WHERE id = ?")
        .bind(provider)
        .bind(customer_id)
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

pub async fn set_status(pool: &SqlitePool, id: &str, status: &str) -> Result<()> {
    sqlx::query("UPDATE accounts SET status = ? WHERE id = ?")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// Change the account's email, marking it unverified.
///
/// Unverified on purpose: the new address has done nothing to prove it
/// belongs to this account, and the dashboard's verification gate is what
/// makes an unproven address a bump rather than a hole.
pub async fn set_email(pool: &SqlitePool, id: &str, email: &str) -> Result<()> {
    sqlx::query(
        "UPDATE accounts SET email = ?, email_verified_at = NULL, updated_at = ? WHERE id = ?",
    )
    .bind(email)
    .bind(pro_core::now_iso())
    .bind(id)
    .execute(pool)
    .await
    .map_err(|e| {
        // The unique index is the only expected failure, and it deserves a
        // sentence rather than a driver error.
        if e.to_string().contains("accounts_email_unique") {
            ProError::Conflict("That email is already in use.".into())
        } else {
            ProError::Db(e.to_string())
        }
    })?;
    Ok(())
}

pub async fn rename(pool: &SqlitePool, id: &str, name: &str) -> Result<()> {
    sqlx::query("UPDATE accounts SET name = ? WHERE id = ?")
        .bind(name.trim())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// Accounts for the operator's console.
pub async fn list(
    pool: &SqlitePool,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Account>> {
    let mut sql = format!("SELECT {COLS} FROM accounts WHERE 1=1");
    if search.is_some() {
        sql.push_str(" AND (email LIKE ? OR name LIKE ?)");
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ? OFFSET ?");
    let mut q = sqlx::query(&sql);
    if let Some(term) = search {
        let like = format!("%{term}%");
        q = q.bind(like.clone()).bind(like);
    }
    let rows = q.bind(limit).bind(offset).fetch_all(pool).await.db_err()?;
    Ok(rows.iter().map(map).collect())
}

pub async fn count(pool: &SqlitePool) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM accounts").fetch_one(pool).await.db_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::pool::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn creates_and_finds() {
        let pool = pool().await;
        let a = create(&pool, "Someone@Example.COM", " Someone ", "h").await.unwrap();
        assert_eq!(a.email, "someone@example.com", "normalised for storage");
        assert_eq!(a.name, "Someone", "trimmed");
        assert!(!a.is_verified());
        assert_eq!(find_by_email(&pool, "SOMEONE@example.com").await.unwrap().unwrap().id, a.id);
        assert_eq!(get(&pool, &a.id).await.unwrap().email, a.email);
    }

    #[tokio::test]
    async fn refuses_a_duplicate_email() {
        let pool = pool().await;
        create(&pool, "dup@example.com", "A", "h").await.unwrap();
        let err = create(&pool, "DUP@example.com", "B", "h").await.unwrap_err();
        assert_eq!(err.status(), 409);
    }

    #[tokio::test]
    async fn refuses_a_non_address() {
        let pool = pool().await;
        assert_eq!(create(&pool, "not-an-email", "A", "h").await.unwrap_err().status(), 400);
    }

    #[tokio::test]
    async fn round_trips_a_password() {
        let hash = hash_password("correct horse battery").unwrap();
        assert!(verify_password("correct horse battery", &hash));
        assert!(!verify_password("wrong", &hash));
    }

    /// bcrypt truncates at 72 bytes, so a longer password must be refused
    /// rather than silently shortened.
    #[tokio::test]
    async fn refuses_an_overlong_password() {
        assert!(hash_password(&"a".repeat(73)).is_err());
        assert!(hash_password(&"a".repeat(72)).is_ok());
    }

    /// A corrupt hash refuses the login instead of erroring, which would
    /// distinguish that account from every other.
    #[tokio::test]
    async fn a_corrupt_hash_just_fails() {
        assert!(!verify_password("anything", "not-a-bcrypt-hash"));
    }

    #[tokio::test]
    async fn authenticates() {
        let pool = pool().await;
        let hash = hash_password("s3cret").unwrap();
        let a = create(&pool, "login@example.com", "L", &hash).await.unwrap();

        let signed_in = authenticate(&pool, "login@example.com", "s3cret").await.unwrap();
        assert_eq!(signed_in.id, a.id);
        assert!(get(&pool, &a.id).await.unwrap().last_login_at.is_some());

        assert_eq!(
            authenticate(&pool, "login@example.com", "wrong").await.unwrap_err().status(),
            401
        );
    }

    /// An unknown address and a wrong password must be indistinguishable —
    /// same status and same message — or the platform's customer list can be
    /// enumerated.
    #[tokio::test]
    async fn does_not_reveal_whether_an_account_exists() {
        let pool = pool().await;
        let hash = hash_password("s3cret").unwrap();
        create(&pool, "real@example.com", "R", &hash).await.unwrap();

        let wrong_password = authenticate(&pool, "real@example.com", "nope").await.unwrap_err();
        let no_account = authenticate(&pool, "ghost@example.com", "nope").await.unwrap_err();
        assert_eq!(wrong_password.status(), no_account.status());
        assert_eq!(wrong_password.to_string(), no_account.to_string());
    }

    #[tokio::test]
    async fn a_blocked_account_cannot_sign_in() {
        let pool = pool().await;
        let hash = hash_password("s3cret").unwrap();
        let a = create(&pool, "blocked@example.com", "B", &hash).await.unwrap();
        set_status(&pool, &a.id, "blocked").await.unwrap();
        assert_eq!(
            authenticate(&pool, "blocked@example.com", "s3cret").await.unwrap_err().status(),
            403
        );
    }

    #[tokio::test]
    async fn verification_is_idempotent() {
        let pool = pool().await;
        let a = create(&pool, "v@example.com", "V", "h").await.unwrap();
        mark_verified(&pool, &a.id).await.unwrap();
        let first = get(&pool, &a.id).await.unwrap().email_verified_at;
        assert!(first.is_some());
        mark_verified(&pool, &a.id).await.unwrap();
        assert_eq!(get(&pool, &a.id).await.unwrap().email_verified_at, first, "not moved");
    }

    /// A customer id is only meaningful alongside the provider that issued it.
    #[tokio::test]
    async fn resolves_a_customer_per_provider() {
        let pool = pool().await;
        let a = create(&pool, "c@example.com", "C", "h").await.unwrap();
        set_billing_customer(&pool, &a.id, "stripe", "cus_1").await.unwrap();

        assert_eq!(find_by_customer(&pool, "stripe", "cus_1").await.unwrap().unwrap().id, a.id);
        assert!(
            find_by_customer(&pool, "paddle", "cus_1").await.unwrap().is_none(),
            "the same id at another provider is a different customer"
        );
    }

    #[tokio::test]
    async fn lists_and_counts() {
        let pool = pool().await;
        create(&pool, "one@example.com", "One", "h").await.unwrap();
        create(&pool, "two@example.com", "Two", "h").await.unwrap();
        assert_eq!(count(&pool).await.unwrap(), 2);
        assert_eq!(list(&pool, None, 50, 0).await.unwrap().len(), 2);
        assert_eq!(list(&pool, Some("two"), 50, 0).await.unwrap().len(), 1);
    }

    /// The hash must never be serialised into an API response.
    #[tokio::test]
    async fn password_hash_is_not_serialized() {
        let pool = pool().await;
        let a = create(&pool, "j@example.com", "J", "secret-hash").await.unwrap();
        let json = serde_json::to_string(&a).unwrap();
        assert!(!json.contains("secret-hash"), "got {json}");
    }
}
