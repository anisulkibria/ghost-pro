//! Creating a site, reconfiguring it, and taking it away.
use pro_core::{Config, DomainKind, Plan, ProError, Result, Tenant, TenantStatus};
use pro_db::Db;
use pro_supervisor::Supervisor;
use std::path::Path;
use std::sync::Arc;

use crate::config_file;

/// What the dashboard collected to create a site.
pub struct NewSite<'a> {
    pub account_id: &'a str,
    /// Subdomain label, already checked for availability.
    pub slug: &'a str,
    /// The site's title.
    pub title: &'a str,
    pub plan: &'a Plan,
    /// The owner's name and email inside the new Ghost — normally the
    /// customer's own.
    pub owner_name: &'a str,
    pub owner_email: &'a str,
    /// The owner's password for *their site's* admin.
    ///
    /// Separate from their platform password on purpose: these are two
    /// different systems with two different user databases, and reusing one
    /// password across both would mean a compromise of either is a compromise
    /// of both. Ghost requires at least 10 characters.
    pub owner_password: &'a str,
    /// Where periodic limits count from — the subscription's start.
    pub period_start: &'a str,
}

/// Create a site, end to end, and leave it running.
///
/// On failure the tenant is marked `failed` rather than deleted: the
/// half-built directory needs clearing up, and an operator needs to see why.
/// Returning the error *and* leaving a record is the point.
pub async fn create_site(
    pool: &Db,
    config: &Arc<Config>,
    supervisor: &Supervisor,
    mail: &pro_core::config::MailConfig,
    new: NewSite<'_>,
) -> Result<Tenant> {
    // Ghost's own floor. Checked here so the failure arrives before a
    // directory and a database have been built, rather than at step five.
    if new.owner_password.len() < 10 {
        return Err(ProError::BadRequest(
            "The site password must be at least 10 characters.".into(),
        ));
    }
    config.slug_available(new.slug)?;

    // Claims the slug and a port in one transaction. From here on there is a
    // row, so every later failure is visible.
    let tenant = pro_db::tenants::create(
        pool,
        new.account_id,
        new.slug,
        new.title,
        &new.plan.slug,
        new.period_start,
        config.tenants.port_floor,
        config.tenants.port_ceiling,
    )
    .await?;

    match build_and_start(pool, config, supervisor, mail, &tenant, &new).await {
        Ok(()) => {
            pro_db::tenants::set_status(pool, &tenant.id, TenantStatus::Active).await?;
            pro_db::tenants::get(pool, &tenant.id).await
        }
        Err(e) => {
            tracing::error!(tenant = %tenant.slug, "provisioning failed: {e}");
            let _ = pro_db::tenants::set_status(pool, &tenant.id, TenantStatus::Failed).await;
            // Stop anything that did start, so a failed tenant is not holding
            // a port.
            let _ = supervisor.stop(&tenant.id).await;
            Err(e)
        }
    }
}

