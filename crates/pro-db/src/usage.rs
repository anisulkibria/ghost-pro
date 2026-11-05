//! Sampled per-tenant usage.
//!
//! Sampled rather than computed on demand, because the honest answer to "how
//! many members does this tenant have" requires opening that tenant's own
//! SQLite file — and doing that for every tenant when a dashboard page loads
//! would be unusable at any real number of sites. A background job takes a
//! reading per tenant per day; the dashboard reads the samples.
//!
//! The consequence is that these figures are *as of* a time, and the UI says
//! so. A storage quota enforced from a day-old sample can be exceeded by a day
//! of uploads, which is an acceptable trade for a quota — the alternative is
//! measuring a directory tree on every upload.
use pro_core::{ids, now_iso, DbErrorExt, Result};
use crate::Db;
use sqlx::Row;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Sample {
    pub sampled_at: String,
    pub members: i64,
    pub posts: i64,
    pub storage_bytes: i64,
    pub emails_sent: i64,
}

pub async fn record(
    pool: &Db,
    tenant_id: &str,
    members: i64,
    posts: i64,
    storage_bytes: i64,
    emails_sent: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO usage_samples (id, tenant_id, sampled_at, members, posts, storage_bytes, \
         emails_sent) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ids::object_id())
    .bind(tenant_id)
    .bind(now_iso())
    .bind(members)
    .bind(posts)
    .bind(storage_bytes)
    .bind(emails_sent)
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

/// The most recent reading for one tenant.
pub async fn latest(pool: &Db, tenant_id: &str) -> Result<Option<Sample>> {
    let row = sqlx::query(
        "SELECT sampled_at, members, posts, storage_bytes, emails_sent FROM usage_samples \
         WHERE tenant_id = ? ORDER BY sampled_at DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .db_err()?;
    Ok(row.map(|r| Sample {
        sampled_at: r.get("sampled_at"),
        members: r.get("members"),
        posts: r.get("posts"),
        storage_bytes: r.get("storage_bytes"),
        emails_sent: r.get("emails_sent"),
    }))
}

/// A tenant's readings over a window, oldest first, for a chart.
pub async fn history(pool: &Db, tenant_id: &str, days: i64) -> Result<Vec<Sample>> {
    let rows = sqlx::query(
        "SELECT sampled_at, members, posts, storage_bytes, emails_sent FROM usage_samples \
         WHERE tenant_id = ? AND sampled_at >= ? ORDER BY sampled_at",
    )
    .bind(tenant_id)
    .bind(pro_core::days_from_now(-days))
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows
        .iter()
        .map(|r| Sample {
            sampled_at: r.get("sampled_at"),
            members: r.get("members"),
            posts: r.get("posts"),
            storage_bytes: r.get("storage_bytes"),
            emails_sent: r.get("emails_sent"),
        })
        .collect())
}

/// Platform-wide totals from each tenant's latest reading.
///
/// A correlated subquery per tenant rather than a window function, because
/// SQLite's bundled version in older distributions may lack them and this
/// query runs once per dashboard load, not per request.
pub async fn platform_totals(pool: &Db) -> Result<(i64, i64, i64)> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(u.members), 0) AS members, \
                COALESCE(SUM(u.posts), 0) AS posts, \
                COALESCE(SUM(u.storage_bytes), 0) AS storage \
         FROM usage_samples u \
         WHERE u.sampled_at = ( \
             SELECT MAX(sampled_at) FROM usage_samples WHERE tenant_id = u.tenant_id)",
    )
    .fetch_one(pool)
    .await
    .db_err()?;
    Ok((row.get("members"), row.get("posts"), row.get("storage")))
}

