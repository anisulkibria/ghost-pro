//! DNS hosts a customer has connected to their account.
//!
//! The token itself arrives here already sealed; this module never sees it in
//! the clear. See `pro_core::secret_box` and `migrations/*/0007`.
use pro_core::{ids, now_iso, DbErrorExt, Result};
use sqlx::any::AnyRow;
use sqlx::Row;

use crate::Db;

#[derive(Clone, Debug)]
pub struct DnsProvider {
    pub id: String,
    pub account_id: String,
    /// The provider's slug, e.g. `cloudflare`.
    pub provider: String,
    /// The sealed API token.
    pub secret: String,
    /// The token's last few characters, so the customer can tell which one
    /// is connected without the platform ever showing it.
    pub hint: String,
    /// Zones the token could see when it was last checked.
    pub zones: Vec<String>,
    pub last_used_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl DnsProvider {
    /// Whether this connection reaches `zone`. An empty list means the
    /// provider could not list zones, and the write itself is the test.
    pub fn covers(&self, zone: &str) -> bool {
        self.zones.is_empty() || self.zones.iter().any(|z| z.eq_ignore_ascii_case(zone))
    }
}

fn map(row: &AnyRow) -> DnsProvider {
    let zones: String = row.get("zones");
    DnsProvider {
        id: row.get("id"),
        account_id: row.get("account_id"),
        provider: row.get("provider"),
        secret: row.get("secret"),
        hint: row.get("hint"),
        zones: serde_json::from_str(&zones).unwrap_or_default(),
        last_used_at: row.get("last_used_at"),
        last_error: row.get("last_error"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

const COLUMNS: &str = "id, account_id, provider, secret, hint, zones, last_used_at, last_error, created_at, updated_at";

pub async fn list_for_account(pool: &Db, account_id: &str) -> Result<Vec<DnsProvider>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM dns_providers WHERE account_id = ? ORDER BY provider"
    ))
    .bind(account_id)
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows.iter().map(map).collect())
}

pub async fn get(pool: &Db, account_id: &str, provider: &str) -> Result<Option<DnsProvider>> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM dns_providers WHERE account_id = ? AND provider = ?"
    ))
    .bind(account_id)
    .bind(provider)
    .fetch_optional(pool)
    .await
    .db_err()?;
    Ok(row.as_ref().map(map))
}

/// Connect a provider, or replace the token of one already connected.
pub async fn save(
    pool: &Db,
    account_id: &str,
    provider: &str,
    secret: &str,
    hint: &str,
    zones: &[String],
) -> Result<()> {
    let now = now_iso();
    let zones = serde_json::to_string(zones)?;
    let updated = sqlx::query(
        "UPDATE dns_providers SET secret = ?, hint = ?, zones = ?, last_error = NULL, updated_at = ? \
         WHERE account_id = ? AND provider = ?",
    )
    .bind(secret)
    .bind(hint)
    .bind(&zones)
    .bind(&now)
    .bind(account_id)
    .bind(provider)
    .execute(pool)
    .await
    .db_err()?;
    if updated.rows_affected() > 0 {
        return Ok(());
    }
    let inserted = sqlx::query(
        "INSERT INTO dns_providers (id, account_id, provider, secret, hint, zones, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ids::object_id())
    .bind(account_id)
    .bind(provider)
    .bind(secret)
    .bind(hint)
    .bind(&zones)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await;
    match inserted {
        Ok(_) => Ok(()),
        // Two saves racing: the other one inserted first, so this one updates.
        Err(e) if crate::unique::is_unique_violation(
            &e.to_string(),
            &["dns_providers_account_provider", "dns_providers.account_id"],
        ) =>
        {
            sqlx::query(
                "UPDATE dns_providers SET secret = ?, hint = ?, zones = ?, last_error = NULL, updated_at = ? \
                 WHERE account_id = ? AND provider = ?",
            )
            .bind(secret)
            .bind(hint)
            .bind(&zones)
            .bind(&now)
            .bind(account_id)
            .bind(provider)
            .execute(pool)
            .await
            .db_err()?;
            Ok(())
        }
        Err(e) => Err(pro_core::ProError::Db(e.to_string())),
    }
}

/// Record the outcome of using a connection: `None` for success.
pub async fn record_use(pool: &Db, id: &str, error: Option<&str>) -> Result<()> {
    let now = now_iso();
    let query = match error {
        None => sqlx::query("UPDATE dns_providers SET last_used_at = ?, last_error = NULL WHERE id = ?")
            .bind(now)
            .bind(id),
        Some(error) => sqlx::query("UPDATE dns_providers SET last_error = ? WHERE id = ?")
            .bind(error.chars().take(900).collect::<String>())
            .bind(id),
    };
    query.execute(pool).await.db_err()?;
    Ok(())
}

pub async fn remove(pool: &Db, account_id: &str, provider: &str) -> Result<bool> {
    let done = sqlx::query("DELETE FROM dns_providers WHERE account_id = ? AND provider = ?")
        .bind(account_id)
        .bind(provider)
        .execute(pool)
        .await
        .db_err()?;
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn account(pool: &Db) -> String {
        crate::accounts::create(pool, "dns@example.com", "Dns", "$argon2id$fake").await.unwrap().id
    }

    #[tokio::test]
    async fn connect_replace_and_disconnect() {
        let pool = crate::open_in_memory().await.unwrap();
        let acct = account(&pool).await;
        assert!(list_for_account(&pool, &acct).await.unwrap().is_empty());

        save(&pool, &acct, "cloudflare", "v1:aaa", "…1234", &["example.com".into()]).await.unwrap();
        let p = get(&pool, &acct, "cloudflare").await.unwrap().unwrap();
        assert_eq!(p.hint, "…1234");
        assert!(p.covers("example.com") && !p.covers("other.com"));

        record_use(&pool, &p.id, Some("rejected")).await.unwrap();
        save(&pool, &acct, "cloudflare", "v1:bbb", "…5678", &[]).await.unwrap();
        let all = list_for_account(&pool, &acct).await.unwrap();
        assert_eq!(all.len(), 1, "a second save replaces, never duplicates");
        assert_eq!(all[0].secret, "v1:bbb");
        assert_eq!(all[0].last_error, None, "a new token clears the old failure");
        assert!(all[0].covers("anything.com"));

        assert!(remove(&pool, &acct, "cloudflare").await.unwrap());
        assert!(get(&pool, &acct, "cloudflare").await.unwrap().is_none());
    }
}
