//! The background work queue.
//!
//! A table rather than an in-memory channel, because the work here is not
//! cheap to lose: provisioning a site takes seconds and involves creating
//! directories and running migrations, and a process that dies halfway must
//! leave something an operator can see and retry. An in-memory queue would
//! lose the job and leave the half-built directory with nobody responsible for
//! it.
//!
//! Claiming is done with a conditional UPDATE rather than a `SELECT` followed
//! by an `UPDATE`. That matters even on SQLite: two workers that both read the
//! same pending row would both run it, and provisioning a tenant twice means
//! two processes binding one port.
use pro_core::{ids, now_iso, DbErrorExt, Result};
use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// How many times a job is retried before it is left alone for an operator.
///
/// Five, with the backoff below, spans about half an hour — long enough to
/// ride out a provider being briefly unreachable, short enough that a real
/// failure surfaces the same working day.
pub const MAX_ATTEMPTS: i64 = 5;

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub tenant_id: Option<String>,
    pub payload: Value,
    pub status: String,
    pub attempts: i64,
    pub run_after: String,
    pub last_error: Option<String>,
    pub created_at: String,
}

fn map(row: &SqliteRow) -> Job {
    Job {
        id: row.get("id"),
        kind: row.get("kind"),
        tenant_id: row.get("tenant_id"),
        // A payload that will not parse is handed to the worker as null
        // rather than failing the claim — the job is already recorded, and a
        // worker that cannot understand its payload fails it with a readable
        // error.
        payload: serde_json::from_str(&row.get::<String, _>("payload")).unwrap_or(Value::Null),
        status: row.get("status"),
        attempts: row.get("attempts"),
        run_after: row.get("run_after"),
        last_error: row.get("last_error"),
        created_at: row.get("created_at"),
    }
}

const COLS: &str = "id, kind, tenant_id, payload, status, attempts, run_after, last_error, \
                    created_at";

/// Queue a job to run as soon as a worker is free.
pub async fn enqueue(
    pool: &SqlitePool,
    kind: &str,
    tenant_id: Option<&str>,
    payload: Value,
) -> Result<String> {
    enqueue_at(pool, kind, tenant_id, payload, &now_iso()).await
}

/// Queue a job for later — a retry, or tonight's backup.
pub async fn enqueue_at(
    pool: &SqlitePool,
    kind: &str,
    tenant_id: Option<&str>,
    payload: Value,
    run_after: &str,
) -> Result<String> {
    let id = ids::object_id();
    sqlx::query(
        "INSERT INTO jobs (id, kind, tenant_id, payload, status, run_after, created_at) \
         VALUES (?, ?, ?, ?, 'pending', ?, ?)",
    )
    .bind(&id)
    .bind(kind)
    .bind(tenant_id)
    .bind(serde_json::to_string(&payload)?)
    .bind(run_after)
    .bind(now_iso())
    .execute(pool)
    .await
    .db_err()?;
    Ok(id)
}

/// Queue a job unless one of the same kind is already outstanding for this
/// tenant.
///
/// Used for the jobs it makes no sense to have two of: measuring one tenant's
/// storage twice tells us nothing, and two concurrent backups of one directory
/// race each other.
pub async fn enqueue_unique(
    pool: &SqlitePool,
    kind: &str,
    tenant_id: Option<&str>,
    payload: Value,
) -> Result<Option<String>> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM jobs WHERE kind = ? AND status IN ('pending','running') \
         AND COALESCE(tenant_id, '') = COALESCE(?, '') LIMIT 1",
    )
    .bind(kind)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .db_err()?;
    if existing.is_some() {
        return Ok(None);
    }
    enqueue(pool, kind, tenant_id, payload).await.map(Some)
}

