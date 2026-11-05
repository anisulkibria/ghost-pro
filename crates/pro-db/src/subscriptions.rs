//! Platform subscriptions — what a customer pays *us*.
//!
//! Three providers write into this one table, so the vocabulary here is
//! normalised and provider-neutral: `status` is always one of our six values,
//! never Stripe's `incomplete_expired` or Lemon Squeezy's `on_trial`. The
//! translation happens in `pro-billing`, at the edge of the system, which
//! means everything downstream — entitlement, dunning, the dashboard — is
//! written once rather than three times.
use pro_core::{ids, now_iso, DbErrorExt, ProError, Result, Subscription};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// The statuses this table stores. Anything a provider sends is mapped onto
/// one of these before it gets here.
pub const STATUSES: &[&str] =
    &["trialing", "active", "past_due", "paused", "canceled", "expired"];

fn map(row: &SqliteRow) -> Subscription {
    Subscription {
        id: row.get("id"),
        account_id: row.get("account_id"),
        tenant_id: row.get("tenant_id"),
        plan_slug: row.get("plan_slug"),
        provider: row.get("provider"),
        provider_subscription_id: row.get("provider_subscription_id"),
        provider_customer_id: row.get("provider_customer_id"),
        status: row.get("status"),
        current_period_start: row.get("current_period_start"),
        current_period_end: row.get("current_period_end"),
        trial_ends_at: row.get("trial_ends_at"),
        cancel_at_period_end: row.get::<i64, _>("cancel_at_period_end") != 0,
        canceled_at: row.get("canceled_at"),
        past_due_since: row.get("past_due_since"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

const COLS: &str = "id, account_id, tenant_id, plan_slug, provider, provider_subscription_id, \
                    provider_customer_id, status, current_period_start, current_period_end, \
                    trial_ends_at, cancel_at_period_end, canceled_at, past_due_since, \
                    created_at, updated_at";

/// What a webhook or a checkout tells us about a subscription.
///
/// Every field is optional because no provider sends all of them on every
/// event, and an absent field means "unchanged" rather than "cleared" — see
/// [`upsert`]. Getting that distinction wrong is how a renewal event that
/// omits the trial date silently cancels somebody's trial.
#[derive(Debug, Clone, Default)]
pub struct Upsert<'a> {
    pub account_id: &'a str,
    pub tenant_id: Option<&'a str>,
    pub plan_slug: Option<&'a str>,
    pub provider: &'a str,
    pub provider_subscription_id: Option<&'a str>,
    pub provider_customer_id: Option<&'a str>,
    pub status: Option<&'a str>,
    pub current_period_start: Option<&'a str>,
    pub current_period_end: Option<&'a str>,
    pub trial_ends_at: Option<&'a str>,
    pub cancel_at_period_end: Option<bool>,
    pub canceled_at: Option<&'a str>,
}

/// Create or update a subscription, keyed by `(provider, subscription id)`.
///
/// Webhooks arrive out of order and more than once, which shapes two rules
/// here:
///
/// * an absent field leaves the stored value alone, so a sparse event cannot
///   erase what a richer one established;
/// * `past_due_since` is stamped on the *transition* into `past_due` and
///   cleared on the way out, so the dunning grace period is measured from the
///   first failure rather than from the most recent retry. Without that, a
///   provider that retries daily would reset the clock every day and a site
///   would never be suspended.
pub async fn upsert(pool: &SqlitePool, up: Upsert<'_>) -> Result<Subscription> {
    if let Some(status) = up.status {
        if !STATUSES.contains(&status) {
            // A provider status that was not translated is a bug in
            // pro-billing, and storing it would quietly break entitlement.
            return Err(ProError::Internal(format!(
                "subscription status {status:?} was not normalised"
            )));
        }
    }

    let existing = match up.provider_subscription_id {
        Some(sub_id) => find_by_provider_id(pool, up.provider, sub_id).await?,
        None => None,
    };
    let now = now_iso();

    match existing {
        Some(current) => {
            let status = up.status.unwrap_or(&current.status);
            // Stamp the start of dunning, or clear it on recovery.
            let past_due_since = match (status, current.past_due_since.as_deref()) {
                ("past_due", None) => Some(now.clone()),
                ("past_due", Some(since)) => Some(since.to_string()),
                _ => None,
            };
            sqlx::query(
                "UPDATE subscriptions SET \
                    tenant_id = COALESCE(?, tenant_id), \
                    plan_slug = COALESCE(?, plan_slug), \
                    provider_customer_id = COALESCE(?, provider_customer_id), \
                    status = ?, \
                    current_period_start = COALESCE(?, current_period_start), \
                    current_period_end = COALESCE(?, current_period_end), \
                    trial_ends_at = COALESCE(?, trial_ends_at), \
                    cancel_at_period_end = COALESCE(?, cancel_at_period_end), \
                    canceled_at = COALESCE(?, canceled_at), \
                    past_due_since = ?, \
                    updated_at = ? \
                 WHERE id = ?",
            )
            .bind(up.tenant_id)
            .bind(up.plan_slug)
            .bind(up.provider_customer_id)
            .bind(status)
            .bind(up.current_period_start)
            .bind(up.current_period_end)
            .bind(up.trial_ends_at)
            .bind(up.cancel_at_period_end.map(i64::from))
            .bind(up.canceled_at)
            .bind(past_due_since)
            .bind(&now)
            .bind(&current.id)
            .execute(pool)
            .await
            .db_err()?;
            get(pool, &current.id).await
        }
        None => {
            let id = ids::object_id();
            let status = up.status.unwrap_or("active");
            sqlx::query(
                "INSERT INTO subscriptions (id, account_id, tenant_id, plan_slug, provider, \
                 provider_subscription_id, provider_customer_id, status, current_period_start, \
                 current_period_end, trial_ends_at, cancel_at_period_end, canceled_at, \
                 past_due_since, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(up.account_id)
            .bind(up.tenant_id)
            .bind(up.plan_slug.unwrap_or(""))
            .bind(up.provider)
            .bind(up.provider_subscription_id)
            .bind(up.provider_customer_id)
            .bind(status)
            .bind(up.current_period_start)
            .bind(up.current_period_end)
            .bind(up.trial_ends_at)
            .bind(i64::from(up.cancel_at_period_end.unwrap_or(false)))
            .bind(up.canceled_at)
            .bind(if status == "past_due" { Some(now.clone()) } else { None })
            .bind(&now)
            .bind(&now)
            .execute(pool)
            .await
            .db_err()?;
            get(pool, &id).await
        }
    }
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Subscription> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM subscriptions WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .db_err()?
        .ok_or_else(|| ProError::NotFound(format!("no subscription {id}")))?;
    Ok(map(&row))
}

pub async fn find_by_provider_id(
    pool: &SqlitePool,
    provider: &str,
    subscription_id: &str,
) -> Result<Option<Subscription>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM subscriptions WHERE provider = ? AND provider_subscription_id = ?"
    ))
    .bind(provider)
    .bind(subscription_id)
    .fetch_optional(pool)
    .await
    .db_err()?;
    Ok(row.as_ref().map(map))
}