async fn build_and_start(
    pool: &Db,
    config: &Arc<Config>,
    supervisor: &Supervisor,
    mail: &pro_core::config::MailConfig,
    tenant: &Tenant,
    new: &NewSite<'_>,
) -> Result<()> {
    let dir = config.tenant_dir(&tenant.id);
    create_tree(&dir).await?;
    install_themes(config, &dir).await?;

    // The platform subdomain, which is live immediately and is this tenant's
    // canonical address until the customer attaches their own domain.
    let host = config.tenant_host(&tenant.slug);
    pro_db::domains::add(pool, &tenant.id, &host, DomainKind::Subdomain, true).await?;

    // On a MySQL deployment the site gets its own database and its own login
    // before anything is written. Done here, not lazily at first boot, so that
    // a server that refuses the grant fails this provision — which is recorded
    // and retryable — rather than leaving a site that starts and then cannot
    // save a post.
    let store = config.tenant_store()?;
    let creds = match store {
        pro_core::TenantStore::SqliteFile => None,
        pro_core::TenantStore::MySqlServer { .. } => {
            Some(crate::tenant_db::create(&store, &tenant.id).await?)
        }
    };

    let mut body = config_file::build(
        &config.tenant_url(&tenant.slug),
        tenant.port,
        new.plan,
        new.period_start,
        &config.billing_url(),
        mail,
        &config_file::database_block(creds.as_ref()),
    );

    // Bootstrap config: the `customIntegrations` flag is lifted for the site's
    // first moments, because the platform's own integration — the key it will
    // sample usage with forever after — is created in that window, and a
    // starter plan that withholds integrations from the *customer* must not
    // withhold them from the host. The plan is applied in full below, which
    // rewrites the config and restarts the site; what this relaxes is one key
    // for the length of a setup, on a site nobody knows exists yet.
    if let Some(limits) = body["hostSettings"]["limits"].as_object_mut() {
        limits.remove("customIntegrations");
    }
    config_file::write(&dir, &config.tenants.env, &body).await?;

    // Boots, runs its migrations, and binds its port.
    supervisor.ensure_running(tenant).await?;

    complete_ghost_setup(tenant.port, new).await?;

    // While the owner's credentials are still in hand from setup, mint the
    // platform's own long-lived way in: a custom integration whose admin key
    // is kept in the control plane. After this returns, the credentials are
    // forgotten and the key is the only access.
    match create_platform_integration(tenant.port, new).await {
        Ok((key_id, secret)) => {
            pro_db::tenants::set_platform_key(pool, &tenant.id, Some(&key_id), Some(&secret)).await?;
        }
        Err(e) => {
            // Not fatal: the site works, and only the platform's usage
            // sampling misses out. Logged loudly because it should not
            // quietly become the norm.
            tracing::warn!(
                "could not create the platform integration on {}: {e}",
                tenant.slug
            );
        }
    }

    // The plan in full, restoring whatever bootstrap lifted. Runs whether or
    // not the integration was created, so a site never ends its setup
    // carrying the relaxed config.
    apply_plan(pool, config, supervisor, tenant, new.plan).await?;
    Ok(())
}

/// The directory layout a Ghost expects, relative to its working directory.
async fn create_tree(dir: &Path) -> Result<()> {
    for sub in [
        "content/themes",
        "content/images",
        "content/media",
        "content/files",
        "content/data",
        "content/settings",
        "content/adapters",
    ] {
        tokio::fs::create_dir_all(dir.join(sub)).await?;
    }
    Ok(())
}

/// Copy the starting themes in.
///
/// Every tenant gets its own copy rather than a shared directory, because
/// Ghost writes into a theme's directory — activation state, and the
/// `custom_theme_settings` a theme declares. A shared tree would let one
/// tenant's theme change show up on another's site.
async fn install_themes(config: &Arc<Config>, dir: &Path) -> Result<()> {
    // Relative paths are taken from the deployment directory, so a config can
    // point at the Ghost-rust checkout beside it without naming this machine.
    let Some(source) = config.theme_source_path() else {
        // Not fatal. Ghost-rust falls back to its built-in rendering, and a
        // platform may deliberately ship no themes and let customers upload.
        tracing::warn!(
            "paths.themeSource is unset, so new sites start with no theme installed"
        );
        return Ok(());
    };
    if !source.exists() {
        tracing::warn!("paths.themeSource {} does not exist; skipping", source.display());
        return Ok(());
    }
    let dest = dir.join("content/themes");
    // A recursive copy is blocking work.
    tokio::task::spawn_blocking(move || copy_dir(&source, &dest))
        .await
        .map_err(|e| ProError::internal(format!("theme copy failed: {e}")))?
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(from).follow_links(false).into_iter().filter_map(|e| e.ok()) {
        let rel = match entry.path().strip_prefix(from) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let target = to.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
        // Symlinks are skipped: a theme source containing one would otherwise
        // be copied as a link pointing outside the tenant's tree.
    }
    Ok(())
}