/// Take the next due job, marking it running in the same statement.
///
/// The `status = 'pending'` in the WHERE clause is the lock: two workers
/// racing on the same row produce one UPDATE that matches and one that does
/// not, and only the winner gets the job back.
pub async fn claim(pool: &SqlitePool) -> Result<Option<Job>> {
    let now = now_iso();
    loop {
        let candidate: Option<String> = sqlx::query_scalar(
            "SELECT id FROM jobs WHERE status = 'pending' AND run_after <= ? \
             ORDER BY run_after LIMIT 1",
        )
        .bind(&now)
        .fetch_optional(pool)
        .await
        .db_err()?;

        let Some(id) = candidate else { return Ok(None) };

        let claimed = sqlx::query(
            "UPDATE jobs SET status = 'running', attempts = attempts + 1, started_at = ? \
             WHERE id = ? AND status = 'pending'",
        )
        .bind(&now)
        .bind(&id)
        .execute(pool)
        .await
        .db_err()?;

        if claimed.rows_affected() == 1 {
            let row = sqlx::query(&format!("SELECT {COLS} FROM jobs WHERE id = ?"))
                .bind(&id)
                .fetch_one(pool)
                .await
                .db_err()?;
            return Ok(Some(map(&row)));
        }
        // Another worker took it between the select and the update; look again.
    }
}

pub async fn complete(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("UPDATE jobs SET status = 'done', finished_at = ?, last_error = NULL WHERE id = ?")
        .bind(now_iso())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// Record a failure, scheduling a retry if there are attempts left.
///
/// The backoff is exponential — roughly 1, 2, 4, 8, 16 minutes — which is the
/// right shape for the failures these jobs actually see: a provider's API
/// briefly unreachable, DNS not yet propagated, a disk momentarily full.
/// Retrying a DNS check every second would not make it propagate faster.
pub async fn fail(pool: &SqlitePool, job: &Job, error: &str) -> Result<()> {
    let give_up = job.attempts >= MAX_ATTEMPTS;
    if give_up {
        sqlx::query(
            "UPDATE jobs SET status = 'failed', finished_at = ?, last_error = ? WHERE id = ?",
        )
        .bind(now_iso())
        .bind(truncate(error))
        .bind(&job.id)
        .execute(pool)
        .await
        .db_err()?;
        tracing::error!(
            job = %job.id, kind = %job.kind, attempts = job.attempts,
            "giving up on job: {error}"
        );
    } else {
        let delay_secs = 60i64 * 2i64.pow((job.attempts.max(1) - 1).min(8) as u32);
        let run_after = (chrono::Utc::now() + chrono::Duration::seconds(delay_secs))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        sqlx::query(
            "UPDATE jobs SET status = 'pending', run_after = ?, last_error = ? WHERE id = ?",
        )
        .bind(&run_after)
        .bind(truncate(error))
        .bind(&job.id)
        .execute(pool)
        .await
        .db_err()?;
        tracing::warn!(
            job = %job.id, kind = %job.kind, attempt = job.attempts,
            "job failed, retrying after {delay_secs}s: {error}"
        );
    }
    Ok(())
}

/// An error message can be anything, including a whole response body. Keep
/// enough to diagnose and no more.
fn truncate(s: &str) -> String {
    s.chars().take(2000).collect()
}

/// Return jobs that were running when the process died.
///
/// Called once at boot. A `running` row with nobody running it would otherwise
/// sit there forever: nothing claims it, because claiming only looks at
/// `pending`. Jobs that have exhausted their attempts go straight to `failed`
/// so a crash loop cannot retry one indefinitely.
pub async fn requeue_orphans(pool: &SqlitePool) -> Result<u64> {
    let bumped = sqlx::query(
        "UPDATE jobs SET status = 'pending', last_error = 'interrupted by a restart' \
         WHERE status = 'running' AND attempts < ?",
    )
    .bind(MAX_ATTEMPTS)
    .execute(pool)
    .await
    .db_err()?;

    let abandoned = sqlx::query(
        "UPDATE jobs SET status = 'failed', finished_at = ?, \
         last_error = 'interrupted by a restart, out of attempts' \
         WHERE status = 'running' AND attempts >= ?",
    )
    .bind(now_iso())
    .bind(MAX_ATTEMPTS)
    .execute(pool)
    .await
    .db_err()?;

    let total = bumped.rows_affected() + abandoned.rows_affected();
    if total > 0 {
        tracing::info!(
            "recovered {} interrupted job(s) ({} requeued, {} abandoned)",
            total,
            bumped.rows_affected(),
            abandoned.rows_affected()
        );
    }
    Ok(total)
}

/// Jobs an operator should look at.
pub async fn list_failed(pool: &SqlitePool, limit: i64) -> Result<Vec<Job>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM jobs WHERE status = 'failed' ORDER BY created_at DESC LIMIT ?"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows.iter().map(map).collect())
}

