//! Tenant records, and the two pieces of bookkeeping that only the database
//! can do safely: allocating a port and claiming a slug.
use pro_core::{ids, now_iso, DbErrorExt, ProError, Result, Tenant, TenantStatus};
use crate::Db;
use sqlx::any::AnyRow;
use sqlx::Row;

fn map(row: &AnyRow) -> Tenant {
    Tenant {
        id: row.get("id"),
        slug: row.get("slug"),
        name: row.get("name"),
        account_id: row.get("account_id"),
        plan_slug: row.get("plan_slug"),
        // An unrecognised status means a row written by a newer version. It
        // is read as `Failed` rather than panicking: a tenant the platform
        // cannot interpret must not be routed to, and must be visible to an
        // operator.
        status: TenantStatus::parse(row.get::<String, _>("status").as_str())
            .unwrap_or(TenantStatus::Failed),
        port: row.get::<i64, _>("port") as u16,
        period_start: row.get("period_start"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        last_request_at: row.get("last_request_at"),
        suspended_at: row.get("suspended_at"),
        suspended_reason: row.get("suspended_reason"),
        delete_after: row.get("delete_after"),
        storage_bytes: row.get("storage_bytes"),
        platform_key_id: row.get("platform_key_id"),
        platform_key_secret: row.get("platform_key_secret"),
        notes: row.get("notes"),
    }
}

const COLS: &str = "id, slug, name, account_id, plan_slug, status, port, period_start, \
                    created_at, updated_at, last_request_at, suspended_at, suspended_reason, \
                    delete_after, storage_bytes, notes, platform_key_id, platform_key_secret";

/// Create a tenant row, allocating its port.
///
/// The port allocation and the insert are one transaction on purpose. Two
/// signups racing would otherwise read the same "lowest free port" and the
/// second insert would fail on the unique index — or worse, if the index were
/// missing, two tenants would be told to bind the same port and one would
/// silently fail to start.
pub async fn create(
    pool: &Db,
    account_id: &str,
    slug: &str,
    name: &str,
    plan_slug: &str,
    period_start: &str,
    port_floor: u16,
    port_ceiling: u16,
) -> Result<Tenant> {
    let id = ids::object_id();
    let now = now_iso();
    let mut tx = pool.begin().await.db_err()?;

    // The lowest unused port in range. `tenants_port_unique` is what makes
    // this safe under a race; this query only has to avoid the common case.
    let taken: Vec<i64> = sqlx::query_scalar("SELECT port FROM tenants ORDER BY port")
        .fetch_all(&mut *tx)
        .await
        .db_err()?;
    let mut port = port_floor;
    for t in taken {
        if t == port as i64 {
            port = port.checked_add(1).ok_or_else(|| {
                ProError::Internal("ran out of tenant ports".into())
            })?;
        } else if t > port as i64 {
            break;
        }
    }
    if port > port_ceiling {
        // Capacity, not a bug: the operator has to widen the range or add a
        // machine. Said plainly so it is actionable.
        return Err(ProError::Unavailable(format!(
            "no free tenant port between {port_floor} and {port_ceiling} — widen \
             tenants.portFloor/portCeiling or move tenants to another host"
        )));
    }

    let result = sqlx::query(
        "INSERT INTO tenants (id, slug, name, account_id, plan_slug, status, port, \
         period_start, created_at, updated_at) VALUES (?, ?, ?, ?, ?, 'provisioning', ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(slug)
    .bind(name)
    .bind(account_id)
    .bind(plan_slug)
    .bind(port as i64)
    .bind(period_start)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await;

    match result {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            // The unique index on the slug is the authority on whether an
            // address is free — a prior availability check can always be
            // overtaken between the check and the insert.
            if crate::unique::is_unique_violation(&msg, &["tenants_slug_unique", "tenants.slug"]) {
                return Err(ProError::Conflict(format!(
                    "“{slug}” has just been taken. Please choose another address."
                )));
            }
            if crate::unique::is_unique_violation(&msg, &["tenants_port_unique", "tenants.port"]) {
                return Err(ProError::Unavailable(
                    "Another site was created at the same moment. Please try again.".into(),
                ));
            }
            return Err(ProError::Db(msg));
        }
    }
    tx.commit().await.db_err()?;

    get(pool, &id).await
}

pub async fn get(pool: &Db, id: &str) -> Result<Tenant> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM tenants WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .db_err()?
        .ok_or_else(|| ProError::NotFound(format!("no tenant {id}")))?;
    Ok(map(&row))
}