/// Complete the new site's own setup, through its own API.
///
/// Deliberately *not* a database write. This is the same endpoint a human
/// would reach in a browser, it refuses once setup is complete, and it means
/// the platform holds no knowledge of how the CMS stores users.
async fn complete_ghost_setup(port: u16, new: &NewSite<'_>) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| ProError::internal(e))?;

    let response = client
        .post(format!("http://127.0.0.1:{port}/ghost/api/admin/authentication/setup"))
        .json(&serde_json::json!({
            "setup": [{
                "name": new.owner_name,
                "email": new.owner_email,
                "password": new.owner_password,
                "blogTitle": new.title,
            }]
        }))
        .send()
        .await
        .map_err(|e| {
            ProError::Unavailable(format!("the new site did not answer its setup call: {e}"))
        })?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    // 403 means setup was already done — which happens on a retry of a job
    // that got further than it reported. That is success, not failure:
    // insisting otherwise would strand a site that is actually fine.
    if status.as_u16() == 403 {
        tracing::info!("site setup was already complete");
        return Ok(());
    }

    let body = response.text().await.unwrap_or_default();
    Err(ProError::Internal(format!("site setup failed ({status}): {body}")))
}

/// Create the platform's integration on a freshly set-up site, and return
/// its admin API key.
///
/// Two calls with the owner's session, made once in the site's life: sign in,
/// then add a custom integration named for the platform. The response carries
/// the integration's keys, and the admin one is what comes back. This is the
/// same mechanism a customer would use to wire up Zapier, which is the point:
/// the platform's access is exactly as powerful and exactly as revocable as
/// any other integration's.
async fn create_platform_integration(
    port: u16,
    new: &NewSite<'_>,
) -> Result<(String, String)> {
    let base = format!("http://127.0.0.1:{port}/ghost/api/admin");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(ProError::internal)?;

    let login = client
        .post(format!("{base}/session/"))
        .json(&serde_json::json!({
            "email": new.owner_email,
            "password": new.owner_password,
        }))
        .send()
        .await
        .map_err(|e| ProError::internal(e))?;
    if !login.status().is_success() {
        return Err(ProError::Internal(format!(
            "could not sign in to the new site to create the platform integration ({})",
            login.status()
        )));
    }
    let cookie = login
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            ProError::Internal("the new site's sign-in returned no session".into())
        })?
        .to_string();

    let created = client
        .post(format!("{base}/integrations/"))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&serde_json::json!({
            "integrations": [{
                "name": "Ghost Pro platform",
                "description": "Managed hosting. Usage figures and health checks. \
Deleting this revokes the platform's access to this site.",
            }]
        }))
        .send()
        .await
        .map_err(ProError::internal)?;
    if !created.status().is_success() {
        let body = created.text().await.unwrap_or_default();
        return Err(ProError::Internal(format!(
            "creating the platform integration failed: {body}"
        )));
    }

    let body: serde_json::Value = created.json().await.map_err(ProError::internal)?;
    let key = body["integrations"][0]["api_keys"]
        .as_array()
        .and_then(|keys| {
            keys.iter()
                .find(|k| k["type"] == "admin")
                .cloned()
        })
        .ok_or_else(|| {
            ProError::Internal("the integration was created without an admin key".into())
        })?;
    let id = key["id"].as_str().ok_or_else(|| {
        ProError::Internal("the integration's admin key has no id".into())
    })?;
    let secret = key["secret"].as_str().ok_or_else(|| {
        ProError::Internal("the integration's admin key has no secret".into())
    })?;
    Ok((id.to_string(), secret.to_string()))
}

