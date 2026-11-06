//! `pro-web` — the two panels, the public pages, and the edge's request
//! handler.
//!
//! ## Two panels, one process
//!
//! | Surface | Who | Prefix | Cookie |
//! |---|---|---|---|
//! | [`dashboard`] | subscribers | `/dashboard` | `pro_session` |
//! | [`console`] | the operator | `/console` | `pro_staff` |
//! | [`public`] | anybody | `/`, `/signup`, `/pricing` | — |
//!
//! The split is not cosmetic. They are different populations in different
//! tables with different sessions, and the only way to reach the console is a
//! staff login — see [`auth`] and `pro_db::sessions`.
//!
//! ## Everything else is a tenant
//!
//! Any request whose `Host` is not the platform's own is a tenant's, and falls
//! through to [`tenant_request`]: resolve the hostname, wake the site if it is
//! asleep, and stream the request to it.
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use pro_core::{ProError, Result};

pub mod auth;
pub mod client_ip;
pub mod console;
pub mod dashboard;
pub mod dashboard_billing;
pub mod public;
pub mod reply;
pub mod state;
pub mod view;
pub mod webhooks;
pub mod worker;

pub use state::AppState;

/// The platform's own routes.
///
/// Mounted behind the host check in [`entry`], so these are only reachable on
/// the platform's hostname — a tenant's domain cannot be used to reach the
/// console.
pub fn platform_router(state: AppState) -> Router {
    platform_routes().with_state(state)
}

/// The routes themselves, before state is applied.
///
/// Separate from [`platform_router`] because [`entry`] needs to attach the
/// host-gate middleware *and* the fallback before the state goes on — axum
/// takes a `Router<S>` for those and a `Router<()>` afterwards.
fn platform_routes() -> Router<AppState> {
    Router::new()
        // ---- public ------------------------------------------------------
        .route("/", get(public::home))
        // The landing page's two static files. Named paths rather than a
        // directory service: there are exactly two, they are compiled in, and
        // a served directory is a wider door than this needs.
        .route("/assets/landing.css", get(public::landing_css))
        .route("/assets/landing.js", get(public::landing_js))
        .route("/pricing", get(public::pricing))
        .route("/signup", get(public::signup_form).post(public::signup))
        .route("/verify", get(public::verify))
        .route("/forgot", get(public::forgot_form).post(public::forgot))
        .route("/reset", get(public::reset_form).post(public::reset))
        .route("/health", get(public::health))
        // The appearance switcher, which every page's rail posts to. Not
        // inside either panel's prefix because both panels and the public
        // pages share it, and it needs no session: the choice is a cookie.
        .route("/appearance", post(public::appearance))
        // ---- customer dashboard ------------------------------------------
        .route("/dashboard", get(dashboard::sites))
        .route("/dashboard/signin", get(public::signin_form).post(public::signin))
        .route("/dashboard/signout", get(public::signout).post(public::signout))
        .route("/dashboard/resend-verification", post(public::resend_verification))
        .route("/dashboard/new", get(dashboard::new_site_form).post(dashboard::create_site))
        .route("/dashboard/sites/{id}", get(dashboard::site))
        .route("/dashboard/sites/{id}/restart", post(dashboard::restart_site))
        // Starting (or restarting) the payment a closed site is waiting for.
        // Not the billing screen's plan change, which refuses a plan the site
        // is already on.
        .route("/dashboard/sites/{id}/pay", post(dashboard::pay_site))
        .route("/dashboard/sites/{id}/backup", post(dashboard::backup_site))
        .route(
            "/dashboard/sites/{id}/backups/{backup_id}",
            get(dashboard::download_backup),
        )
        .route("/dashboard/sites/{id}/delete", post(dashboard::delete_site))
        .route("/dashboard/sites/{id}/restore", post(dashboard::restore_site))
        .route("/dashboard/sites/{id}/domains", post(dashboard::add_domain))
        .route(
            "/dashboard/sites/{id}/domains/{domain_id}/check",
            post(dashboard::check_domain),
        )
        .route(
            "/dashboard/sites/{id}/domains/{domain_id}/primary",
            post(dashboard::set_primary_domain),
        )
        .route(
            "/dashboard/sites/{id}/domains/{domain_id}/remove",
            post(dashboard::remove_domain),
        )
        .route("/dashboard/billing", get(dashboard_billing::billing))
        .route("/dashboard/billing/change", post(dashboard_billing::change_plan))
        .route(
            "/dashboard/billing/manage/{id}",
            post(dashboard_billing::manage_subscription),
        )
        .route(
            "/dashboard/billing/cancel/{id}",
            post(dashboard_billing::cancel_subscription),
        )
        .route(
            "/dashboard/account",
            get(dashboard_billing::account_page).post(dashboard_billing::update_account),
        )
        .route("/dashboard/account/password", post(dashboard_billing::change_password))
        .route("/dashboard/account/email", post(dashboard_billing::change_email))
        .route(
            "/dashboard/account/sessions/{id}/revoke",
            post(dashboard_billing::revoke_session),
        )
        // ---- operator console --------------------------------------------
        .route("/console", get(console::overview))
        .route("/console/signin", get(console::signin_form).post(console::signin))
        .route("/console/signout", get(console::signout).post(console::signout))
        .route("/console/sites", get(console::sites))
        .route("/console/sites/{id}", get(console::site))
        .route("/console/sites/{id}/plan", post(console::set_plan))
        .route("/console/sites/{id}/suspend", post(console::suspend))
        .route("/console/sites/{id}/unsuspend", post(console::unsuspend))
        .route("/console/sites/{id}/restart", post(console::restart))
        .route("/console/sites/{id}/stop", post(console::stop))
        .route("/console/sites/{id}/backup", post(console::backup))
        .route(
            "/console/sites/{id}/backups/{backup_id}",
            get(console::download_backup),
        )
        .route(
            "/console/sites/{id}/backups/{backup_id}/restore",
            post(console::restore_backup),
        )
        .route(
            "/console/webhooks/{provider}/{event_id}/replay",
            post(console::replay_webhook),
        )
        .route(
            "/console/subscriptions/{id}/attach",
            post(console::attach_subscription),
        )
        .route("/console/customers", get(console::customers))
        .route("/console/customers/{id}", get(console::customer))
        .route("/console/customers/{id}/toggle", post(console::toggle_customer))
        .route("/console/plans", get(console::plans_page).post(console::save_plans))
        .route("/console/settings", get(console::settings).post(console::save_settings))
        .route("/console/staff", get(console::staff_page).post(console::create_staff))
        .route("/console/staff/{id}/role", post(console::set_staff_role))
        .route("/console/staff/{id}/toggle", post(console::toggle_staff))
        .route("/console/activity", get(console::activity))
        .route("/console/jobs/{id}/retry", post(console::retry_job))
        // ---- billing webhooks --------------------------------------------
        //
        // Outside both panels and unauthenticated by design: the authentication
        // *is* the signature, which is why `pro_billing::signature` fails
        // closed when a secret is missing.
        .route("/webhooks/{provider}", post(webhooks::receive))
}

