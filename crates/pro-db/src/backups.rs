//! Snapshot archives of a tenant's site.
//!
//! The rows here are an index over files in the backups directory — the file
//! is the backup, and a row without its file (or the reverse) is a bug this
//! module's callers avoid by always going through [`record`] and
//! [`take_pruned`] together, so a file is only ever removed alongside its row.
//!
//! A handful per tenant is kept, pruned oldest-first: unbounded backups fill a
//! disk, and the oldest copy of a site is rarely the one anybody wants.
use pro_core::{ProError, Result};
use crate::Db;

/// One archive, as the platform knows it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Backup {
    pub id: String,
    pub tenant_id: String,
    /// Absolute path of the zip on this machine.
    pub path: String,
    pub size_bytes: i64,
    /// Why it was taken: `manual`, `auto` (nightly) or `pre_delete`.
    pub reason: String,
    pub created_at: String,
}

/// A human label for a backup's `reason`, for the tables both panels show.
pub fn reason_label(reason: &str) -> &'static str {
    match reason {
        "manual" => "Manual",
        "auto" => "Nightly",
        "pre_delete" => "Before deletion",
        _ => "Backup",
    }
}

/// Record a finished archive.
pub async fn record(
    pool: &Db,
    tenant_id: &str,
    path: &str,
    size_bytes: i64,
    reason: &str,
) -> Result<Backup> {
    let id = pro_core::ids::object_id();
    sqlx::query(
        "INSERT INTO backups (id, tenant_id, path, size_bytes, reason, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(path)
    .bind(size_bytes)
    .bind(reason)
    .bind(pro_core::now_iso())
    .execute(pool)
    .await
    .map_err(|e| ProError::Db(e.to_string()))?;

    get(pool, &id).await
}

/// A tenant's archives, newest first.
pub async fn list_for_tenant(
    pool: &Db,
    tenant_id: &str,
    limit: i64,
) -> Result<Vec<Backup>> {
    // `id` breaks the tie rather than SQLite's `rowid`, which MySQL has no
    // equivalent of. It only has to be *deterministic*: the tiebreak decides
    // the order of two snapshots taken in the same second, and `take_pruned`
    // orders identically, so whichever rows this call shows are exactly the
    // ones pruning keeps.
    sqlx::query_as(&format!(
        "SELECT id, tenant_id, path, size_bytes, reason, created_at \
         FROM backups WHERE tenant_id = ? ORDER BY created_at DESC, id DESC LIMIT {limit}"
    ))
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .map_err(|e| ProError::Db(e.to_string()))
}

/// One archive by id — the lookup behind a download or restore.
pub async fn get(pool: &Db, id: &str) -> Result<Backup> {
    sqlx::query_as(
        "SELECT id, tenant_id, path, size_bytes, reason, created_at \
         FROM backups WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ProError::Db(e.to_string()))?
    .ok_or_else(|| ProError::NotFound("No such backup.".into()))
}

/// When this tenant was last backed up, if ever — the nightly pass skips
/// sites backed up more recently than its interval.
pub async fn last_backup_at(pool: &Db, tenant_id: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT created_at FROM backups WHERE tenant_id = ? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ProError::Db(e.to_string()))?;
    Ok(row.map(|(at,)| at))
}

/// Drop the rows past `keep`, returning their `(id, path)` so the caller can
/// remove the files. The caller deletes the file *and* the row, or neither —
/// a row for a deleted file is a download that 404s; a file with no row is
/// unreachable and quietly eats disk forever.
pub async fn take_pruned(
    pool: &Db,
    tenant_id: &str,
    keep: i64,
) -> Result<Vec<(String, String)>> {
    // Ordered exactly as `list_for_tenant`, so "the rows past `keep`" means
    // the same thing in both — see the note there about the `id` tiebreak.
    // `skip_first` is "every row after the offset", which SQLite spells with a
    // negative limit and MySQL refuses.
    sqlx::query_as(&format!(
        "SELECT id, path FROM backups WHERE tenant_id = ? \
         ORDER BY created_at DESC, id DESC {}",
        crate::dialect::skip_first()
    ))
    .bind(tenant_id)
    .bind(keep)
    .fetch_all(pool)
    .await
    .map_err(|e| ProError::Db(e.to_string()))
}

/// Remove one row, once its file is gone.
pub async fn remove(pool: &Db, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM backups WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| ProError::Db(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> crate::Db {
        let pool = crate::open_in_memory().await.unwrap();
        sqlx::query("INSERT INTO accounts (id, email, name, password_hash, status, created_at) \
                     VALUES ('a1', 'a@example.com', 'A', 'x', 'active', '2026-01-01T00:00:00Z')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tenants (id, slug, name, account_id, plan_slug, status, port, \
                     period_start, created_at, updated_at) \
                     VALUES ('t1', 'one', 'One', 'a1', 'starter', 'active', 21000, \
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')")
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    /// `record` stamps now; tests want controlled orderings, so the row's
    /// timestamp is set after the insert.
    async fn seed(pool: &Db, tenant: &str, reason: &str, at: &str) -> Backup {
        let backup = record(pool, tenant, &format!("/tmp/{tenant}-{at}.zip"), 100, reason)
            .await
            .unwrap();
        sqlx::query("UPDATE backups SET created_at = ? WHERE id = ?")
            .bind(at)
            .bind(&backup.id)
            .execute(pool)
            .await
            .unwrap();
        get(pool, &backup.id).await.unwrap()
    }

    #[tokio::test]
    async fn lists_newest_first_for_one_tenant_only() {
        let pool = pool().await;
        seed(&pool, "t1", "auto", "2026-01-01T00:00:00Z").await;
        seed(&pool, "t1", "manual", "2026-01-02T00:00:00Z").await;
        // Another tenant's archives must not appear.
        sqlx::query("INSERT INTO tenants (id, slug, name, account_id, plan_slug, status, port, \
                     period_start, created_at, updated_at) VALUES ('t2', 'two', 'Two', 'a1', \
                     'starter', 'active', 21001, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', \
                     '2026-01-01T00:00:00Z')")
            .execute(&pool)
            .await
            .unwrap();
        seed(&pool, "t2", "manual", "2026-01-03T00:00:00Z").await;

        let listed = list_for_tenant(&pool, "t1", 10).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].created_at > listed[1].created_at, "newest first");
        assert!(listed.iter().all(|b| b.tenant_id == "t1"));
    }

    #[tokio::test]
    async fn pruning_keeps_the_newest() {
        let pool = pool().await;
        for day in 1..=6 {
            seed(&pool, "t1", "auto", &format!("2026-01-0{day}T00:00:00Z")).await;
        }

        let pruned = take_pruned(&pool, "t1", 5).await.unwrap();
        assert_eq!(pruned.len(), 1, "only the oldest falls out");
        assert!(pruned[0].1.ends_with("2026-01-01T00:00:00Z.zip"), "the oldest");

        remove(&pool, &pruned[0].0).await.unwrap();
        assert_eq!(list_for_tenant(&pool, "t1", 10).await.unwrap().len(), 5);
        assert_eq!(
            last_backup_at(&pool, "t1").await.unwrap().as_deref(),
            Some("2026-01-06T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn get_reports_a_missing_archive() {
        let pool = pool().await;
        assert!(get(&pool, "nope").await.is_err());
        let one = seed(&pool, "t1", "manual", "2026-01-01T00:00:00Z").await;
        assert_eq!(get(&pool, &one.id).await.unwrap().path, one.path);
    }

    #[test]
    fn reason_labels_read_well() {
        assert_eq!(reason_label("pre_delete"), "Before deletion");
        assert_eq!(reason_label("anything-else"), "Backup");
    }
}