/// Rewrite a tenant's `hostSettings` for a new plan, and restart it.
///
/// The restart is not optional: the CMS reads `hostSettings` once at boot and
/// builds its limit table from it, so a plan change that only rewrote the file
/// would appear to work and change nothing until the next restart — which
/// might be weeks away.
pub async fn apply_plan(
    pool: &Db,
    config: &Arc<Config>,
    supervisor: &Supervisor,
    tenant: &Tenant,
    plan: &Plan,
) -> Result<()> {
    let dir = config.tenant_dir(&tenant.id);
    let mut body = config_file::read(&dir, &config.tenants.env).await?;
    body["hostSettings"] = plan.host_settings(&tenant.period_start, &config.billing_url());
    config_file::write(&dir, &config.tenants.env, &body).await?;

    pro_db::tenants::set_plan(pool, &tenant.id, &plan.slug).await?;

    // Only restart what was running. Starting a hibernating tenant just to
    // apply a plan would wake every site on a platform-wide plan change — and
    // it will read the new file whenever it next starts anyway.
    if supervisor.is_running(&tenant.id) {
        supervisor.restart(tenant).await?;
    }
    Ok(())
}

/// Close a site to the public while leaving its admin reachable.
///
/// Implemented as a plan swap rather than as a refusal at the edge, which is
/// the detail that makes it recoverable: the site still answers, so its owner
/// can sign in, see why, and pay. A 402 at the proxy would lock them out of
/// the very screen that fixes it.
pub async fn suspend_site(
    pool: &Db,
    config: &Arc<Config>,
    supervisor: &Supervisor,
    tenant: &Tenant,
    reason: &str,
) -> Result<()> {
    let suspended = pro_core::plans::suspended_plan();
    let dir = config.tenant_dir(&tenant.id);

    // A tenant whose directory has gone is a broken record, not something to
    // fail on — mark it and move on, so the suspension sweep is not blocked
    // by one bad row.
    if dir.exists() {
        let mut body = config_file::read(&dir, &config.tenants.env).await?;
        body["hostSettings"] = suspended.host_settings(&tenant.period_start, &config.billing_url());
        config_file::write(&dir, &config.tenants.env, &body).await?;
    } else {
        tracing::warn!(tenant = %tenant.slug, "suspending a tenant whose directory is missing");
    }

    pro_db::tenants::suspend(pool, &tenant.id, reason).await?;
    if supervisor.is_running(&tenant.id) {
        supervisor.restart(tenant).await?;
    }
    Ok(())
}

/// Put a site back on its paid plan.
pub async fn unsuspend_site(
    pool: &Db,
    config: &Arc<Config>,
    supervisor: &Supervisor,
    tenant: &Tenant,
    plan: &Plan,
) -> Result<()> {
    pro_db::tenants::unsuspend(pool, &tenant.id).await?;
    let fresh = pro_db::tenants::get(pool, &tenant.id).await?;
    apply_plan(pool, config, supervisor, &fresh, plan).await
}

/// Point a tenant's canonical `url` at a different hostname.
///
/// Every absolute link the site emits is built from this string, so it has to
/// be a hostname that already resolves here — which is why
/// `pro_db::domains::set_primary` refuses anything but a live domain.
pub async fn update_primary_domain(
    pool: &Db,
    config: &Arc<Config>,
    supervisor: &Supervisor,
    tenant: &Tenant,
) -> Result<()> {
    let primary = pro_db::domains::primary_for_tenant(pool, &tenant.id)
        .await?
        .ok_or_else(|| ProError::Internal("tenant has no primary domain".into()))?;

    let scheme = if config.public_tls() { "https" } else { "http" };
    let url = match config.edge_port() {
        443 | 80 => format!("{scheme}://{}", primary.domain),
        port => format!("{scheme}://{}:{port}", primary.domain),
    };

    let dir = config.tenant_dir(&tenant.id);
    let mut body = config_file::read(&dir, &config.tenants.env).await?;
    if body["url"] == serde_json::json!(url) {
        return Ok(());
    }
    body["url"] = serde_json::json!(url);
    config_file::write(&dir, &config.tenants.env, &body).await?;

    // This one *must* restart even if hibernating: the site's links are wrong
    // until it does, and a sleeping site will be woken by the next visitor
    // reading those links.
    if supervisor.is_running(&tenant.id) {
        supervisor.restart(tenant).await?;
    }
    tracing::info!(tenant = %tenant.slug, "canonical url is now {url}");
    Ok(())
}