/// The edge's entry point.
///
/// Every request arrives here, and the `Host` header decides what it is: the
/// platform's own hostname is routed by [`platform_router`], and everything
/// else is a tenant's and is proxied.
///
/// One listener rather than two because there is one certificate store, one
/// port, and one place where hibernation has to be woken — splitting it would
/// mean duplicating all three.
///
/// The host decision is a **middleware**, not a fallback, and that distinction
/// is the security-relevant part. A fallback is only reached when no route
/// matches, so `https://a-customers-domain.com/console` would have matched the
/// console route and been served. It would still have demanded a staff login,
/// but the console must not be *addressable* from a hostname a customer
/// controls: that is enough to host a convincing copy of its sign-in page on a
/// domain their own visitors already trust. Deciding before routing means
/// those paths do not exist on any host but the platform's.
pub fn entry(state: AppState) -> Router {
    platform_routes()
        // The platform's own 404, for a path it genuinely does not serve.
        .fallback(platform_not_found)
        .layer(axum::middleware::from_fn_with_state(state.clone(), host_gate))
        .with_state(state)
}

/// Decide, before routing, whether this request belongs to the platform.
async fn host_gate(
    State(state): State<AppState>,
    client: client_ip::ClientAddr,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // ACME's HTTP-01 challenge must be answerable on *any* hostname, since it
    // is how a custom domain's certificate is obtained in the first place.
    // Nothing else is shared between the two worlds.
    if request.uri().path().starts_with("/.well-known/acme-challenge/") {
        return next.run(request).await;
    }

    if pro_core::normalize_host(&host) == state.hosts.platform_host() {
        return next.run(request).await;
    }

    // Read before the request is consumed by the proxy, so the error page for
    // a hostname we do not serve still honours the visitor's colour scheme.
    let chrome = state.chrome(request.headers());

    match serve_tenant(&state, &host, client, request).await {
        Ok(response) => response,
        Err(e) => {
            if e.status() >= 500 {
                tracing::error!("serving {host}: {e}");
            }
            unknown_host_page(&state, &chrome, &host, &e)
        }
    }
}