pub async fn find_by_slug(pool: &Db, slug: &str) -> Result<Option<Tenant>> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM tenants WHERE lower(slug) = lower(?)"))
        .bind(slug)
        .fetch_optional(pool)
        .await
        .db_err()?;
    Ok(row.as_ref().map(map))
}

/// Every tenant belonging to one customer, newest first.
pub async fn list_for_account(pool: &Db, account_id: &str) -> Result<Vec<Tenant>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM tenants WHERE account_id = ? AND status != 'deleted' \
         ORDER BY created_at DESC"
    ))
    .bind(account_id)
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows.iter().map(map).collect())
}

/// Tenants for the operator's console, filtered and paged.
pub async fn list(
    pool: &Db,
    status: Option<TenantStatus>,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Tenant>> {
    let mut sql = format!("SELECT {COLS} FROM tenants WHERE 1=1");
    if status.is_some() {
        sql.push_str(" AND status = ?");
    }
    if search.is_some() {
        sql.push_str(" AND (slug LIKE ? OR name LIKE ?)");
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ? OFFSET ?");

    let mut q = sqlx::query(&sql);
    if let Some(s) = status {
        q = q.bind(s.as_str());
    }
    if let Some(term) = search {
        let like = format!("%{term}%");
        q = q.bind(like.clone()).bind(like);
    }
    let rows = q.bind(limit).bind(offset).fetch_all(pool).await.db_err()?;
    Ok(rows.iter().map(map).collect())
}

pub async fn count_by_status(pool: &Db) -> Result<Vec<(String, i64)>> {
    sqlx::query_as("SELECT status, COUNT(*) FROM tenants GROUP BY status")
        .fetch_all(pool)
        .await
        .db_err()
}

pub async fn set_status(pool: &Db, id: &str, status: TenantStatus) -> Result<()> {
    sqlx::query("UPDATE tenants SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status.as_str())
        .bind(now_iso())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// Move a tenant to a different plan.
///
/// This only changes the record. The plan does not take effect until the new
/// `hostSettings` is written and the tenant process restarted — which is
/// `pro_provision::apply_plan`'s job, and is why that is the function callers
/// should reach for rather than this one.
pub async fn set_plan(pool: &Db, id: &str, plan_slug: &str) -> Result<()> {
    sqlx::query("UPDATE tenants SET plan_slug = ?, updated_at = ? WHERE id = ?")
        .bind(plan_slug)
        .bind(now_iso())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

pub async fn suspend(pool: &Db, id: &str, reason: &str) -> Result<()> {
    let now = now_iso();
    sqlx::query(
        "UPDATE tenants SET status = 'suspended', suspended_at = ?, suspended_reason = ?, \
         updated_at = ? WHERE id = ?",
    )
    .bind(&now)
    .bind(reason)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

pub async fn unsuspend(pool: &Db, id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE tenants SET status = 'active', suspended_at = NULL, suspended_reason = NULL, \
         updated_at = ? WHERE id = ?",
    )
    .bind(now_iso())
    .bind(id)
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

/// Remember (or forget) the platform's admin key for this site.
pub async fn set_platform_key(
    pool: &Db,
    id: &str,
    key_id: Option<&str>,
    key_secret: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE tenants SET platform_key_id = ?, platform_key_secret = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(key_id)
    .bind(key_secret)
    .bind(now_iso())
    .bind(id)
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

/// Cancel a scheduled deletion: active again, with nothing left pending.
///
/// `unsuspend` alone leaves `delete_after` set on a site that is no longer
/// waiting to die — inert today, because the sweep only looks at
/// `pending_deletion`, but a stale execution date on an active site is a
/// trap for any future query that trusts it.
pub async fn clear_deletion(pool: &Db, id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE tenants SET status = 'active', delete_after = NULL, suspended_at = NULL, \
         suspended_reason = NULL, updated_at = ? WHERE id = ?",
    )
    .bind(now_iso())
    .bind(id)
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

/// Schedule deletion, keeping the files for `grace_days` first.
///
/// Deliberately not immediate. The commonest reason a site is deleted is a
/// customer changing their mind, and the second commonest is a mistake — so
/// the window between "deleted" and "gone" is the single most valuable piece
/// of this whole lifecycle.
pub async fn schedule_deletion(pool: &Db, id: &str, grace_days: i64) -> Result<()> {
    let now = now_iso();
    sqlx::query(
        "UPDATE tenants SET status = 'pending_deletion', delete_after = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(pro_core::days_from_now(grace_days))
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

/// Tenants whose grace period has expired and whose files may now go.
pub async fn due_for_deletion(pool: &Db) -> Result<Vec<Tenant>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM tenants WHERE status = 'pending_deletion' \
         AND delete_after IS NOT NULL AND delete_after <= ?"
    ))
    .bind(now_iso())
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows.iter().map(map).collect())
}

/// Mark a tenant's files as gone. The row survives so the slug is not
/// immediately reissued and the audit trail still resolves.
pub async fn mark_deleted(pool: &Db, id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE tenants SET status = 'deleted', delete_after = NULL, storage_bytes = 0, \
         updated_at = ? WHERE id = ?",
    )
    .bind(now_iso())
    .bind(id)
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

/// Stamp a tenant as having just served a request.
///
/// On the hot path — the edge calls it for every proxied request — so it is a
/// single unindexed UPDATE and nothing else. `updated_at` is deliberately
/// *not* touched: that column means "an operator or the platform changed
/// something", and letting traffic move it would make it useless for spotting
/// real changes.
pub async fn touch(pool: &Db, id: &str) -> Result<()> {
    sqlx::query("UPDATE tenants SET last_request_at = ? WHERE id = ?")
        .bind(now_iso())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

pub async fn set_storage_bytes(pool: &Db, id: &str, bytes: i64) -> Result<()> {
    sqlx::query("UPDATE tenants SET storage_bytes = ? WHERE id = ?")
        .bind(bytes)
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// Reset the date periodic limits count from.
///
/// Called when a subscription renews, so the tenant's email allowance resets
/// on their own billing anniversary rather than on a calendar month.
pub async fn set_period_start(pool: &Db, id: &str, period_start: &str) -> Result<()> {
    sqlx::query("UPDATE tenants SET period_start = ?, updated_at = ? WHERE id = ?")
        .bind(period_start)
        .bind(now_iso())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

pub async fn rename(pool: &Db, id: &str, name: &str) -> Result<()> {
    sqlx::query("UPDATE tenants SET name = ?, updated_at = ? WHERE id = ?")
        .bind(name)
        .bind(now_iso())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// How many sites this account has that are not deleted — for the per-account
/// site allowance.
pub async fn count_for_account(pool: &Db, account_id: &str) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM tenants WHERE account_id = ? AND status != 'deleted'",
    )
    .bind(account_id)
    .fetch_one(pool)
    .await
    .db_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (Db, String) {
        let pool = crate::pool::open_in_memory().await.unwrap();
        let account = crate::accounts::create(&pool, "owner@example.com", "Owner", "hash")
            .await
            .unwrap();
        (pool, account.id)
    }

    #[tokio::test]
    async fn creates_and_reads_back() {
        let (pool, account) = fixture().await;
        let t = create(&pool, &account, "alice", "Alice's Blog", "starter", "2026-01-01T00:00:00Z", 21000, 21010)
            .await
            .unwrap();
        assert_eq!(t.slug, "alice");
        assert_eq!(t.status, TenantStatus::Provisioning);
        assert_eq!(t.port, 21000);
        assert_eq!(get(&pool, &t.id).await.unwrap().slug, "alice");
        assert_eq!(find_by_slug(&pool, "ALICE").await.unwrap().unwrap().id, t.id);
    }

    /// Ports must not be shared: two tenants on one port means one of them
    /// silently fails to start.
    #[tokio::test]
    async fn allocates_distinct_ports() {
        let (pool, account) = fixture().await;
        let a = create(&pool, &account, "a", "A", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        let b = create(&pool, &account, "b", "B", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        assert_eq!(a.port, 21000);
        assert_eq!(b.port, 21001);
    }

    /// A port freed by a deleted tenant is reused — the range is finite and
    /// churn must not exhaust it. The deleted row keeps its port until the
    /// row itself goes, so this tests a genuinely removed tenant.
    #[tokio::test]
    async fn reuses_a_gap_in_the_port_range() {
        let (pool, account) = fixture().await;
        let a = create(&pool, &account, "a", "A", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        let _b = create(&pool, &account, "b", "B", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        sqlx::query("DELETE FROM tenants WHERE id = ?").bind(&a.id).execute(&pool).await.unwrap();

        let c = create(&pool, &account, "c", "C", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        assert_eq!(c.port, 21000, "the freed port is taken first");
    }

    /// Running out of ports is a capacity problem and has to say so, rather
    /// than failing on a constraint.
    #[tokio::test]
    async fn reports_an_exhausted_port_range() {
        let (pool, account) = fixture().await;
        create(&pool, &account, "a", "A", "starter", "2026-01-01T00:00:00Z", 21000, 21001).await.unwrap();
        create(&pool, &account, "b", "B", "starter", "2026-01-01T00:00:00Z", 21000, 21001).await.unwrap();
        let err = create(&pool, &account, "c", "C", "starter", "2026-01-01T00:00:00Z", 21000, 21001)
            .await
            .expect_err("range is full");
        assert_eq!(err.status(), 503);
    }

    /// The unique index is the authority on a slug, not a prior check — two
    /// signups can always race.
    #[tokio::test]
    async fn refuses_a_duplicate_slug() {
        let (pool, account) = fixture().await;
        create(&pool, &account, "taken", "A", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        let err = create(&pool, &account, "TAKEN", "B", "starter", "2026-01-01T00:00:00Z", 21000, 21010)
            .await
            .expect_err("case-insensitively the same hostname");
        assert_eq!(err.status(), 409);
    }

    #[tokio::test]
    async fn suspends_and_restores() {
        let (pool, account) = fixture().await;
        let t = create(&pool, &account, "s", "S", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        suspend(&pool, &t.id, "payment failed").await.unwrap();
        let s = get(&pool, &t.id).await.unwrap();
        assert_eq!(s.status, TenantStatus::Suspended);
        assert_eq!(s.suspended_reason.as_deref(), Some("payment failed"));
        // Suspended still routes: the site answers, and its own
        // publicSiteAccess limit closes the public half.
        assert!(s.status.is_routable());

        unsuspend(&pool, &t.id).await.unwrap();
        let r = get(&pool, &t.id).await.unwrap();
        assert_eq!(r.status, TenantStatus::Active);
        assert!(r.suspended_at.is_none());
    }

    /// Deletion is scheduled, not immediate — the grace window is the most
    /// valuable part of the lifecycle.
    #[tokio::test]
    async fn deletion_waits_out_a_grace_period() {
        let (pool, account) = fixture().await;
        let t = create(&pool, &account, "d", "D", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();

        schedule_deletion(&pool, &t.id, 30).await.unwrap();
        assert_eq!(get(&pool, &t.id).await.unwrap().status, TenantStatus::PendingDeletion);
        assert!(due_for_deletion(&pool).await.unwrap().is_empty(), "not yet");

        // Backdate the deadline to simulate the window expiring.
        sqlx::query("UPDATE tenants SET delete_after = ? WHERE id = ?")
            .bind(pro_core::days_from_now(-1))
            .bind(&t.id)
            .execute(&pool)
            .await
            .unwrap();
        let due = due_for_deletion(&pool).await.unwrap();
        assert_eq!(due.len(), 1);

        mark_deleted(&pool, &t.id).await.unwrap();
        assert_eq!(get(&pool, &t.id).await.unwrap().status, TenantStatus::Deleted);
        // The row survives, so the slug is not instantly reissued.
        assert!(find_by_slug(&pool, "d").await.unwrap().is_some());
        assert!(due_for_deletion(&pool).await.unwrap().is_empty());
    }

    /// Traffic must not disturb `updated_at`, which means "something was
    /// changed" and would otherwise be meaningless.
    #[tokio::test]
    async fn touch_does_not_move_updated_at() {
        let (pool, account) = fixture().await;
        let t = create(&pool, &account, "t", "T", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        touch(&pool, &t.id).await.unwrap();
        let after = get(&pool, &t.id).await.unwrap();
        assert!(after.last_request_at.is_some());
        assert_eq!(after.updated_at, t.updated_at);
    }

    #[tokio::test]
    async fn lists_and_counts() {
        let (pool, account) = fixture().await;
        create(&pool, &account, "one", "One", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        let two = create(&pool, &account, "two", "Two", "creator", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        set_status(&pool, &two.id, TenantStatus::Active).await.unwrap();

        assert_eq!(list_for_account(&pool, &account).await.unwrap().len(), 2);
        assert_eq!(count_for_account(&pool, &account).await.unwrap(), 2);
        assert_eq!(list(&pool, Some(TenantStatus::Active), None, 50, 0).await.unwrap().len(), 1);
        assert_eq!(list(&pool, None, Some("tw"), 50, 0).await.unwrap().len(), 1);

        let counts = count_by_status(&pool).await.unwrap();
        assert_eq!(counts.iter().find(|(s, _)| s == "active").unwrap().1, 1);
    }

    /// A deleted site stops counting against the account's allowance and
    /// stops appearing in its list.
    #[tokio::test]
    async fn deleted_tenants_leave_the_account_list() {
        let (pool, account) = fixture().await;
        let t = create(&pool, &account, "gone", "Gone", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        mark_deleted(&pool, &t.id).await.unwrap();
        assert!(list_for_account(&pool, &account).await.unwrap().is_empty());
        assert_eq!(count_for_account(&pool, &account).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn moves_plan_and_period() {
        let (pool, account) = fixture().await;
        let t = create(&pool, &account, "p", "P", "starter", "2026-01-01T00:00:00Z", 21000, 21010).await.unwrap();
        set_plan(&pool, &t.id, "team").await.unwrap();
        set_period_start(&pool, &t.id, "2026-02-01T00:00:00Z").await.unwrap();
        set_storage_bytes(&pool, &t.id, 4096).await.unwrap();
        rename(&pool, &t.id, "Renamed").await.unwrap();

        let after = get(&pool, &t.id).await.unwrap();
        assert_eq!(after.plan_slug, "team");
        assert_eq!(after.period_start, "2026-02-01T00:00:00Z");
        assert_eq!(after.storage_bytes, 4096);
        assert_eq!(after.name, "Renamed");
    }

    #[tokio::test]
    async fn missing_tenant_is_a_404() {
        let (pool, _) = fixture().await;
        assert_eq!(get(&pool, "nope").await.unwrap_err().status(), 404);
        assert!(find_by_slug(&pool, "nope").await.unwrap().is_none());
    }
}
