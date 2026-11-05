//! The background loops.
//!
//! Three of them, deliberately separate because they want different cadences
//! and a slow one must not delay a fast one:
//!
//! * **jobs** — the durable queue: DNS checks, backups, deletions, storage
//!   measurement. Claimed one at a time from the database, so a restart loses
//!   nothing.
//! * **hibernation** — stop sites with no traffic. Frequent and cheap.
//! * **housekeeping** — dunning, expired trials, purges. Hourly; each pass is
//!   a handful of queries.
//!
//! Every loop is written so that a failure in one iteration is logged and the
//! loop continues. A platform where the dunning sweep stops because one
//! customer's record is odd is worse than one that skips that customer and
//! says so.
use pro_core::{Result, TenantStatus};
use std::time::Duration;

use crate::state::AppState;

/// Claim and run jobs, forever.
pub async fn run_jobs(state: AppState) {
    // Anything left `running` by a previous process would never be claimed
    // again, because claiming only looks at `pending`.
    if let Err(e) = pro_db::jobs::requeue_orphans(&state.db).await {
        tracing::error!("could not recover interrupted jobs: {e}");
    }

    loop {
        match pro_db::jobs::claim(&state.db).await {
            Ok(Some(job)) => {
                let started = std::time::Instant::now();
                let outcome = run_job(&state, &job).await;
                match outcome {
                    Ok(()) => {
                        tracing::debug!(
                            job = %job.kind,
                            "completed in {} ms",
                            started.elapsed().as_millis()
                        );
                        if let Err(e) = pro_db::jobs::complete(&state.db, &job.id).await {
                            tracing::error!("could not mark job {} done: {e}", job.id);
                        }
                    }
                    Err(e) => {
                        if let Err(inner) = pro_db::jobs::fail(&state.db, &job, &e.to_string()).await
                        {
                            tracing::error!("could not record job failure: {inner}");
                        }
                    }
                }
                // Straight on to the next: a queue with work in it should
                // drain rather than wait.
                continue;
            }
            Ok(None) => {}
            Err(e) => tracing::error!("could not claim a job: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_job(state: &AppState, job: &pro_db::jobs::Job) -> Result<()> {
    match job.kind.as_str() {
        "verify_domain" => verify_domain_job(state, job).await,
        "backup_tenant" => backup_job(state, job).await,
        "restore_tenant" => restore_job(state, job).await,
        "delete_tenant" => delete_job(state, job).await,
        "sample_usage" => sample_usage_job(state, job).await,
        other => {
            // A job kind this version does not know is a downgrade, not a
            // transient failure — retrying it would never help.
            tracing::warn!("ignoring unknown job kind {other}");
            Ok(())
        }
    }
}

async fn verify_domain_job(state: &AppState, job: &pro_db::jobs::Job) -> Result<()> {
    let Some(domain_id) = job.payload["domain_id"].as_str() else {
        return Err(pro_core::ProError::BadRequest("job has no domain_id".into()));
    };
    let domain = match pro_db::domains::get(&state.db, domain_id).await {
        Ok(d) => d,
        // Removed while the job was queued: nothing to do, and not a failure.
        Err(_) => return Ok(()),
    };
    let verdict = crate::verify_domain_now(state, &domain).await?;

    // Still waiting on DNS? Come back later. Re-queued with a delay rather
    // than retried as a failure, because "not propagated yet" is the expected
    // state for the first hour and is not an error worth alarming anybody
    // about.
    if !verdict.is_servable() {
        let attempts = job.payload["attempt"].as_i64().unwrap_or(0) + 1;
        // Roughly: every 2 minutes for the first while, then every 30, giving
        // up after a day. A customer who takes longer than that uses the
        // "Check now" button.
        if attempts <= 48 {
            let delay = if attempts <= 10 { 120 } else { 1800 };
            let run_after = (chrono::Utc::now() + chrono::Duration::seconds(delay))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            pro_db::jobs::enqueue_at(
                &state.db,
                "verify_domain",
                job.tenant_id.as_deref(),
                serde_json::json!({"domain_id": domain_id, "attempt": attempts}),
                &run_after,
            )
            .await?;
        } else {
            tracing::info!(
                domain = %domain.domain,
                "stopped checking DNS after {attempts} attempts"
            );
        }
    }
    Ok(())
}

async fn backup_job(state: &AppState, job: &pro_db::jobs::Job) -> Result<()> {
    let Some(tenant_id) = job.tenant_id.as_deref() else {
        return Err(pro_core::ProError::BadRequest("backup job has no tenant".into()));
    };
    let tenant = pro_db::tenants::get(&state.db, tenant_id).await?;
    let dir = state.config.tenant_dir(&tenant.id);
    if !dir.exists() {
        return Ok(());
    }

    let reason = job.payload["reason"].as_str().unwrap_or("auto").to_string();
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let archive = state
        .config
        .backup_dir()
        .join(&tenant.slug)
        .join(format!("{}-{stamp}.zip", tenant.slug));

    // A consistent copy of the database first, taken while the site keeps
    // serving — a plain file copy of a live SQLite database can miss committed
    // transactions still in its WAL.
    let snapshot_path = state
        .config
        .data_dir()
        .join("tmp")
        .join(format!("{}-{stamp}.db", tenant.id));
    let snapshot = pro_provision::snapshot_database(&dir, &snapshot_path).await?;

    let size = pro_provision::pack(&dir, &archive, snapshot.clone()).await?;

    if let Some(path) = snapshot {
        let _ = tokio::fs::remove_file(path).await;
    }

    pro_db::backups::record(
        &state.db,
        &tenant.id,
        &archive.to_string_lossy(),
        size as i64,
        &reason,
    )
    .await?;

    tracing::info!(
        tenant = %tenant.slug,
        "backed up {} to {}",
        pro_core::plans::bytes_display(size as i64),
        archive.display()
    );

    // Keep the last few. Unbounded backups fill a disk, and the oldest copy of
    // a site is rarely the one anybody wants.
    prune_backups(state, &tenant.id, 5).await;
    Ok(())
}

async fn prune_backups(state: &AppState, tenant_id: &str, keep: i64) {
    let old = match pro_db::backups::take_pruned(&state.db, tenant_id, keep).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("could not list old backups: {e}");
            return;
        }
    };

    for (id, path) in old {
        let _ = tokio::fs::remove_file(&path).await;
        let _ = pro_db::backups::remove(&state.db, &id).await;
    }
}

async fn delete_job(state: &AppState, job: &pro_db::jobs::Job) -> Result<()> {
    let Some(tenant_id) = job.tenant_id.as_deref() else {
        return Err(pro_core::ProError::BadRequest("delete job has no tenant".into()));
    };
    let tenant = pro_db::tenants::get(&state.db, tenant_id).await?;

    // Re-checked here rather than trusted from the queue: a customer may have
    // cancelled the deletion after the job was scheduled, and acting on a
    // stale instruction would destroy a site somebody chose to keep.
    if tenant.status != TenantStatus::PendingDeletion {
        tracing::info!(tenant = %tenant.slug, "deletion no longer scheduled; skipping");
        return Ok(());
    }
    if !pro_core::has_passed(tenant.delete_after.as_deref()) {
        tracing::info!(tenant = %tenant.slug, "deletion grace period has not expired; skipping");
        return Ok(());
    }

    pro_provision::delete_site_files(&state.config, &state.supervisor, &tenant).await?;
    pro_db::tenants::mark_deleted(&state.db, &tenant.id).await?;
    state.hosts.invalidate_all();

    pro_db::audit::record(
        &state.db,
        "system",
        "tenant.deleted",
        Some(&tenant.id),
        Some(&tenant.account_id),
        serde_json::json!({"slug": tenant.slug}),
    )
    .await;
    Ok(())
}

/// Bring a site back from one of its archives.
///
/// Operator-initiated only: restoring overwrites whatever the site's
/// directory would hold, and the restore refuses to run while the directory
/// exists — so in practice this is for a site whose files are already gone,
/// such as one past its deletion window. The tenant row surviving its files
/// is what makes resurrection possible at all.
async fn restore_job(state: &AppState, job: &pro_db::jobs::Job) -> Result<()> {
    let Some(tenant_id) = job.tenant_id.as_deref() else {
        return Err(pro_core::ProError::BadRequest("restore job has no tenant".into()));
    };
    let Some(backup_id) = job.payload["backup_id"].as_str() else {
        return Err(pro_core::ProError::BadRequest("restore job has no backup_id".into()));
    };
    let backup = pro_db::backups::get(&state.db, backup_id).await?;
    if backup.tenant_id != tenant_id {
        return Err(pro_core::ProError::BadRequest(
            "that archive belongs to a different site".into(),
        ));
    }
    let tenant = pro_db::tenants::get(&state.db, tenant_id).await?;

    // Checked here rather than trusted from the button: a site whose files
    // came back — a cancelled deletion, an earlier restore — turns this job
    // into an overwrite, and no overwrite is wanted. Skipping is not failing:
    // the situation is not an error, and retrying would never help.
    if state.config.tenant_dir(&tenant.id).exists() {
        tracing::info!(tenant = %tenant.slug, "restore skipped: site has files");
        return Ok(());
    }

    pro_provision::restore_site(
        &state.config,
        &state.supervisor,
        &tenant,
        std::path::Path::new(&backup.path),
    )
    .await?;

    // A restored site comes back active: whatever scheduled its deletion or
    // suspension is a fact about the lost directory, not about this one.
    pro_db::tenants::clear_deletion(&state.db, &tenant.id).await?;
    state.hosts.invalidate_all();

    // Re-apply the plan's host settings, so a site restored from before a
    // plan change does not keep serving with the old limits.
    if let Some(plan) = state.config.plan(&tenant.plan_slug) {
        let tenant = pro_db::tenants::get(&state.db, tenant_id).await?;
        pro_provision::apply_plan(&state.db, &state.config, &state.supervisor, &tenant, plan)
            .await?;
    }

    pro_db::audit::record(
        &state.db,
        "system",
        "tenant.restored",
        Some(&tenant.id),
        Some(&tenant.account_id),
        serde_json::json!({"backup": backup.id, "archive": backup.path}),
    )
    .await;
    Ok(())
}

/// Take a usage reading for one tenant.
///
/// The counts come from the tenant's **own API** over loopback, not from
/// opening its database. That keeps the control plane free of any knowledge of
/// the CMS's schema — the coupling this whole design exists to avoid — and it
/// is why a hibernating site simply keeps its previous reading rather than
/// being woken to be counted.
async fn sample_usage_job(state: &AppState, job: &pro_db::jobs::Job) -> Result<()> {
    let Some(tenant_id) = job.tenant_id.as_deref() else {
        return Err(pro_core::ProError::BadRequest("sample job has no tenant".into()));
    };
    let tenant = pro_db::tenants::get(&state.db, tenant_id).await?;

    // Storage is measured from the filesystem, which works whether the site is
    // running or not.
    let storage = pro_provision::measure_tenant(&state.db, &state.config, tenant_id).await?;

    // Waking a site purely to count its members would defeat hibernation, so
    // the rest is only sampled when it happens to be up.
    if !state.supervisor.is_running(&tenant.id) {
        let previous = pro_db::usage::latest(&state.db, &tenant.id).await?;
        pro_db::usage::record(
            &state.db,
            &tenant.id,
            previous.as_ref().map(|p| p.members).unwrap_or(0),
            previous.as_ref().map(|p| p.posts).unwrap_or(0),
            storage as i64,
            previous.as_ref().map(|p| p.emails_sent).unwrap_or(0),
        )
        .await?;
        return Ok(());
    }

    // Ask the site itself. If the answer does not come back, the previous
    // reading stands: a stale figure on a dashboard is a far smaller problem
    // than a zero that looks like the site emptied overnight.
    let counts = match (&tenant.platform_key_id, &tenant.platform_key_secret) {
        (Some(id), Some(secret)) => {
            tenant_usage(tenant.port, id, secret, &tenant.period_start).await
        }
        _ => None,
    };
    let previous = pro_db::usage::latest(&state.db, &tenant.id).await?;
    pro_db::usage::record(
        &state.db,
        &tenant.id,
        counts.as_ref().map(|c| c.members).unwrap_or_else(|| previous.as_ref().map(|p| p.members).unwrap_or(0)),
        counts.as_ref().map(|c| c.posts).unwrap_or_else(|| previous.as_ref().map(|p| p.posts).unwrap_or(0)),
        storage as i64,
        counts.as_ref().map(|c| c.emails).unwrap_or_else(|| previous.as_ref().map(|p| p.emails_sent).unwrap_or(0)),
    )
    .await?;
    Ok(())
}

/// What a site reports about itself.
struct TenantCounts {
    members: i64,
    posts: i64,
    emails: i64,
}

/// Ask a running tenant how much it holds, over its own Admin API.
///
/// The platform signs each request as the integration it created at setup —
/// the same `Authorization: Ghost <jwt>` scheme every Ghost integration uses,
/// minted per call because the tokens live five minutes. A missing or
/// revoked key returns `None`, and the caller keeps the previous reading
/// rather than recording zeroes.
async fn tenant_usage(
    port: u16,
    key_id: &str,
    key_secret: &str,
    period_start: &str,
) -> Option<TenantCounts> {
    let token = mint_admin_token(key_id, key_secret)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let base = format!("http://127.0.0.1:{port}/ghost/api/admin");
    let auth = format!("Ghost {token}");

    // Members: the site's own totals, free and paying alike.
    let members: i64 = {
        let body: serde_json::Value = client
            .get(format!("{base}/stats/member_count/"))
            .header("Authorization", &auth)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        let totals = &body["meta"]["totals"];
        ["free", "paid", "comped"]
            .iter()
            .map(|k| totals[*k].as_i64().unwrap_or(0))
            .sum()
    };

    // Posts: every status, counted by the paginator rather than fetched.
    let posts: i64 = {
        let body: serde_json::Value = client
            .get(format!("{base}/posts/?limit=1"))
            .header("Authorization", &auth)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        body["meta"]["pagination"]["total"].as_i64().unwrap_or(0)
    };

    // Emails: recipients since the billing period began, which is the same
    // window the plan's email ceiling is enforced over — so the meter shows
    // what the limit actually counts.
    let emails: i64 = {
        let from = period_start.get(..10).unwrap_or(period_start);
        let to = pro_core::now_iso();
        let to = to.get(..10).unwrap_or("");
        let body: serde_json::Value = client
            .get(format!("{base}/stats/newsletter-stats/?date_from={from}&date_to={to}"))
            .header("Authorization", &auth)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        body["stats"]
            .as_array()
            .map(|rows| rows.iter().map(|r| r["sent"].as_i64().unwrap_or(0)).sum())
            .unwrap_or(0)
    };

    Some(TenantCounts { members, posts, emails })
}

/// Mint the short-lived token the Admin API expects, in Ghost's own shape.
///
/// The signing key is the stored secret **decoded from hex into bytes** —
/// signing the hex text would produce a token every Ghost client rejects —
/// and `aud` names the API being called, which is what stops a Content API
/// token being replayed here.
fn mint_admin_token(key_id: &str, key_secret: &str) -> Option<String> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = serde_json::json!({"alg": "HS256", "typ": "JWT", "kid": key_id});
    let claims = serde_json::json!({
        "iat": chrono::Utc::now().timestamp(),
        "exp": chrono::Utc::now().timestamp() + 5 * 60,
        "aud": "/ghost/api/admin/",
    });
    let header_b64 = b64.encode(serde_json::to_string(&header).ok()?);
    let claims_b64 = b64.encode(serde_json::to_string(&claims).ok()?);

    let secret = hex::decode(key_secret).ok()?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret).ok()?;
    mac.update(format!("{header_b64}.{claims_b64}").as_bytes());
    let sig_b64 = b64.encode(mac.finalize().into_bytes());

    Some(format!("{header_b64}.{claims_b64}.{sig_b64}"))
}