/// A path the platform does not serve, on the platform's own host.
async fn platform_not_found(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    reply::error_page(
        &state.chrome(&headers),
        &ProError::NotFound("No such page.".into()),
    )
}

async fn serve_tenant(
    state: &AppState,
    host: &str,
    client: client_ip::ClientAddr,
    request: Request,
) -> Result<Response> {
    let route = state.hosts.resolve(&state.db, host).await;

    let tenant_id = match route {
        pro_proxy::Route::Tenant { tenant_id } => tenant_id,
        // The platform's own host reaching the fallback means a path the
        // platform does not serve.
        pro_proxy::Route::Platform => {
            return Err(ProError::NotFound("No such page.".into()))
        }
        pro_proxy::Route::Unknown => {
            return Err(ProError::NotFound("No site is configured here.".into()))
        }
    };

    let target = pro_proxy::target(&state.db, &tenant_id)
        .await
        .ok_or_else(|| ProError::NotFound("No site is configured here.".into()))?;

    // Hibernation, made invisible: start the site and wait rather than
    // failing. The visitor sees a slow page, which is the whole bargain that
    // lets one machine hold far more sites than it can run at once.
    let live = state.live();
    if !state.supervisor.is_running(&target.tenant.id) {
        // At the ceiling, something has to be stopped first.
        let all = pro_db::tenants::list(&state.db, None, None, 5_000, 0).await?;
        state.supervisor.evict_to_fit(&all, live.max_running()).await;

        let started = std::time::Instant::now();
        state.supervisor.ensure_running(&target.tenant).await?;
        tracing::info!(
            tenant = %target.tenant.slug,
            "woke a sleeping site in {} ms",
            started.elapsed().as_millis()
        );
    }

    // Stamped for the hibernation sweeper. Deliberately not awaited on the
    // critical path — a write per request would serialise traffic on SQLite,
    // and being a second stale costs nothing here.
    let pool = state.db.clone();
    let id = target.tenant.id.clone();
    tokio::spawn(async move {
        let _ = pro_db::tenants::touch(&pool, &id).await;
    });

    let behind_proxy = state.config.ssl.behind_proxy;
    let client_ip = pro_proxy::client_ip(request.headers(), client.0, behind_proxy);
    let scheme = pro_proxy::client_scheme(
        request.headers(),
        state.config.ssl.enabled,
        behind_proxy,
    );

    state
        .forwarder
        .send(request, target.tenant.port, client_ip, host, scheme)
        .await
}

/// The page shown for a hostname this platform does not serve.
///
/// Three different situations, and a customer debugging their DNS deserves to
/// be told which:
///
/// * a mistyped tenant subdomain — no such site;
/// * a domain pointed here that nobody has attached — the commonest real case,
///   and the one where the message has to say what to do;
/// * a site that is not currently routable.
fn unknown_host_page(
    state: &AppState,
    chrome: &crate::state::Chrome,
    host: &str,
    error: &ProError,
) -> Response {
    if error.status() >= 500 {
        return reply::error_page(chrome, error);
    }

    let platform = &chrome.platform;
    let in_zone = state.hosts.is_tenant_zone(host);
    let (heading, message) = if in_zone {
        (
            "No site here",
            "This address is not in use. If you have just created a site, give it a \
             few seconds and reload."
                .to_string(),
        )
    } else {
        (
            "Domain not connected",
            format!(
                "{} points at {platform}, but no site here is using it yet. If this is \
                 your domain, add it to your site in your dashboard.",
                if host.is_empty() { "This address" } else { host }
            ),
        )
    };

    let card = format!(
        r#"<a class="btn tonal block" href="{}">Go to {}</a>"#,
        view::attr(&state.config.canonical_url()),
        view::esc(platform),
    );

    reply::html_status(
        axum::http::StatusCode::NOT_FOUND,
        // `auth_page` escapes `heading` and `sub` itself, so the hostname
        // inside the message is handed over raw and escaped there — escaping
        // it twice is what would print `&amp;` to a customer debugging DNS.
        view::auth_page(
            heading,
            platform,
            chrome.theme,
            heading,
            &message,
            None,
            &card,
            "",
        ),
    )
}