/// The subscription that pays for a tenant, if there is one.
///
/// Prefers an entitling one. A site that was cancelled and then resubscribed
/// has two rows, and the live one is the one that matters.
pub async fn for_tenant(pool: &SqlitePool, tenant_id: &str) -> Result<Option<Subscription>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM subscriptions WHERE tenant_id = ? \
         ORDER BY CASE WHEN status IN ('trialing','active','past_due') THEN 0 ELSE 1 END, \
                  created_at DESC LIMIT 1"
    ))
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .db_err()?;
    Ok(row.as_ref().map(map))
}

pub async fn list_for_account(pool: &SqlitePool, account_id: &str) -> Result<Vec<Subscription>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM subscriptions WHERE account_id = ? ORDER BY created_at DESC"
    ))
    .bind(account_id)
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows.iter().map(map).collect())
}

/// Attach a subscription to the tenant it paid for.
///
/// Checkout happens before the site exists when payment is required up front,
/// so the link is made once provisioning succeeds.
pub async fn attach_tenant(pool: &SqlitePool, id: &str, tenant_id: &str) -> Result<()> {
    sqlx::query("UPDATE subscriptions SET tenant_id = ?, updated_at = ? WHERE id = ?")
        .bind(tenant_id)
        .bind(now_iso())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// Subscriptions whose dunning grace period has run out, with the tenant they
/// pay for.
///
/// This is the query that decides who gets suspended, so it is deliberately
/// narrow: only `past_due`, only with a recorded start, only where the tenant
/// is still active. A site already suspended is not suspended again.
pub async fn past_due_beyond_grace(
    pool: &SqlitePool,
    grace_days: i64,
) -> Result<Vec<(Subscription, String)>> {
    let cutoff = pro_core::days_from_now(-grace_days);
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM subscriptions s \
         WHERE s.status = 'past_due' AND s.past_due_since IS NOT NULL \
           AND s.past_due_since <= ? AND s.tenant_id IS NOT NULL \
           AND EXISTS (SELECT 1 FROM tenants t WHERE t.id = s.tenant_id AND t.status = 'active')"
    ))
    .bind(cutoff)
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows
        .iter()
        .map(|r| {
            let s = map(r);
            let tenant = s.tenant_id.clone().unwrap_or_default();
            (s, tenant)
        })
        .collect())
}

