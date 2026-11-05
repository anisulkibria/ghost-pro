//! The audit trail, and webhook idempotency.
//!
//! Both live here because both answer the same kind of question after the
//! fact: *what happened to this site, and was it us?*
use pro_core::{ids, now_iso, AuditEvent, DbErrorExt, Result};
use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// Record something that changed money, routing, or a site's existence.
///
/// Deliberately infallible from the caller's point of view: a failure to write
/// an audit row is logged and swallowed, never propagated. Nothing here is
/// worth failing a customer's action over — if the trail cannot be written,
/// the action should still happen and the log will carry it.
pub async fn record(
    pool: &SqlitePool,
    actor: &str,
    action: &str,
    tenant_id: Option<&str>,
    account_id: Option<&str>,
    meta: Value,
) {
    let encoded = serde_json::to_string(&meta).unwrap_or_else(|_| "{}".into());
    let result = sqlx::query(
        "INSERT INTO audit_events (id, actor, action, tenant_id, account_id, meta, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ids::object_id())
    .bind(actor)
    .bind(action)
    .bind(tenant_id)
    .bind(account_id)
    .bind(&encoded)
    .bind(now_iso())
    .execute(pool)
    .await;

    if let Err(e) = result {
        // Logged at warn with the whole event, so the trail survives in the
        // log even when the table rejected it.
        tracing::warn!(
            actor, action, ?tenant_id, ?account_id, meta = %encoded,
            "could not write audit event: {e}"
        );
    }
}

fn map(row: &SqliteRow) -> AuditEvent {
    AuditEvent {
        id: row.get("id"),
        actor: row.get("actor"),
        action: row.get("action"),
        tenant_id: row.get("tenant_id"),
        account_id: row.get("account_id"),
        meta: serde_json::from_str(&row.get::<String, _>("meta")).unwrap_or(Value::Null),
        created_at: row.get("created_at"),
    }
}

const COLS: &str = "id, actor, action, tenant_id, account_id, meta, created_at";

/// The trail, newest first, optionally narrowed to one site or one actor.
pub async fn list(
    pool: &SqlitePool,
    tenant_id: Option<&str>,
    actor: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<AuditEvent>> {
    let mut sql = format!("SELECT {COLS} FROM audit_events WHERE 1=1");
    if tenant_id.is_some() {
        sql.push_str(" AND tenant_id = ?");
    }
    if actor.is_some() {
        sql.push_str(" AND actor = ?");
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?");

    let mut q = sqlx::query(&sql);
    if let Some(t) = tenant_id {
        q = q.bind(t);
    }
    if let Some(a) = actor {
        q = q.bind(a);
    }
    let rows = q.bind(limit).bind(offset).fetch_all(pool).await.db_err()?;
    Ok(rows.iter().map(map).collect())
}

/// Delete entries older than `days`.
///
/// Called on a long timer. The default retention is generous because this
/// table is small — a row per administrative action, not per request — and
/// because the questions it answers tend to be asked months later.
pub async fn purge(pool: &SqlitePool, days: i64) -> Result<u64> {
    let r = sqlx::query("DELETE FROM audit_events WHERE created_at < ?")
        .bind(pro_core::days_from_now(-days))
        .execute(pool)
        .await
        .db_err()?;
    Ok(r.rows_affected())
}

// ---------------------------------------------------------------------------
// Webhook idempotency
// ---------------------------------------------------------------------------

/// What [`claim_webhook`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum WebhookClaim {
    /// First time this event has been seen; go ahead and act on it.
    Fresh,
    /// Already handled. The caller must answer 200 and do nothing — a
    /// provider that gets an error will keep retrying, and acting twice is
    /// what grants a second subscription or cancels a site twice.
    Duplicate,
}

/// Record a provider event before acting on it.
///
/// All three providers retry, and all three can deliver the same event twice.
/// The unique index on `(provider, event_id)` is what makes this safe: the
/// second insert fails, and that failure *is* the answer.
///
/// An event with no id — which some providers omit on some payload shapes —
/// is treated as fresh every time, because there is nothing to deduplicate on.
/// That is the honest behaviour; the handlers for those events are written to
/// be idempotent in themselves.
pub async fn claim_webhook(
    pool: &SqlitePool,
    provider: &str,
    event_id: &str,
    event_type: &str,
    payload: &str,
) -> Result<WebhookClaim> {
    if event_id.is_empty() {
        return Ok(WebhookClaim::Fresh);
    }
    let result = sqlx::query(
        "INSERT INTO webhook_events (id, provider, event_id, event_type, received_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(ids::object_id())
    .bind(provider)
    .bind(event_id)
    .bind(event_type)
    .bind(now_iso())
    // Truncated: a payload is unbounded and this copy exists so a failed
    // event can be read and replayed, not as an archive.
    .bind(payload.chars().take(64_000).collect::<String>())
    .execute(pool)
    .await;

    match result {
        Ok(_) => Ok(WebhookClaim::Fresh),
        Err(e)
            if crate::unique::is_unique_violation(
                &e.to_string(),
                &["webhook_events_unique", "webhook_events.event_id"],
            ) =>
        {
            Ok(WebhookClaim::Duplicate)
        }
        Err(e) => Err(pro_core::ProError::Db(e.to_string())),
    }
}

/// Mark an event as handled.
pub async fn finish_webhook(
    pool: &SqlitePool,
    provider: &str,
    event_id: &str,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE webhook_events SET processed_at = ?, error = ? WHERE provider = ? AND event_id = ?",
    )
    .bind(now_iso())
    .bind(error.map(|e| e.chars().take(2000).collect::<String>()))
    .bind(provider)
    .bind(event_id)
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

/// Release a claim so the provider's next retry is treated as fresh.
///
/// Called when handling failed in a way that is worth retrying — the database
/// was briefly unavailable, say. Without this, a transient failure would be
/// permanently recorded as "seen" and the provider's retry ignored, losing the
/// event for good.
pub async fn release_webhook(pool: &SqlitePool, provider: &str, event_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM webhook_events WHERE provider = ? AND event_id = ?")
        .bind(provider)
        .bind(event_id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// One stored event's raw payload, as received and verified.
///
/// For replay: the body is kept verbatim precisely so it can be re-run, and
/// it is returned exactly as stored — re-serialising it would change the
/// bytes that were signed, which is the same trap the receive path avoids.
pub async fn webhook_payload(
    pool: &SqlitePool,
    provider: &str,
    event_id: &str,
) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT payload FROM webhook_events WHERE provider = ? AND event_id = ?",
    )
    .bind(provider)
    .bind(event_id)
    .fetch_optional(pool)
    .await
    .db_err()?;
    Ok(row.map(|(payload,)| payload))
}

/// Events that failed to process, for an operator to read.
pub async fn failed_webhooks(pool: &SqlitePool, limit: i64) -> Result<Vec<(String, String, String, String)>> {
    sqlx::query_as(
        "SELECT provider, event_id, event_type, COALESCE(error, '') FROM webhook_events \
         WHERE error IS NOT NULL ORDER BY received_at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .db_err()
}

pub async fn purge_webhooks(pool: &SqlitePool, days: i64) -> Result<u64> {
    let r = sqlx::query(
        "DELETE FROM webhook_events WHERE received_at < ? AND error IS NULL",
    )
    .bind(pro_core::days_from_now(-days))
    .execute(pool)
    .await
    .db_err()?;
    Ok(r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn pool() -> SqlitePool {
        crate::pool::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn records_and_filters() {
        let pool = pool().await;
        record(&pool, "staff:1", "tenant.suspended", Some("t1"), None, json!({"reason": "unpaid"}))
            .await;
        record(&pool, "account:9", "domain.added", Some("t1"), Some("a9"), json!({})).await;
        record(&pool, "staff:1", "settings.changed", None, None, json!({"key": "billing.provider"}))
            .await;

        assert_eq!(list(&pool, None, None, 50, 0).await.unwrap().len(), 3);
        assert_eq!(list(&pool, Some("t1"), None, 50, 0).await.unwrap().len(), 2);
        assert_eq!(list(&pool, None, Some("staff:1"), 50, 0).await.unwrap().len(), 2);

        let first = &list(&pool, Some("t1"), Some("staff:1"), 50, 0).await.unwrap()[0];
        assert_eq!(first.action, "tenant.suspended");
        assert_eq!(first.meta["reason"], "unpaid");
    }

    /// An audit failure must never fail the customer's action.
    #[tokio::test]
    async fn recording_never_panics() {
        let pool = pool().await;
        // Drop the table out from under it: the call must still return.
        sqlx::query("DROP TABLE audit_events").execute(&pool).await.unwrap();
        record(&pool, "system", "tenant.created", None, None, json!({})).await;
    }

    /// The core of webhook safety: a retried event must not be acted on
    /// twice, or a provider's redelivery grants a second subscription.
    #[tokio::test]
    async fn a_repeated_event_is_claimed_once() {
        let pool = pool().await;
        assert_eq!(
            claim_webhook(&pool, "stripe", "evt_1", "customer.subscription.updated", "{}")
                .await
                .unwrap(),
            WebhookClaim::Fresh
        );
        assert_eq!(
            claim_webhook(&pool, "stripe", "evt_1", "customer.subscription.updated", "{}")
                .await
                .unwrap(),
            WebhookClaim::Duplicate
        );
    }

    /// Two providers can mint the same event id, and they are different
    /// events.
    #[tokio::test]
    async fn event_ids_are_scoped_per_provider() {
        let pool = pool().await;
        assert_eq!(claim_webhook(&pool, "stripe", "1", "a", "{}").await.unwrap(), WebhookClaim::Fresh);
        assert_eq!(claim_webhook(&pool, "paddle", "1", "a", "{}").await.unwrap(), WebhookClaim::Fresh);
        assert_eq!(
            claim_webhook(&pool, "lemonsqueezy", "1", "a", "{}").await.unwrap(),
            WebhookClaim::Fresh
        );
    }

    /// With no id there is nothing to deduplicate on, and pretending
    /// otherwise would drop every such event after the first.
    #[tokio::test]
    async fn an_event_without_an_id_is_always_fresh() {
        let pool = pool().await;
        assert_eq!(claim_webhook(&pool, "paddle", "", "t", "{}").await.unwrap(), WebhookClaim::Fresh);
        assert_eq!(claim_webhook(&pool, "paddle", "", "t", "{}").await.unwrap(), WebhookClaim::Fresh);
    }

    /// A transient failure must not permanently swallow the event — the
    /// provider's retry has to get another go.
    #[tokio::test]
    async fn a_released_claim_can_be_retried() {
        let pool = pool().await;
        claim_webhook(&pool, "stripe", "evt_2", "t", "{}").await.unwrap();
        release_webhook(&pool, "stripe", "evt_2").await.unwrap();
        assert_eq!(
            claim_webhook(&pool, "stripe", "evt_2", "t", "{}").await.unwrap(),
            WebhookClaim::Fresh
        );
    }

    #[tokio::test]
    async fn records_a_processing_failure_for_an_operator() {
        let pool = pool().await;
        claim_webhook(&pool, "stripe", "evt_3", "invoice.paid", "{}").await.unwrap();
        finish_webhook(&pool, "stripe", "evt_3", Some("no such account")).await.unwrap();

        let failed = failed_webhooks(&pool, 10).await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].1, "evt_3");
        assert_eq!(failed[0].3, "no such account");
    }

    /// Purging keeps the failures, which are the ones worth reading.
    #[tokio::test]
    async fn purges_handled_events_only() {
        let pool = pool().await;
        claim_webhook(&pool, "stripe", "ok", "t", "{}").await.unwrap();
        claim_webhook(&pool, "stripe", "bad", "t", "{}").await.unwrap();
        finish_webhook(&pool, "stripe", "ok", None).await.unwrap();
        finish_webhook(&pool, "stripe", "bad", Some("boom")).await.unwrap();
        sqlx::query("UPDATE webhook_events SET received_at = ?")
            .bind(pro_core::days_from_now(-90))
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(purge_webhooks(&pool, 30).await.unwrap(), 1);
        assert_eq!(failed_webhooks(&pool, 10).await.unwrap().len(), 1, "the failure survives");
    }

    /// A payload is unbounded and attacker-adjacent; the stored copy is for
    /// replay, not archival.
    #[tokio::test]
    async fn truncates_a_huge_payload() {
        let pool = pool().await;
        claim_webhook(&pool, "stripe", "big", "t", &"x".repeat(200_000)).await.unwrap();
        let stored: String = sqlx::query_scalar("SELECT payload FROM webhook_events WHERE event_id = 'big'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored.len(), 64_000);
    }

    #[tokio::test]
    async fn purges_old_audit_entries() {
        let pool = pool().await;
        record(&pool, "system", "tenant.created", None, None, json!({})).await;
        sqlx::query("UPDATE audit_events SET created_at = ?")
            .bind(pro_core::days_from_now(-500))
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(purge(&pool, 365).await.unwrap(), 1);
    }
}