/// Stop sites that have had no traffic for a while.
pub async fn run_hibernation(state: AppState) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;

        // Notice crashed processes first, so the proxy does not forward to a
        // port nothing is listening on.
        let gone = state.supervisor.reap_exited().await;
        for id in &gone {
            tracing::warn!(tenant = %id, "tenant process had exited; it will restart on the next visit");
        }

        let live = state.live();
        let idle_timeout = live.idle_timeout_secs();
        if idle_timeout == 0 {
            continue;
        }

        let tenants = match pro_db::tenants::list(&state.db, None, None, 5_000, 0).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("hibernation sweep could not list sites: {e}");
                continue;
            }
        };

        for id in state.supervisor.idle_candidates(&tenants, idle_timeout) {
            tracing::info!(tenant = %id, "hibernating an idle site");
            if let Err(e) = state.supervisor.stop(&id).await {
                tracing::warn!(tenant = %id, "could not hibernate: {e}");
            }
        }
    }
}

/// Dunning, trials, deletions and purges.
pub async fn run_housekeeping(state: AppState) {
    // A short delay before the first pass, so a restart does not do all of
    // this while the platform is still coming up.
    tokio::time::sleep(Duration::from_secs(30)).await;

    loop {
        if let Err(e) = housekeeping_pass(&state).await {
            tracing::error!("housekeeping pass failed: {e}");
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

async fn housekeeping_pass(state: &AppState) -> Result<()> {
    let live = state.live();

    // ---- suspend sites past their dunning grace period --------------------
    let grace = live.dunning_grace_days();
    for (subscription, tenant_id) in
        pro_db::subscriptions::past_due_beyond_grace(&state.db, grace).await?
    {
        let Ok(tenant) = pro_db::tenants::get(&state.db, &tenant_id).await else { continue };
        tracing::info!(
            tenant = %tenant.slug,
            "suspending: payment outstanding for more than {grace} days"
        );

        if let Err(e) = pro_provision::suspend_site(
            &state.db,
            &state.config,
            &state.supervisor,
            &tenant,
            pro_core::suspension::PAYMENT_OUTSTANDING,
        )
        .await
        {
            tracing::error!(tenant = %tenant.slug, "could not suspend: {e}");
            continue;
        }

        if let Ok(account) = pro_db::accounts::get(&state.db, &subscription.account_id).await {
            let _ = state
                .mailer()
                .send(pro_mail::templates::site_suspended(
                    &live.platform_name(),
                    &account.email,
                    &account.name,
                    &state.config.billing_url(),
                ))
                .await;
        }

        pro_db::audit::record(
            &state.db,
            "system",
            "tenant.suspended",
            Some(&tenant.id),
            Some(&subscription.account_id),
            serde_json::json!({"reason": "dunning", "grace_days": grace}),
        )
        .await;
    }

    // ---- trials that have run out ----------------------------------------
    for (subscription, tenant_id) in pro_db::subscriptions::expired_trials(&state.db).await? {
        let Ok(tenant) = pro_db::tenants::get(&state.db, &tenant_id).await else { continue };
        // A trial ending is not a payment failure: it moves the subscription
        // into dunning, which gives the customer the same grace period and the
        // same chance to add a card.
        pro_db::subscriptions::upsert(
            &state.db,
            pro_db::subscriptions::Upsert {
                account_id: &subscription.account_id,
                tenant_id: Some(&tenant.id),
                provider: &subscription.provider,
                provider_subscription_id: subscription.provider_subscription_id.as_deref(),
                status: Some("past_due"),
                ..Default::default()
            },
        )
        .await?;

        if let Ok(account) = pro_db::accounts::get(&state.db, &subscription.account_id).await {
            let _ = state
                .mailer()
                .send(pro_mail::templates::trial_ending(
                    &live.platform_name(),
                    &account.email,
                    &account.name,
                    &state.config.billing_url(),
                    0,
                ))
                .await;
        }
        tracing::info!(tenant = %tenant.slug, "trial ended");
    }

    // ---- sites whose deletion window has closed --------------------------
    for tenant in pro_db::tenants::due_for_deletion(&state.db).await? {
        pro_db::jobs::enqueue_unique(
            &state.db,
            "delete_tenant",
            Some(&tenant.id),
            serde_json::json!({}),
        )
        .await?;
    }

    // ---- custom domains still waiting on DNS ------------------------------
    //
    // The check button's own retry ladder gives up after about a day; a
    // customer who set their DNS a day *after* attaching the domain would
    // otherwise wait forever for a result they cannot force. The sweep
    // restarts the ladder for anything still unresolved, hourly, and
    // `enqueue_unique` keeps it from stacking on a ladder still climbing.
    for domain in pro_db::domains::pending_verification(&state.db, 200).await? {
        let _ = pro_db::jobs::enqueue_unique(
            &state.db,
            "verify_domain",
            Some(&domain.tenant_id),
            serde_json::json!({"domain_id": domain.id, "attempt": 0}),
        )
        .await?;
    }

    // ---- a daily usage reading per site ----------------------------------
    //
    // Queued rather than done here, so one slow directory walk does not hold
    // up the rest of the pass.
    let active = pro_db::tenants::list(&state.db, Some(TenantStatus::Active), None, 5_000, 0).await?;
    for tenant in &active {
        let due = match pro_db::usage::latest(&state.db, &tenant.id).await? {
            Some(sample) => pro_core::parse_iso(&sample.sampled_at)
                .map(|at| chrono::Utc::now().signed_duration_since(at).num_hours() >= 24)
                .unwrap_or(true),
            None => true,
        };
        if due {
            pro_db::jobs::enqueue_unique(
                &state.db,
                "sample_usage",
                Some(&tenant.id),
                serde_json::json!({}),
            )
            .await?;
        }
    }

    // ---- nightly backups --------------------------------------------------
    for tenant in &active {
        let last = pro_db::backups::last_backup_at(&state.db, &tenant.id).await?;

        let due = last
            .and_then(|at| pro_core::parse_iso(&at))
            .map(|at| chrono::Utc::now().signed_duration_since(at).num_hours() >= 24)
            .unwrap_or(true);
        if due {
            pro_db::jobs::enqueue_unique(
                &state.db,
                "backup_tenant",
                Some(&tenant.id),
                serde_json::json!({"reason": "auto"}),
            )
            .await?;
        }
    }

    // ---- purges ----------------------------------------------------------
    //
    // None of these affect correctness; they stop tables growing without
    // bound. Failures are logged and ignored.
    let _ = pro_db::sessions::purge_expired(&state.db).await;
    let _ = pro_db::tokens::purge(&state.db).await;
    let _ = pro_db::jobs::purge_done(&state.db, 14).await;
    let _ = pro_db::audit::purge_webhooks(&state.db, 60).await;
    let _ = pro_db::usage::purge(&state.db, 120).await;
    let _ = pro_db::audit::purge(&state.db, 400).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The token must be the shape Ghost's Admin API verifies: three parts,
    /// an HS256 header naming the key, and a signature over `header.claims`
    /// made with the secret's bytes, not its hex text.
    #[test]
    fn a_minted_token_matches_ghosts_scheme() {
        use base64::Engine;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let secret_hex = "0123456789abcdef".repeat(4);
        let token = mint_admin_token("key_abc", &secret_hex).unwrap();

        let mut parts = token.split('.');
        let (h, c, s) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
        assert!(parts.next().is_none(), "exactly three parts");

        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header: serde_json::Value =
            serde_json::from_slice(&b64.decode(h).unwrap()).unwrap();
        assert_eq!(header["alg"], "HS256");
        assert_eq!(header["kid"], "key_abc");

        let claims: serde_json::Value =
            serde_json::from_slice(&b64.decode(c).unwrap()).unwrap();
        assert_eq!(claims["aud"], "/ghost/api/admin/");
        assert!(claims["iat"].is_i64() && claims["exp"].is_i64());

        let mut mac = Hmac::<Sha256>::new_from_slice(&hex::decode(&secret_hex).unwrap())
            .unwrap();
        mac.update(format!("{h}.{c}").as_bytes());
        assert_eq!(b64.encode(mac.finalize().into_bytes()), s, "signature verifies");
    }

    /// A secret that is not hex yields no token, not a broken one; the
    /// sampler's caller keeps the previous reading instead.
    #[test]
    fn a_malformed_secret_mints_nothing() {
        assert!(mint_admin_token("k", "not-hex-!!").is_none());
        assert!(mint_admin_token("k", "abc").is_none(), "odd length");
    }
}