/// Trials that have ended without a payment method, with their tenant.
pub async fn expired_trials(pool: &SqlitePool) -> Result<Vec<(Subscription, String)>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM subscriptions s \
         WHERE s.status = 'trialing' AND s.trial_ends_at IS NOT NULL \
           AND s.trial_ends_at <= ? AND s.tenant_id IS NOT NULL"
    ))
    .bind(now_iso())
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows
        .iter()
        .map(|r| {
            let s = map(r);
            let tenant = s.tenant_id.clone().unwrap_or_default();
            (s, tenant)
        })
        .collect())
}

/// Record a subscription an operator granted by hand — a comped site, an
/// internal one, a customer being migrated in.
pub async fn create_manual(
    pool: &SqlitePool,
    account_id: &str,
    tenant_id: Option<&str>,
    plan_slug: &str,
) -> Result<Subscription> {
    let id = ids::object_id();
    let now = now_iso();
    sqlx::query(
        "INSERT INTO subscriptions (id, account_id, tenant_id, plan_slug, provider, status, \
         current_period_start, cancel_at_period_end, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'manual', 'active', ?, 0, ?, ?)",
    )
    .bind(&id)
    .bind(account_id)
    .bind(tenant_id)
    .bind(plan_slug)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .db_err()?;
    get(pool, &id).await
}

pub async fn count_by_status(pool: &SqlitePool) -> Result<Vec<(String, i64)>> {
    sqlx::query_as("SELECT status, COUNT(*) FROM subscriptions GROUP BY status")
        .fetch_all(pool)
        .await
        .db_err()
}