/// Remove a tenant's files, after its grace period.
///
/// Stops the process first, then moves the directory aside and deletes it.
/// The move is so that a deletion interrupted halfway cannot leave a tenant
/// with a half-empty content directory that still looks live — a
/// `.deleting-` prefix is unmistakably not a tenant.
pub async fn delete_site_files(
    config: &Arc<Config>,
    supervisor: &Supervisor,
    tenant: &Tenant,
) -> Result<()> {
    supervisor.stop(&tenant.id).await?;

    let dir = config.tenant_dir(&tenant.id);
    if dir.exists() {
        let condemned = config
            .tenants_dir()
            .join(format!(".deleting-{}-{}", tenant.id, pro_core::ids::object_id()));
        tokio::fs::rename(&dir, &condemned).await?;
        tokio::fs::remove_dir_all(&condemned).await?;
        tracing::info!(tenant = %tenant.slug, "site files removed");
    }

    // On MySQL the site's data is not in that directory, so removing the files
    // is only half the deletion. A database left behind is a former customer's
    // content that nobody is looking after, and it still occupies the server.
    //
    // Logged rather than returned: the files are already gone, so failing here
    // would leave the tenant permanently undeletable because the database
    // server was briefly unreachable. An operator needs to know, and `doctor`
    // is not the place — this is a named site and a named database.
    let store = config.tenant_store()?;
    if let pro_core::TenantStore::MySqlServer { .. } = store {
        if let Err(e) = crate::tenant_db::drop(&store, &tenant.id).await {
            tracing::error!(
                tenant = %tenant.slug,
                database = %store.database_name(&tenant.id).unwrap_or_default(),
                "site files were removed but its database was not: {e}. \
                 It must be dropped by hand."
            );
        }
    }
    Ok(())
}