// ---------------------------------------------------------------------------
// Shared helpers used by the dashboard and the worker
// ---------------------------------------------------------------------------

/// What the platform expects a customer's DNS to say.
///
/// The addresses come from `ssl.domains`-style configuration if an operator
/// has set them, and otherwise from resolving the platform's own hostname —
/// which is right far more often than it looks, because the platform's
/// hostname is already pointed at the machine that serves tenants.
pub fn domain_expectation(state: &AppState) -> pro_proxy::dns::Expectation {
    let configured: Vec<std::net::IpAddr> = state
        .config
        .public_addresses
        .iter()
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    pro_proxy::dns::Expectation {
        addresses: configured,
        // A CNAME to the tenant zone is the recommended setup: it keeps
        // working when the platform's address changes.
        cname_target: state.config.subdomain_base.trim_matches('.').to_string(),
    }
}


/// Open a tenant's archive for download, checked for that tenant.
///
/// Shared by both panels: the customer's right to a copy and the operator's
/// are the same file behind the same checks. The path in the row was written
/// by this platform, but it is re-anchored under the backups directory
/// anyway — a tampered database must not turn the platform into a file
/// server, and `canonicalize` resolving the file doubles as its existence
/// check.
pub async fn open_backup(
    state: &AppState,
    tenant_id: &str,
    backup_id: &str,
) -> Result<(pro_db::backups::Backup, tokio::fs::File)> {
    let backup = pro_db::backups::get(&state.db, backup_id).await?;
    if backup.tenant_id != tenant_id {
        // 404, not 403: an id that belongs to somebody else is
        // indistinguishable from one that was never issued.
        return Err(ProError::NotFound("No such backup.".into()));
    }

    let path = std::path::PathBuf::from(&backup.path);
    let base = state.config.backup_dir();
    let (canonical, base) = match (path.canonicalize(), base.canonicalize()) {
        (Ok(c), Ok(b)) => (c, b),
        _ => return Err(ProError::NotFound("That backup is no longer available.".into())),
    };
    if !canonical.starts_with(&base) {
        return Err(ProError::NotFound("That backup is no longer available.".into()));
    }

    let file = tokio::fs::File::open(&canonical)
        .await
        .map_err(|_| ProError::NotFound("That backup is no longer available.".into()))?;
    Ok((backup, file))
}

/// Check one domain now and apply the result.
///
/// Shared by the dashboard's "Check now" button and the background sweeper, so
/// both reach the same conclusion and write the same state.
pub async fn verify_domain_now(
    state: &AppState,
    domain: &pro_core::Domain,
) -> Result<pro_proxy::dns::Verdict> {
    let expect = domain_expectation(state);
    let verdict =
        pro_proxy::dns::check(&domain.domain, &domain.verification_token, &expect).await?;

    use pro_core::DomainStatus;
    use pro_proxy::dns::Verdict;

    let (status, error) = match &verdict {
        Verdict::PointsHere => (DomainStatus::Active, None),
        Verdict::OwnershipProven => (DomainStatus::Verified, Some(verdict.message())),
        Verdict::PointsElsewhere { .. } => (DomainStatus::Failed, Some(verdict.message())),
        Verdict::NotFound => (DomainStatus::Pending, Some(verdict.message())),
    };

    pro_db::domains::set_status(&state.db, &domain.id, status, error.as_deref()).await?;

    if status == DomainStatus::Active && domain.status != DomainStatus::Active {
        // Now routable, so the edge's cache has to let go of its "unknown".
        state.hosts.invalidate(&domain.domain);
        state.hosts.invalidate_all();

        pro_db::audit::record(
            &state.db,
            "system",
            "domain.verified",
            Some(&domain.tenant_id),
            None,
            serde_json::json!({"domain": domain.domain}),
        )
        .await;

        // Tell the customer, since this is the step they were waiting on and
        // DNS propagation means they have no idea when it finished.
        if let Ok(tenant) = pro_db::tenants::get(&state.db, &domain.tenant_id).await {
            if let Ok(account) = pro_db::accounts::get(&state.db, &tenant.account_id).await {
                let live = state.live();
                let _ = state
                    .mailer()
                    .send(pro_mail::templates::domain_live(
                        &live.platform_name(),
                        &account.email,
                        &account.name,
                        &domain.domain,
                    ))
                    .await;
            }
        }
    }

    Ok(verdict)
}