/// Drop readings older than `days`. The dashboard charts a short window and
/// nothing else reads further back.
pub async fn purge(pool: &Db, days: i64) -> Result<u64> {
    let r = sqlx::query("DELETE FROM usage_samples WHERE sampled_at < ?")
        .bind(pro_core::days_from_now(-days))
        .execute(pool)
        .await
        .db_err()?;
    Ok(r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (Db, String) {
        let pool = crate::pool::open_in_memory().await.unwrap();
        let account = crate::accounts::create(&pool, "o@example.com", "O", "h").await.unwrap();
        let tenant = crate::tenants::create(
            &pool, &account.id, "alice", "Alice", "starter", "2026-01-01T00:00:00Z", 21000, 21010,
        )
        .await
        .unwrap();
        (pool, tenant.id)
    }

    #[tokio::test]
    async fn records_and_reads_the_latest() {
        let (pool, tenant) = fixture().await;
        assert!(latest(&pool, &tenant).await.unwrap().is_none());

        record(&pool, &tenant, 10, 5, 1024, 100).await.unwrap();
        let s = latest(&pool, &tenant).await.unwrap().unwrap();
        assert_eq!(s.members, 10);
        assert_eq!(s.posts, 5);
        assert_eq!(s.storage_bytes, 1024);
        assert_eq!(s.emails_sent, 100);
    }

    #[tokio::test]
    async fn keeps_a_history() {
        let (pool, tenant) = fixture().await;
        record(&pool, &tenant, 1, 1, 1, 1).await.unwrap();
        // Backdate one reading so the window filter is exercised.
        sqlx::query("UPDATE usage_samples SET sampled_at = ?")
            .bind(pro_core::days_from_now(-60))
            .execute(&pool)
            .await
            .unwrap();
        record(&pool, &tenant, 2, 2, 2, 2).await.unwrap();

        assert_eq!(history(&pool, &tenant, 30).await.unwrap().len(), 1, "only the recent one");
        assert_eq!(history(&pool, &tenant, 90).await.unwrap().len(), 2);
        // Newest wins for `latest`, regardless of insertion order.
        assert_eq!(latest(&pool, &tenant).await.unwrap().unwrap().members, 2);
    }

    /// Totals must count each tenant once — its newest reading — not sum
    /// every sample ever taken.
    #[tokio::test]
    async fn totals_use_one_reading_per_tenant() {
        let (pool, tenant) = fixture().await;
        let account = crate::accounts::create(&pool, "two@example.com", "T", "h").await.unwrap();
        let other = crate::tenants::create(
            &pool, &account.id, "bob", "Bob", "starter", "2026-01-01T00:00:00Z", 21000, 21010,
        )
        .await
        .unwrap();

        // An older reading that must not be counted.
        record(&pool, &tenant, 999, 999, 999, 0).await.unwrap();
        sqlx::query("UPDATE usage_samples SET sampled_at = ? WHERE tenant_id = ?")
            .bind(pro_core::days_from_now(-2))
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();

        record(&pool, &tenant, 10, 2, 100, 0).await.unwrap();
        record(&pool, &other.id, 5, 3, 50, 0).await.unwrap();

        let (members, posts, storage) = platform_totals(&pool).await.unwrap();
        assert_eq!(members, 15);
        assert_eq!(posts, 5);
        assert_eq!(storage, 150);
    }

    /// No samples at all must be zero, not an error — a fresh platform has
    /// none and still has to render its dashboard.
    #[tokio::test]
    async fn totals_of_nothing_are_zero() {
        let (pool, _) = fixture().await;
        assert_eq!(platform_totals(&pool).await.unwrap(), (0, 0, 0));
    }

    #[tokio::test]
    async fn purges_old_samples() {
        let (pool, tenant) = fixture().await;
        record(&pool, &tenant, 1, 1, 1, 1).await.unwrap();
        sqlx::query("UPDATE usage_samples SET sampled_at = ?")
            .bind(pro_core::days_from_now(-120))
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(purge(&pool, 90).await.unwrap(), 1);
    }

    /// Samples go with the tenant when it is really deleted.
    #[tokio::test]
    async fn samples_cascade_with_the_tenant() {
        let (pool, tenant) = fixture().await;
        record(&pool, &tenant, 1, 1, 1, 1).await.unwrap();
        sqlx::query("DELETE FROM tenants WHERE id = ?")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        assert!(latest(&pool, &tenant).await.unwrap().is_none());
    }
}