/// Monthly recurring revenue, in minor units, by plan.
///
/// Counts only entitling subscriptions, so a cancelled site stops being
/// counted the moment it lapses rather than at the end of its period.
pub async fn active_by_plan(pool: &SqlitePool) -> Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT plan_slug, COUNT(*) FROM subscriptions \
         WHERE status IN ('trialing','active','past_due') GROUP BY plan_slug",
    )
    .fetch_all(pool)
    .await
    .db_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (SqlitePool, String, String) {
        let pool = crate::pool::open_in_memory().await.unwrap();
        let account = crate::accounts::create(&pool, "o@example.com", "O", "h").await.unwrap();
        let tenant = crate::tenants::create(
            &pool, &account.id, "alice", "Alice", "starter", "2026-01-01T00:00:00Z", 21000, 21010,
        )
        .await
        .unwrap();
        crate::tenants::set_status(&pool, &tenant.id, pro_core::TenantStatus::Active)
            .await
            .unwrap();
        (pool, account.id, tenant.id)
    }

    fn base<'a>(account: &'a str, provider: &'a str, sub_id: &'a str) -> Upsert<'a> {
        Upsert {
            account_id: account,
            provider,
            provider_subscription_id: Some(sub_id),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn creates_then_updates_the_same_row() {
        let (pool, account, tenant) = fixture().await;
        let mut up = base(&account, "stripe", "sub_1");
        up.tenant_id = Some(&tenant);
        up.plan_slug = Some("starter");
        up.status = Some("active");
        let first = upsert(&pool, up).await.unwrap();

        let mut again = base(&account, "stripe", "sub_1");
        again.status = Some("past_due");
        let second = upsert(&pool, again).await.unwrap();

        assert_eq!(first.id, second.id, "same row, not a second one");
        assert_eq!(second.status, "past_due");
        assert_eq!(second.plan_slug, "starter", "a sparse event did not erase the plan");
        assert_eq!(second.tenant_id.as_deref(), Some(tenant.as_str()));
    }

    /// The same id at two providers is two different subscriptions.
    #[tokio::test]
    async fn provider_scopes_the_id() {
        let (pool, account, _) = fixture().await;
        let a = upsert(&pool, base(&account, "stripe", "123")).await.unwrap();
        let b = upsert(&pool, base(&account, "paddle", "123")).await.unwrap();
        assert_ne!(a.id, b.id);
    }

    /// An absent field means "unchanged". Getting this wrong is how a renewal
    /// event that omits the trial date cancels somebody's trial.
    #[tokio::test]
    async fn absent_fields_do_not_erase() {
        let (pool, account, _) = fixture().await;
        let mut up = base(&account, "stripe", "sub_1");
        up.status = Some("trialing");
        up.trial_ends_at = Some("2026-02-01T00:00:00Z");
        up.current_period_end = Some("2026-02-01T00:00:00Z");
        upsert(&pool, up).await.unwrap();

        let mut sparse = base(&account, "stripe", "sub_1");
        sparse.status = Some("active");
        let after = upsert(&pool, sparse).await.unwrap();
        assert_eq!(after.trial_ends_at.as_deref(), Some("2026-02-01T00:00:00Z"));
        assert_eq!(after.current_period_end.as_deref(), Some("2026-02-01T00:00:00Z"));
    }

    /// Dunning is measured from the *first* failure. A provider that retries
    /// daily must not reset the clock, or a site is never suspended.
    #[tokio::test]
    async fn dunning_clock_starts_once() {
        let (pool, account, _) = fixture().await;
        let mut up = base(&account, "stripe", "sub_1");
        up.status = Some("past_due");
        let first = upsert(&pool, up).await.unwrap();
        let started = first.past_due_since.clone().expect("stamped");

        let mut retry = base(&account, "stripe", "sub_1");
        retry.status = Some("past_due");
        let second = upsert(&pool, retry).await.unwrap();
        assert_eq!(second.past_due_since, Some(started), "clock not reset by a retry");

        // Recovery clears it, so a later failure starts a fresh period.
        let mut recovered = base(&account, "stripe", "sub_1");
        recovered.status = Some("active");
        assert!(upsert(&pool, recovered).await.unwrap().past_due_since.is_none());
    }

    /// The suspension query: only past the grace window, only active tenants.
    #[tokio::test]
    async fn finds_subscriptions_past_the_grace_window() {
        let (pool, account, tenant) = fixture().await;
        let mut up = base(&account, "stripe", "sub_1");
        up.tenant_id = Some(&tenant);
        up.status = Some("past_due");
        let sub = upsert(&pool, up).await.unwrap();

        assert!(past_due_beyond_grace(&pool, 14).await.unwrap().is_empty(), "still in grace");

        sqlx::query("UPDATE subscriptions SET past_due_since = ? WHERE id = ?")
            .bind(pro_core::days_from_now(-15))
            .bind(&sub.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(past_due_beyond_grace(&pool, 14).await.unwrap().len(), 1);

        // An already-suspended site is not suspended twice.
        crate::tenants::suspend(&pool, &tenant, "unpaid").await.unwrap();
        assert!(past_due_beyond_grace(&pool, 14).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn finds_expired_trials() {
        let (pool, account, tenant) = fixture().await;
        let mut up = base(&account, "stripe", "sub_1");
        up.tenant_id = Some(&tenant);
        up.status = Some("trialing");
        up.trial_ends_at = Some("2020-01-01T00:00:00Z");
        upsert(&pool, up).await.unwrap();
        assert_eq!(expired_trials(&pool).await.unwrap().len(), 1);
    }

    /// A status a provider sent but nobody translated would silently break
    /// entitlement, so it is refused rather than stored.
    #[tokio::test]
    async fn refuses_an_untranslated_status() {
        let (pool, account, _) = fixture().await;
        let mut up = base(&account, "stripe", "sub_1");
        up.status = Some("incomplete_expired");
        assert_eq!(upsert(&pool, up).await.unwrap_err().status(), 500);
    }

    /// A resubscribed site has two rows; the live one is the one that counts.
    #[tokio::test]
    async fn prefers_the_entitling_subscription_for_a_tenant() {
        let (pool, account, tenant) = fixture().await;
        let mut old = base(&account, "stripe", "sub_old");
        old.tenant_id = Some(&tenant);
        old.status = Some("canceled");
        upsert(&pool, old).await.unwrap();

        let mut new = base(&account, "stripe", "sub_new");
        new.tenant_id = Some(&tenant);
        new.status = Some("active");
        let live = upsert(&pool, new).await.unwrap();

        assert_eq!(for_tenant(&pool, &tenant).await.unwrap().unwrap().id, live.id);
    }

    #[tokio::test]
    async fn entitlement_covers_dunning_but_not_cancellation() {
        let (pool, account, _) = fixture().await;
        for (status, entitled) in [
            ("trialing", true),
            ("active", true),
            ("past_due", true),
            ("paused", false),
            ("canceled", false),
            ("expired", false),
        ] {
            let mut up = base(&account, "stripe", status);
            up.status = Some(status);
            let s = upsert(&pool, up).await.unwrap();
            assert_eq!(s.is_entitled(), entitled, "{status}");
        }
    }

    #[tokio::test]
    async fn records_a_manual_grant() {
        let (pool, account, tenant) = fixture().await;
        let s = create_manual(&pool, &account, Some(&tenant), "business").await.unwrap();
        assert_eq!(s.provider, "manual");
        assert!(s.is_entitled());
        assert_eq!(for_tenant(&pool, &tenant).await.unwrap().unwrap().id, s.id);
    }

    #[tokio::test]
    async fn attaches_a_tenant_after_checkout() {
        let (pool, account, tenant) = fixture().await;
        let s = upsert(&pool, base(&account, "stripe", "sub_1")).await.unwrap();
        assert!(s.tenant_id.is_none());
        attach_tenant(&pool, &s.id, &tenant).await.unwrap();
        assert_eq!(get(&pool, &s.id).await.unwrap().tenant_id.as_deref(), Some(tenant.as_str()));
    }

    #[tokio::test]
    async fn reports_totals() {
        let (pool, account, _) = fixture().await;
        let mut a = base(&account, "stripe", "a");
        a.status = Some("active");
        a.plan_slug = Some("starter");
        upsert(&pool, a).await.unwrap();
        let mut b = base(&account, "stripe", "b");
        b.status = Some("canceled");
        b.plan_slug = Some("starter");
        upsert(&pool, b).await.unwrap();

        assert_eq!(list_for_account(&pool, &account).await.unwrap().len(), 2);
        let by_plan = active_by_plan(&pool).await.unwrap();
        assert_eq!(by_plan, vec![("starter".to_string(), 1)], "cancelled is not counted");
        assert_eq!(count_by_status(&pool).await.unwrap().len(), 2);
    }
}