/// Put a failed job back in the queue, resetting its attempts.
pub async fn retry(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE jobs SET status = 'pending', attempts = 0, run_after = ?, last_error = NULL, \
         finished_at = NULL WHERE id = ? AND status = 'failed'",
    )
    .bind(now_iso())
    .bind(id)
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

pub async fn count_by_status(pool: &SqlitePool) -> Result<Vec<(String, i64)>> {
    sqlx::query_as("SELECT status, COUNT(*) FROM jobs GROUP BY status")
        .fetch_all(pool)
        .await
        .db_err()
}

/// Delete finished jobs older than `days`, so the table does not grow without
/// bound. Failed jobs are kept: they are the ones worth reading.
pub async fn purge_done(pool: &SqlitePool, days: i64) -> Result<u64> {
    let r = sqlx::query("DELETE FROM jobs WHERE status = 'done' AND finished_at < ?")
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
    async fn queues_and_claims() {
        let pool = pool().await;
        enqueue(&pool, "provision_tenant", Some("t1"), json!({"slug": "alice"})).await.unwrap();

        let job = claim(&pool).await.unwrap().expect("a job is due");
        assert_eq!(job.kind, "provision_tenant");
        assert_eq!(job.tenant_id.as_deref(), Some("t1"));
        assert_eq!(job.payload["slug"], "alice");
        assert_eq!(job.attempts, 1, "claiming counts as an attempt");

        complete(&pool, &job.id).await.unwrap();
        assert!(claim(&pool).await.unwrap().is_none(), "nothing left");
    }

    /// The crux: a job must be handed to exactly one worker. Two workers
    /// provisioning one tenant means two processes binding one port.
    #[tokio::test]
    async fn a_job_is_claimed_only_once() {
        let pool = pool().await;
        enqueue(&pool, "backup_tenant", Some("t1"), json!({})).await.unwrap();
        assert!(claim(&pool).await.unwrap().is_some());
        assert!(claim(&pool).await.unwrap().is_none(), "already running");
    }

    #[tokio::test]
    async fn respects_the_schedule() {
        let pool = pool().await;
        enqueue_at(&pool, "backup_tenant", None, json!({}), &pro_core::days_from_now(1))
            .await
            .unwrap();
        assert!(claim(&pool).await.unwrap().is_none(), "not due yet");

        enqueue(&pool, "measure_storage", None, json!({})).await.unwrap();
        assert_eq!(claim(&pool).await.unwrap().unwrap().kind, "measure_storage");
    }

    /// Retries back off rather than hammering. A DNS check does not propagate
    /// faster for being asked every second.
    #[tokio::test]
    async fn failures_back_off_then_give_up() {
        let pool = pool().await;
        enqueue(&pool, "verify_domain", Some("t1"), json!({})).await.unwrap();

        let job = claim(&pool).await.unwrap().unwrap();
        fail(&pool, &job, "no A record yet").await.unwrap();
        // Rescheduled into the future, so it is not immediately re-claimable.
        assert!(claim(&pool).await.unwrap().is_none());

        // Walk it up to the attempt ceiling.
        for attempt in 1..MAX_ATTEMPTS {
            sqlx::query("UPDATE jobs SET run_after = ?")
                .bind(now_iso())
                .execute(&pool)
                .await
                .unwrap();
            let j = claim(&pool).await.unwrap().expect("due again");
            assert_eq!(j.attempts, attempt + 1);
            fail(&pool, &j, "still no A record").await.unwrap();
        }

        let failed = list_failed(&pool, 10).await.unwrap();
        assert_eq!(failed.len(), 1, "eventually left for an operator");
        assert_eq!(failed[0].attempts, MAX_ATTEMPTS);

        // An operator can put it back.
        retry(&pool, &failed[0].id).await.unwrap();
        let again = claim(&pool).await.unwrap().expect("requeued");
        assert_eq!(again.attempts, 1, "attempts reset");
    }

    /// A `running` row with nobody running it would never be claimed again,
    /// because claiming only looks at `pending`.
    #[tokio::test]
    async fn recovers_jobs_interrupted_by_a_restart() {
        let pool = pool().await;
        enqueue(&pool, "provision_tenant", Some("t1"), json!({})).await.unwrap();
        let job = claim(&pool).await.unwrap().unwrap();
        assert_eq!(job.status, "running");

        assert_eq!(requeue_orphans(&pool).await.unwrap(), 1);
        assert!(claim(&pool).await.unwrap().is_some(), "claimable again");
    }

    /// A crash loop must not retry one job forever.
    #[tokio::test]
    async fn an_exhausted_orphan_is_abandoned_not_requeued() {
        let pool = pool().await;
        let id = enqueue(&pool, "provision_tenant", Some("t1"), json!({})).await.unwrap();
        sqlx::query("UPDATE jobs SET status = 'running', attempts = ? WHERE id = ?")
            .bind(MAX_ATTEMPTS)
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();

        requeue_orphans(&pool).await.unwrap();
        assert!(claim(&pool).await.unwrap().is_none());
        assert_eq!(list_failed(&pool, 10).await.unwrap().len(), 1);
    }

    /// Two backups of one directory race each other; one is enough.
    #[tokio::test]
    async fn deduplicates_where_it_matters() {
        let pool = pool().await;
        assert!(enqueue_unique(&pool, "backup_tenant", Some("t1"), json!({})).await.unwrap().is_some());
        assert!(
            enqueue_unique(&pool, "backup_tenant", Some("t1"), json!({})).await.unwrap().is_none(),
            "one is already outstanding"
        );
        // A different tenant is a different job.
        assert!(enqueue_unique(&pool, "backup_tenant", Some("t2"), json!({})).await.unwrap().is_some());

        // Once it has run, another may be queued.
        let job = claim(&pool).await.unwrap().unwrap();
        complete(&pool, &job.id).await.unwrap();
        assert!(enqueue_unique(&pool, "backup_tenant", Some(&job.tenant_id.unwrap()), json!({}))
            .await
            .unwrap()
            .is_some());
    }

    /// An unparseable payload must not break the claim — the worker reports it
    /// instead.
    #[tokio::test]
    async fn survives_a_corrupt_payload() {
        let pool = pool().await;
        let id = enqueue(&pool, "verify_domain", None, json!({})).await.unwrap();
        sqlx::query("UPDATE jobs SET payload = '{broken' WHERE id = ?")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let job = claim(&pool).await.unwrap().expect("still claimable");
        assert!(job.payload.is_null());
    }

    /// Error text can be a whole response body; keep enough to diagnose.
    #[tokio::test]
    async fn truncates_huge_errors() {
        let pool = pool().await;
        enqueue(&pool, "verify_domain", None, json!({})).await.unwrap();
        let job = claim(&pool).await.unwrap().unwrap();
        fail(&pool, &job, &"x".repeat(50_000)).await.unwrap();
        let stored: String = sqlx::query_scalar("SELECT last_error FROM jobs WHERE id = ?")
            .bind(&job.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored.len(), 2000);
    }

    #[tokio::test]
    async fn purges_finished_work_but_keeps_failures() {
        let pool = pool().await;
        let done = enqueue(&pool, "measure_storage", None, json!({})).await.unwrap();
        let job = claim(&pool).await.unwrap().unwrap();
        complete(&pool, &job.id).await.unwrap();
        sqlx::query("UPDATE jobs SET finished_at = ? WHERE id = ?")
            .bind(pro_core::days_from_now(-60))
            .bind(&done)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(purge_done(&pool, 30).await.unwrap(), 1);
        assert_eq!(count_by_status(&pool).await.unwrap().len(), 0);
    }
}