/// Bring a site back from a snapshot archive.
///
/// Refuses if the tenant's directory already exists, because the only safe
/// restore is into an empty place: merging an archive over a live site would
/// produce a database and a content directory from different moments.
pub async fn restore_site(
    config: &Arc<Config>,
    supervisor: &Supervisor,
    tenant: &Tenant,
    archive: &Path,
) -> Result<()> {
    let dir = config.tenant_dir(&tenant.id);
    if dir.exists() {
        return Err(ProError::Conflict(format!(
            "{} still has a site directory. Delete it first, or restore to a new site.",
            tenant.slug
        )));
    }
    supervisor.stop(&tenant.id).await?;
    crate::backup::unpack(archive, &dir).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Arc<Config> {
        let base = std::env::temp_dir().join(format!("pro-prov-{}", pro_core::ids::object_id()));
        let mut c: Config = serde_json::from_value(serde_json::json!({
            "url": "http://pro.localhost:8080",
            "subdomainBase": "localhost",
            "paths": { "ghostBinary": "/bin/sh", "dataDir": base.to_str().unwrap() },
        }))
        .unwrap();
        c.plans = pro_core::plans::default_plans();
        // Away from the default 21000– range, which a real deployment on this
        // machine is using. These tests allocate from an empty in-memory
        // database, so they always pick the bottom of the range, and a live
        // tenant listening there made a start that should have timed out look
        // like a success — `a_failed_provision_is_recorded_as_failed` passed or
        // failed depending on whether a site happened to be awake.
        let floor = 40_000 + (rand_offset() % 20_000);
        c.tenants.port_floor = floor;
        c.tenants.port_ceiling = floor + 50;
        Arc::new(c)
    }

    /// A spread of ports per test process, so two concurrent runs of this
    /// suite do not probe each other's.
    fn rand_offset() -> u16 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut hasher);
        pro_core::ids::object_id().hash(&mut hasher);
        (hasher.finish() % u16::MAX as u64) as u16
    }

    /// Ghost's own password floor, checked before any directory exists so the
    /// failure is cheap and the message is about the password.
    #[tokio::test]
    async fn refuses_a_short_site_password() {
        let pool = pro_db::open_in_memory().await.unwrap();
        let cfg = config();
        let sup = pro_supervisor::Supervisor::new(cfg.clone()).unwrap();
        let account = pro_db::accounts::create(&pool, "o@example.com", "O", "h").await.unwrap();
        let plans = pro_core::plans::default_plans();

        let err = create_site(
            &pool,
            &cfg,
            &sup,
            &pro_core::config::MailConfig::default(),
            NewSite {
                account_id: &account.id,
                slug: "shortpw",
                title: "Short",
                plan: &plans[0],
                owner_name: "O",
                owner_email: "o@example.com",
                owner_password: "short",
                period_start: "2026-01-01T00:00:00Z",
            },
        )
        .await
        .unwrap_err();

        assert_eq!(err.status(), 400);
        assert!(err.to_string().contains("10 characters"), "got {err}");
        // Nothing was claimed.
        assert!(pro_db::tenants::find_by_slug(&pool, "shortpw").await.unwrap().is_none());
    }

    /// A reserved slug is refused before anything is created.
    #[tokio::test]
    async fn refuses_a_reserved_slug() {
        let pool = pro_db::open_in_memory().await.unwrap();
        let cfg = config();
        let sup = pro_supervisor::Supervisor::new(cfg.clone()).unwrap();
        let account = pro_db::accounts::create(&pool, "o@example.com", "O", "h").await.unwrap();
        let plans = pro_core::plans::default_plans();

        let err = create_site(
            &pool,
            &cfg,
            &sup,
            &pro_core::config::MailConfig::default(),
            NewSite {
                account_id: &account.id,
                slug: "admin",
                title: "Nope",
                plan: &plans[0],
                owner_name: "O",
                owner_email: "o@example.com",
                owner_password: "a-long-enough-password",
                period_start: "2026-01-01T00:00:00Z",
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), 409);
    }

    /// A failed provision leaves a record an operator can see, not a silently
    /// missing site — and does not hold a port.
    #[tokio::test]
    async fn a_failed_provision_is_recorded_as_failed() {
        let pool = pro_db::open_in_memory().await.unwrap();
        let mut c = (*config()).clone();
        // /bin/sh exits immediately and never listens, so the start times out.
        c.tenants.start_timeout_secs = 1;
        let cfg = Arc::new(c);
        let sup = pro_supervisor::Supervisor::new(cfg.clone()).unwrap();
        let account = pro_db::accounts::create(&pool, "o@example.com", "O", "h").await.unwrap();
        let plans = pro_core::plans::default_plans();

        let err = create_site(
            &pool,
            &cfg,
            &sup,
            &pro_core::config::MailConfig::default(),
            NewSite {
                account_id: &account.id,
                slug: "willfail",
                title: "Will Fail",
                plan: &plans[0],
                owner_name: "O",
                owner_email: "o@example.com",
                owner_password: "a-long-enough-password",
                period_start: "2026-01-01T00:00:00Z",
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), 503, "a start timeout is retryable");

        let tenant = pro_db::tenants::find_by_slug(&pool, "willfail").await.unwrap().unwrap();
        assert_eq!(tenant.status, TenantStatus::Failed);
        assert!(!sup.is_running(&tenant.id));

        // The directory and config were built before the start was attempted,
        // which is what lets a retry pick up where this left off.
        let dir = cfg.tenant_dir(&tenant.id);
        assert!(dir.join("config.production.json").exists());
        assert!(dir.join("content/data").exists());

        let _ = tokio::fs::remove_dir_all(cfg.data_dir()).await;
    }

    #[tokio::test]
    async fn builds_the_directory_tree() {
        let dir = std::env::temp_dir().join(format!("pro-tree-{}", pro_core::ids::object_id()));
        create_tree(&dir).await.unwrap();
        for sub in ["content/themes", "content/images", "content/data", "content/media"] {
            assert!(dir.join(sub).is_dir(), "{sub} missing");
        }
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    /// Each tenant gets its own copy of the themes, because Ghost writes into
    /// a theme's directory.
    #[tokio::test]
    async fn copies_themes_per_tenant() {
        let base = std::env::temp_dir().join(format!("pro-themes-{}", pro_core::ids::object_id()));
        let source = base.join("source");
        tokio::fs::create_dir_all(source.join("casper/partials")).await.unwrap();
        tokio::fs::write(source.join("casper/index.hbs"), "{{title}}").await.unwrap();
        tokio::fs::write(source.join("casper/partials/x.hbs"), "x").await.unwrap();

        let mut c = (*config()).clone();
        c.paths.theme_source = Some(source.to_str().unwrap().to_string());
        let cfg = Arc::new(c);

        let tenant_dir = base.join("tenant");
        create_tree(&tenant_dir).await.unwrap();
        install_themes(&cfg, &tenant_dir).await.unwrap();

        assert!(tenant_dir.join("content/themes/casper/index.hbs").exists());
        assert!(tenant_dir.join("content/themes/casper/partials/x.hbs").exists());

        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    /// A missing theme source is a warning, not a failure — a platform may
    /// ship none and let customers upload.
    #[tokio::test]
    async fn a_missing_theme_source_is_not_fatal() {
        let mut c = (*config()).clone();
        c.paths.theme_source = Some("/nonexistent/themes".into());
        let cfg = Arc::new(c);
        let dir = std::env::temp_dir().join(format!("pro-nothemes-{}", pro_core::ids::object_id()));
        create_tree(&dir).await.unwrap();
        assert!(install_themes(&cfg, &dir).await.is_ok());
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    /// Restoring over a live site would mix a database and content from
    /// different moments.
    #[tokio::test]
    async fn refuses_to_restore_over_an_existing_site() {
        let pool = pro_db::open_in_memory().await.unwrap();
        let cfg = config();
        let sup = pro_supervisor::Supervisor::new(cfg.clone()).unwrap();
        let account = pro_db::accounts::create(&pool, "o@example.com", "O", "h").await.unwrap();
        let tenant = pro_db::tenants::create(
            &pool, &account.id, "live", "Live", "starter", "2026-01-01T00:00:00Z", 21000, 21010,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(cfg.tenant_dir(&tenant.id)).await.unwrap();

        let err = restore_site(&cfg, &sup, &tenant, Path::new("/tmp/whatever.zip"))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 409);

        let _ = tokio::fs::remove_dir_all(cfg.data_dir()).await;
    }

    /// Deleting a site whose files are already gone must not error — the
    /// sweeper retries, and a second pass should be a no-op.
    #[tokio::test]
    async fn deleting_missing_files_is_harmless() {
        let pool = pro_db::open_in_memory().await.unwrap();
        let cfg = config();
        let sup = pro_supervisor::Supervisor::new(cfg.clone()).unwrap();
        let account = pro_db::accounts::create(&pool, "o@example.com", "O", "h").await.unwrap();
        let tenant = pro_db::tenants::create(
            &pool, &account.id, "gone", "Gone", "starter", "2026-01-01T00:00:00Z", 21000, 21010,
        )
        .await
        .unwrap();
        assert!(delete_site_files(&cfg, &sup, &tenant).await.is_ok());
    }
}
