//! The operator's panel: `/console`.
//!
//! This is the platform owner's surface, not a customer's. It has its own
//! login, its own cookie and its own session population — see
//! `pro_db::sessions` for why those are three separate mechanisms rather than
//! one.
//!
//! The rail carries a **Console** tag under the platform's name, and that is
//! not decoration: an operator with both panels open needs to know which one
//! they are about to click Suspend in.
//!
//! The settings screen is the reason this panel exists in the shape it does:
//! the payment provider and its credentials are changed here, at runtime, by
//! the owner, and by nobody else. Every field is rendered from
//! `pro_core::settings::REGISTRY`, which also declares the minimum role for
//! each — so a field an administrator may not change is visibly locked, and
//! the save path refuses it regardless of what the form carried.
use axum::extract::{Form, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use pro_core::settings::Kind;
use pro_core::{ProError, Result, Staff, StaffRole, TenantStatus};
use pro_db::sessions::{self, Subject};
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth;
use crate::reply::{self, redirect_with, redirect_with_cookie};
use crate::state::AppState;
use crate::view::{self, attr, esc, BannerKind, Flash, NavItem, Page, PillKind, Theme, Who};

/// Which rail entry is lit.
#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Overview,
    Sites,
    Customers,
    Settings,
    Staff,
    Activity,
}

fn nav(active: Tab) -> Vec<NavItem> {
    vec![
        NavItem::new("Overview", "/console", "chart", "blue", active == Tab::Overview),
        NavItem::new("Sites", "/console/sites", "sites", "green", active == Tab::Sites),
        NavItem::new("Customers", "/console/customers", "people", "purple", active == Tab::Customers),
        NavItem::new("Settings", "/console/settings", "settings", "pink", active == Tab::Settings),
        NavItem::new("Staff", "/console/staff", "shield", "amber", active == Tab::Staff),
        NavItem::new("Activity", "/console/activity", "history", "slate", active == Tab::Activity),
    ]
}

/// The console shell.
fn shell(
    state: &AppState,
    theme: Theme,
    title: &str,
    tab: Tab,
    member: &Staff,
    flash: Option<&Flash>,
    body: &str,
) -> Response {
    let platform = state.platform_name();
    let nav = nav(tab);
    let who = Who {
        name: member.name.clone(),
        detail: member.role.label().to_string(),
        href: None,
        signout: "/console/signout".into(),
    };
    reply::html(view::page(Page {
        title,
        platform: &platform,
        tag: Some("Console"),
        brand_href: "/console",
        nav: &nav,
        who: Some(&who),
        flash,
        theme,
        body,
    }))
}

// ---------------------------------------------------------------------------
// Sign in
// ---------------------------------------------------------------------------

pub async fn signin_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let card = view::form(
        "/console/signin",
        &format!(
            r#"{email}{password}<button class="btn" type="submit">{lock} Sign in</button>"#,
            email = view::field("email", "Email", "email", "", None, true),
            password = view::field("password", "Password", "password", "", None, true),
            lock = view::svg("lock"),
        ),
    );
    reply::html(view::auth_page(
        "Operator sign in",
        &format!("{} Console", state.platform_name()),
        auth::theme(&headers),
        "Operator sign in",
        "This is the platform console, not a customer account.",
        Flash::from_query(&query).as_ref(),
        &card,
        r#"<p class="auth-foot">Looking for your own sites?
<a href="/dashboard/signin">Customer sign in</a></p>"#,
    ))
}

#[derive(Deserialize)]
pub struct SigninForm {
    pub email: String,
    pub password: String,
}

pub async fn signin(
    State(state): State<AppState>,
    headers: HeaderMap,
    client: crate::client_ip::ClientAddr,
    Form(form): Form<SigninForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/console/signin", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = pro_db::staff::authenticate(&state.db, &form.email, &form.password).await?;
        let ip = client.ip_string();
        let user_agent = headers.get("user-agent").and_then(|v| v.to_str().ok());
        let token =
            sessions::create(&state.db, Subject::Staff, &member.id, ip.as_deref(), user_agent)
                .await?;

        pro_db::audit::record(
            &state.db,
            &format!("staff:{}", member.id),
            "staff.signed_in",
            None,
            None,
            serde_json::json!({"ip": ip}),
        )
        .await;

        let cookie = auth::set_cookie(
            sessions::staff_cookie_name(state.secure_cookies()),
            &token,
            state.secure_cookies(),
            sessions::STAFF_TTL_DAYS,
        );
        Ok(redirect_with_cookie("/console", &cookie))
    }.await)
}

pub async fn signout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let name = sessions::staff_cookie_name(state.secure_cookies());
    if let Some(token) = auth::cookie(&headers, name) {
        let _ = sessions::destroy(&state.db, &token).await;
    }
    redirect_with_cookie("/console/signin", &auth::clear_cookie(name, state.secure_cookies()))
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

pub async fn overview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin",
        &chrome,
        overview_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn overview_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let member = auth::staff(state, headers).await?;
    let live = state.live();

    let by_status = pro_db::tenants::count_by_status(&state.db).await?;
    let count = |name: &str| {
        by_status.iter().find(|(s, _)| s == name).map(|(_, n)| *n).unwrap_or(0)
    };
    let accounts = pro_db::accounts::count(&state.db).await?;
    let (members, posts, storage) = pro_db::usage::platform_totals(&state.db).await?;
    let running = state.supervisor.running_count();
    let subs = pro_db::subscriptions::count_by_status(&state.db).await?;
    let sub_count =
        |name: &str| subs.iter().find(|(s, _)| s == name).map(|(_, n)| *n).unwrap_or(0);

    // Monthly recurring revenue from entitling subscriptions, which is the one
    // number an operator checks first.
    let by_plan = pro_db::subscriptions::active_by_plan(&state.db).await?;
    let mrr_cents: i64 = by_plan
        .iter()
        .filter_map(|(slug, n)| {
            live.plan(slug).map(|p| {
                let monthly = if p.interval == "year" { p.price_cents / 12 } else { p.price_cents };
                monthly * n
            })
        })
        .sum();

    let jobs = pro_db::jobs::count_by_status(&state.db).await?;
    let failed_jobs =
        jobs.iter().find(|(s, _)| s == "failed").map(|(_, n)| *n).unwrap_or(0);
    let failed_webhooks = pro_db::audit::failed_webhooks(&state.db, 1).await?.len();

    let mut body = view::page_head(
        "Overview",
        &format!(
            "Signed in as {} ({})",
            esc(&member.name),
            esc(member.role.label())
        ),
        "",
    );

    // Problems first, if any: an operator opening the console wants to know
    // what needs attention before they want statistics.
    let mut warnings = Vec::new();
    if count("failed") > 0 {
        warnings.push(format!(
            r#"{} site{} failed to finish setting up. <a href="/console/sites?status=failed">Review</a>"#,
            count("failed"),
            if count("failed") == 1 { "" } else { "s" }
        ));
    }
    if failed_jobs > 0 {
        warnings.push(format!(
            r#"{failed_jobs} background job{} failed. <a href="/console/activity">Review</a>"#,
            if failed_jobs == 1 { "" } else { "s" }
        ));
    }
    if failed_webhooks > 0 {
        warnings.push(
            r#"Some billing webhooks could not be processed. <a href="/console/activity">Review</a>"#
                .to_string(),
        );
    }
    if live.billing_provider() == "none" {
        warnings.push(
            r#"No payment provider is configured, so nothing is being charged.
<a href="/console/settings/billing">Set one up</a>"#
                .to_string(),
        );
    } else if !live.provider_is_configured(&live.billing_provider()) {
        warnings.push(format!(
            r#"The selected provider (<strong>{}</strong>) is missing credentials — new checkouts
will fail. <a href="/console/settings/billing">Fix</a>"#,
            esc(&live.billing_provider())
        ));
    }
    // A checkout that completes and never reopens the site. Worth its own
    // warning because it is invisible from the checkout path — the operator
    // sees a working button, and only a paying customer finds out otherwise.
    if live.billing_provider() != "none" {
        if let Some(reason) = state.config.webhook_unreachable_reason() {
            warnings.push(format!(
                r#"{reason}. Serve the platform on a public address, or open paid sites by hand
from <a href="/console/sites">Sites</a>."#,
                reason = esc(&reason),
            ));
        } else if live.webhook_secret(&live.billing_provider()).is_none() {
            warnings.push(format!(
                r#"No webhook signing secret for <strong>{}</strong>, so every payment
notification is refused and paid sites stay closed.
<a href="/console/settings/billing">Set one</a>"#,
                esc(&live.billing_provider())
            ));
        }
    }
    // The mailer's own verdict, not a second guess at it: this warning used
    // to check the transport alone, so a platform set to SMTP with no host or
    // no from address showed an all-clear console while every message went to
    // the log.
    if let Some(reason) = pro_mail::Mailer::new(live.mail(), live.platform_name()).not_live_reason()
    {
        warnings.push(format!(
            r#"{reason} — so customers receive no email.
<a href="/console/settings/email">Configure email</a>"#,
            reason = esc(&reason),
        ));
    }
    for warning in &warnings {
        body.push_str(&view::banner(BannerKind::Warn, warning));
    }

    body.push_str(r#"<div class="tiles">"#);
    body.push_str(&view::metric("Active sites", &view::number(count("active")), None));
    body.push_str(&view::metric(
        "Running now",
        &view::number(running as i64),
        Some(&format!("of {} total", view::number(count("active") + count("suspended")))),
    ));
    body.push_str(&view::metric("Customers", &view::number(accounts), None));
    body.push_str(&view::metric(
        "MRR",
        &format!("${}", view::number(mrr_cents / 100)),
        Some("from active subscriptions"),
    ));
    body.push_str("</div>");

    body.push_str(&view::section("Across all sites"));
    body.push_str(r#"<div class="tiles">"#);
    body.push_str(&view::metric("Members", &view::number(members), None));
    body.push_str(&view::metric("Posts", &view::number(posts), None));
    body.push_str(&view::metric(
        "Storage",
        &pro_core::plans::bytes_display(storage),
        None,
    ));
    body.push_str(&view::metric(
        "Suspended",
        &view::number(count("suspended")),
        Some("closed for non-payment"),
    ));
    body.push_str("</div>");

    body.push_str(&view::section("Billing"));
    body.push_str(&view::rows(&view::row(
        "billing",
        "green",
        &match live.billing_provider().as_str() {
            "none" => "No payment provider".to_string(),
            "stripe" => "Stripe".to_string(),
            "paddle" => "Paddle".to_string(),
            "lemonsqueezy" => "Lemon Squeezy".to_string(),
            other => other.to_string(),
        },
        &format!(
            "Trialing {trialing} · Active {active} · Past due {past_due} · Cancelled {cancelled}",
            trialing = sub_count("trialing"),
            active = sub_count("active"),
            past_due = sub_count("past_due"),
            cancelled = sub_count("canceled"),
        ),
        r#"<a class="btn outlined small" href="/console/settings">Settings</a>"#,
    )));

    Ok(shell(
        state,
        theme,
        "Overview",
        Tab::Overview,
        &member,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

// ---------------------------------------------------------------------------
// Sites
// ---------------------------------------------------------------------------

pub async fn sites(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin",
        &chrome,
        sites_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn sites_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let member = auth::staff(state, headers).await?;

    let status_filter = query.get("status").and_then(|s| TenantStatus::parse(s));
    let search = query.get("q").map(String::as_str).filter(|s| !s.trim().is_empty());
    let tenants = pro_db::tenants::list(&state.db, status_filter, search, 200, 0).await?;
    let by_status = pro_db::tenants::count_by_status(&state.db).await?;
    let count = |name: &str| {
        by_status.iter().find(|(s, _)| s == name).map(|(_, n)| *n).unwrap_or(0)
    };

    let current = query.get("status").map(String::as_str).unwrap_or("");
    // The counts come from the whole table rather than the filtered page, so
    // the tabs say how many there *are*, not how many are shown.
    let tabs: Vec<(String, String, Option<i64>, bool)> = [
        ("", "All", by_status.iter().map(|(_, n)| *n).sum::<i64>()),
        ("active", "Active", count("active")),
        ("suspended", "Suspended", count("suspended")),
        ("provisioning", "Setting up", count("provisioning")),
        ("failed", "Failed", count("failed")),
        ("pending_deletion", "Deleting", count("pending_deletion")),
    ]
    .iter()
    .map(|(value, label, n)| {
        (
            if value.is_empty() {
                "/console/sites".to_string()
            } else {
                format!("/console/sites?status={value}")
            },
            label.to_string(),
            Some(*n),
            *value == current,
        )
    })
    .collect();

    let mut rows = String::new();
    for tenant in &tenants {
        let running = state.supervisor.is_running(&tenant.id);
        rows.push_str(&format!(
            r#"<tr>
<td><a class="lead" href="/console/sites/{id}">{name}</a><span class="sub mono">{host}</span></td>
<td>{status} {running}</td>
<td>{plan}</td>
<td class="num">{storage}</td>
<td class="muted">{seen}</td></tr>"#,
            id = attr(&tenant.id),
            name = esc(&tenant.name),
            host = esc(&state.config.tenant_host(&tenant.slug)),
            status = view::tenant_pill(tenant.status),
            running = if running {
                view::status_pill("Up", PillKind::Ok)
            } else {
                view::status_pill("Asleep", PillKind::Idle)
            },
            plan = esc(&tenant.plan_slug),
            storage = esc(&pro_core::plans::bytes_display(tenant.storage_bytes)),
            seen = esc(&view::when(tenant.last_request_at.as_deref())),
        ));
    }

    let body = format!(
        r#"{head}{tabs}{search}{table}"#,
        head = view::page_head(
            "Sites",
            &format!("{} shown", view::number(tenants.len() as i64)),
            ""
        ),
        tabs = view::seg_bar(&tabs),
        search = view::search_form(
            "/console/sites",
            search.unwrap_or(""),
            "Search by address or title"
        ),
        table = if tenants.is_empty() {
            view::empty("No sites match", "Try a different filter or search.", None)
        } else {
            view::table(
                &["Site", "Status", "Plan", "num:Storage", "Last visit"],
                &rows,
            )
        },
    );

    Ok(shell(
        state,
        theme,
        "Sites",
        Tab::Sites,
        &member,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

pub async fn site(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin",
        &chrome,
        site_inner(&state, chrome.theme, &headers, &id, &query).await,
    )
}

async fn site_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    id: &str,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let member = auth::staff(state, headers).await?;
    let tenant = pro_db::tenants::get(&state.db, id).await?;
    let account = pro_db::accounts::get(&state.db, &tenant.account_id).await?;
    let domains = pro_db::domains::list_for_tenant(&state.db, &tenant.id).await?;
    let subscription = pro_db::subscriptions::for_tenant(&state.db, &tenant.id).await?;
    let usage = pro_db::usage::latest(&state.db, &tenant.id).await.unwrap_or(None);
    let events = pro_db::audit::list(&state.db, Some(&tenant.id), None, 20, 0).await?;
    let live = state.live();
    let running = state.supervisor.is_running(&tenant.id);
    let can_write = member.can(StaffRole::Admin);

    let scheme = if state.config.public_tls() { "https" } else { "http" };
    let host = domains
        .iter()
        .find(|d| d.is_primary)
        .map(|d| d.domain.clone())
        .unwrap_or_else(|| state.config.tenant_host(&tenant.slug));

    let mut body = view::page_head(
        &tenant.name,
        &format!(
            r#"<a class="mono" href="{scheme}://{host}" target="_blank" rel="noopener">{host}</a>
· owned by <a href="/console/customers/{account_id}">{email}</a><br>
{status} {running} <span class="muted small">port {port} · created {created}</span>"#,
            scheme = scheme,
            host = esc(&host),
            account_id = attr(&account.id),
            email = esc(&account.email),
            status = view::tenant_pill(tenant.status),
            running = if running {
                view::status_pill("Running", PillKind::Ok)
            } else {
                view::status_pill("Asleep", PillKind::Idle)
            },
            port = tenant.port,
            created = esc(&view::when(Some(&tenant.created_at))),
        ),
        &format!(
            r#"<a class="btn outlined" href="{scheme}://{host}/ghost/" target="_blank" rel="noopener">{open} Open admin</a>"#,
            scheme = scheme,
            host = attr(&host),
            open = view::svg("open"),
        ),
    );

    if let Some(reason) = &tenant.suspended_reason {
        body.push_str(&view::banner(
            BannerKind::Error,
            &format!(
                "<strong>Suspended {}:</strong> {}",
                esc(&view::when(tenant.suspended_at.as_deref())),
                esc(reason)
            ),
        ));
    }

    // ---- figures ----------------------------------------------------------
    body.push_str(r#"<div class="tiles">"#);
    body.push_str(&view::metric(
        "Members",
        &view::number(usage.as_ref().map(|u| u.members).unwrap_or(0)),
        None,
    ));
    body.push_str(&view::metric(
        "Posts",
        &view::number(usage.as_ref().map(|u| u.posts).unwrap_or(0)),
        None,
    ));
    body.push_str(&view::metric(
        "Storage",
        &pro_core::plans::bytes_display(tenant.storage_bytes),
        None,
    ));
    body.push_str(&view::metric(
        "Subscription",
        subscription.as_ref().map(|s| s.status.as_str()).unwrap_or("none"),
        subscription.as_ref().map(|s| s.provider.as_str()),
    ));
    body.push_str("</div>");

    body.push_str(&crate::email_usage::card(state, &tenant, can_write).await?);

    // ---- plan -------------------------------------------------------------
    let plan_options: String = live
        .config()
        .plans
        .iter()
        .map(|p| {
            format!(
                r#"<option value="{}"{}>{}</option>"#,
                attr(&p.slug),
                view::flag("selected", p.slug == tenant.plan_slug),
                esc(&p.name)
            )
        })
        .collect();

    body.push_str(&view::section("Plan"));
    body.push_str(&format!(
        r#"<div class="card padded">
<form method="post" action="/console/sites/{id}/plan">
<div class="inline-form">{plan}
<button class="btn" type="submit"{disabled}>Apply plan</button></div>
<p class="hint">Applying a plan rewrites the site's limits and restarts it. This does not
change what the customer is charged.</p></form></div>"#,
        id = attr(&tenant.id),
        disabled = view::flag("disabled", !can_write),
        plan = view::select("plan", "Plan", &plan_options, None, !can_write),
    ));

    // ---- domains ----------------------------------------------------------
    let domain_rows: String = if domains.is_empty() {
        view::empty_row(5, "No addresses on this site.")
    } else {
        domains
            .iter()
            .map(|d| {
                format!(
                    r#"<tr><td class="wrap"><span class="lead mono">{domain}</span>{primary}</td>
<td>{status}</td><td class="muted">{kind}</td><td class="muted">{checked}</td>
<td class="muted wrap">{error}</td></tr>"#,
                    domain = esc(&d.domain),
                    primary = if d.is_primary {
                        r#" <span class="chip tonal">Primary</span>"#
                    } else {
                        ""
                    },
                    status = if tenant.status.serves_public() {
                        view::domain_pill(d.status)
                    } else {
                        view::domain_pill_closed(d.status)
                    },
                    kind = esc(d.kind.as_str()),
                    checked = esc(&view::when(d.last_checked_at.as_deref())),
                    error = esc(d.last_error.as_deref().unwrap_or("")),
                )
            })
            .collect()
    };
    body.push_str(&view::section("Addresses"));
    body.push_str(&view::table(
        &["Domain", "Status", "Type", "Checked", "Last error"],
        &domain_rows,
    ));

    // ---- actions ----------------------------------------------------------
    if can_write {
        let suspend_or_restore = if tenant.status == TenantStatus::Suspended {
            view::row(
                "check",
                "green",
                "Restore this site",
                "Brings the public site back and reapplies its plan's limits.",
                &view::action_button(
                    &format!("/console/sites/{}/unsuspend", tenant.id),
                    "Restore site",
                    "small",
                    None,
                ),
            )
        } else {
            format!(
                r#"<div class="row"><span class="ic tint-red">{glyph}</span>
<div class="body" style="flex:1">
<span class="title">Suspend</span>
<span class="detail">Closes the public site and leaves the admin reachable, so the owner can
still sign in and pay. Nothing is deleted.</span>
{form}</div></div>"#,
                glyph = view::svg("lock"),
                form = view::form(
                    &format!("/console/sites/{}/suspend", tenant.id),
                    &format!(
                        r#"<div class="inline-form" style="margin-top:12px">{reason}
<button class="btn danger" type="submit">Suspend</button></div>"#,
                        reason = view::field(
                            "reason",
                            "Reason (shown to the customer)",
                            "text",
                            "",
                            None,
                            true
                        )
                    ),
                ),
            )
        };

        body.push_str(&view::section("Actions"));
        body.push_str(&view::rows(&format!(
            "{process}{backup}{suspend}",
            process = view::row(
                "restart",
                "amber",
                "Process",
                "Restart to pick up a configuration change; stop to free memory.",
                &format!("{restart}{stop}",
                    restart = view::action_button(
                        &format!("/console/sites/{}/restart", tenant.id),
                        "Restart",
                        "outlined small",
                        None
                    ),
                    stop = if running {
                        view::action_button(
                            &format!("/console/sites/{}/stop", tenant.id),
                            "Stop",
                            "text small",
                            None,
                        )
                    } else {
                        String::new()
                    },
                ),
            ),
            backup = view::row(
                "archive",
                "blue",
                "Backup",
                "Queues a full snapshot of this site.",
                &view::action_button(
                    &format!("/console/sites/{}/backup", tenant.id),
                    "Back up now",
                    "outlined small",
                    None
                ),
            ),
            suspend = suspend_or_restore,
        )));
    }

    // ---- backups ----------------------------------------------------------
    body.push_str(&console_backups_section(state, &tenant).await?);

    // ---- history ----------------------------------------------------------
    if !events.is_empty() {
        let event_rows: String = events
            .iter()
            .map(|e| {
                format!(
                    r#"<tr><td>{action}</td><td class="mono muted">{actor}</td>
<td class="muted">{when}</td></tr>"#,
                    action = esc(&e.action),
                    actor = esc(&e.actor),
                    when = esc(&view::when(Some(&e.created_at))),
                )
            })
            .collect();
        body.push_str(&view::section("History"));
        body.push_str(&view::table(&["Action", "Actor", "When"], &event_rows));
    }

    Ok(shell(
        state,
        theme,
        &tenant.name,
        Tab::Sites,
        &member,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

// ---- site actions ---------------------------------------------------------

#[derive(Deserialize)]
pub struct PlanForm {
    pub plan: String,
}

pub async fn set_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<PlanForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = auth::staff_with(&state, &headers, StaffRole::Admin).await?;
        let tenant = pro_db::tenants::get(&state.db, &id).await?;
        let live = state.live();
        let plan = live
            .config()
            .plan(form.plan.trim())
            .ok_or_else(|| ProError::BadRequest("Unknown plan.".into()))?
            .clone();

        pro_provision::apply_plan(&state.db, &state.config, &state.supervisor, &tenant, &plan)
            .await?;
        pro_db::audit::record(
            &state.db,
            &format!("staff:{}", member.id),
            "tenant.plan_changed",
            Some(&tenant.id),
            Some(&tenant.account_id),
            serde_json::json!({"to": plan.slug, "by": "operator"}),
        )
        .await;
        Ok(redirect_with(&back, Flash::ok(format!("Moved to {}.", plan.name))))
    }.await)
}

#[derive(Deserialize)]
pub struct AttachForm {
    pub tenant_id: String,
}

#[derive(Deserialize)]
pub struct SuspendForm {
    pub reason: String,
}

pub async fn suspend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<SuspendForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = auth::staff_with(&state, &headers, StaffRole::Admin).await?;
        let tenant = pro_db::tenants::get(&state.db, &id).await?;
        let reason = form.reason.trim();
        if reason.is_empty() {
            return Err(ProError::BadRequest("Give a reason — the customer sees it.".into()));
        }

        pro_provision::suspend_site(
            &state.db,
            &state.config,
            &state.supervisor,
            &tenant,
            reason,
        )
        .await?;
        pro_db::audit::record(
            &state.db,
            &format!("staff:{}", member.id),
            "tenant.suspended",
            Some(&tenant.id),
            Some(&tenant.account_id),
            serde_json::json!({"reason": reason}),
        )
        .await;
        Ok(redirect_with(&back, Flash::ok("Site suspended.")))
    }.await)
}

pub async fn unsuspend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = auth::staff_with(&state, &headers, StaffRole::Admin).await?;
        let tenant = pro_db::tenants::get(&state.db, &id).await?;
        let live = state.live();
        let plan = live
            .config()
            .plan(&tenant.plan_slug)
            .cloned()
            .ok_or_else(|| ProError::Config("this site's plan is not in the catalogue".into()))?;

        pro_provision::unsuspend_site(
            &state.db,
            &state.config,
            &state.supervisor,
            &tenant,
            &plan,
        )
        .await?;
        pro_db::audit::record(
            &state.db,
            &format!("staff:{}", member.id),
            "tenant.unsuspended",
            Some(&tenant.id),
            Some(&tenant.account_id),
            serde_json::json!({}),
        )
        .await;
        Ok(redirect_with(&back, Flash::ok("Site restored.")))
    }.await)
}

pub async fn restart(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        auth::staff_with(&state, &headers, StaffRole::Support).await?;
        let tenant = pro_db::tenants::get(&state.db, &id).await?;
        state.supervisor.restart(&tenant).await?;
        Ok(redirect_with(&back, Flash::ok("Restarted.")))
    }.await)
}

pub async fn stop(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        auth::staff_with(&state, &headers, StaffRole::Support).await?;
        state.supervisor.stop(&id).await?;
        Ok(redirect_with(
            &back,
            Flash::ok("Stopped. It will start again on the next visit."),
        ))
    }.await)
}

/// The console's backups card: every kept archive, downloadable, and — for
/// an operator — the button that resurrects a site from one.
///
/// Restore is offered on every row, including for live sites, because the
/// check that matters (no site directory) belongs to the restore itself,
/// where it cannot be raced out of date by a page rendered minutes ago.
async fn console_backups_section(state: &AppState, tenant: &pro_core::Tenant) -> Result<String> {
    let backups = pro_db::backups::list_for_tenant(&state.db, &tenant.id, 10).await?;

    let rows = if backups.is_empty() {
        view::empty_row(4, "No backups yet.")
    } else {
        backups
            .iter()
            .map(|b| {
                format!(
                    r#"<tr><td>{when}</td><td class="num">{size}</td><td>{reason}</td>
<td class="end"><a class="btn tonal small" href="/console/sites/{tenant}/backups/{id}">{dl} Download</a>
{restore}</td></tr>"#,
                    when = esc(&view::when(Some(&b.created_at))),
                    size = esc(&pro_core::plans::bytes_display(b.size_bytes)),
                    reason = esc(pro_db::backups::reason_label(&b.reason)),
                    tenant = attr(&tenant.id),
                    id = attr(&b.id),
                    dl = view::svg("download"),
                    restore = view::action_button(
                        &format!("/console/sites/{}/backups/{}/restore", tenant.id, b.id),
                        "Restore",
                        "text small",
                        Some(&format!(
                            "Restore from the archive taken {}?|This only runs if the site \
                             currently has no files — a live site is never overwritten.",
                            view::when(Some(&b.created_at))
                        ))
                    ),
                )
            })
            .collect()
    };

    Ok(format!(
        "{section}{table}",
        section = view::section("Backups"),
        table = view::table(&["Taken", "num:Size", "Type", "end:"], &rows),
    ))
}

pub async fn backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        auth::staff_with(&state, &headers, StaffRole::Support).await?;
        pro_db::jobs::enqueue_unique(
            &state.db,
            "backup_tenant",
            Some(&id),
            serde_json::json!({"reason": "manual"}),
        )
        .await?;
        Ok(redirect_with(&back, Flash::ok("Backup queued.")))
    }.await)
}

/// Download any site's archive.
pub async fn download_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, backup_id)): Path<(String, String)>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        auth::staff_with(&state, &headers, StaffRole::Support).await?;
        let tenant = pro_db::tenants::get(&state.db, &id).await?;

        let (backup, file) = crate::open_backup(&state, &tenant.id, &backup_id).await?;
        let filename = std::path::Path::new(&backup.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("{}.zip", tenant.slug));
        Ok(reply::download(&filename, file).await)
    }.await)
}

/// Restore a site from one of its archives.
///
/// Operator-initiated on purpose. Restoring replaces whatever the site's
/// directory holds, so it is queued for the worker rather than run in the
/// request, and the restore itself refuses to run while the directory still
/// exists — the operator is told the constraint rather than the platform
/// silently merging a live site with an old archive.
pub async fn restore_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, backup_id)): Path<(String, String)>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        auth::staff_with(&state, &headers, StaffRole::Admin).await?;
        let tenant = pro_db::tenants::get(&state.db, &id).await?;

        let backup = pro_db::backups::get(&state.db, &backup_id).await?;
        if backup.tenant_id != tenant.id {
            return Err(ProError::BadRequest("That archive belongs to a different site.".into()));
        }

        pro_db::jobs::enqueue_unique(
            &state.db,
            "restore_tenant",
            Some(&tenant.id),
            serde_json::json!({"backup_id": backup.id}),
        )
        .await?;
        pro_db::audit::record(
            &state.db,
            "console",
            "tenant.restore_scheduled",
            Some(&tenant.id),
            Some(&tenant.account_id),
            serde_json::json!({"backup": backup.id}),
        )
        .await;
        Ok(redirect_with(
            &back,
            Flash::ok("Restore queued. It only runs if the site has no files — a live site is never overwritten."),
        ))
    }.await)
}

/// Pin a subscription to the site it pays for.
///
/// The one act the webhook pipeline deliberately leaves to a person: when a
/// subscription arrives naming no site, on an account holding several,
/// guessing which one it entitles would occasionally put somebody else's
/// money on the wrong publication.
pub async fn attach_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(subscription_id): Path<String>,
    Form(form): Form<AttachForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/console/customers", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = auth::staff_with(&state, &headers, StaffRole::Admin).await?;
        let subscription = pro_db::subscriptions::get(&state.db, &subscription_id).await?;
        let tenant = pro_db::tenants::get(&state.db, form.tenant_id.trim()).await?;
        if tenant.account_id != subscription.account_id {
            return Err(ProError::BadRequest(
                "That site belongs to a different customer.".into(),
            ));
        }

        pro_db::subscriptions::attach_tenant(&state.db, &subscription.id, &tenant.id).await?;
        pro_db::audit::record(
            &state.db,
            &format!("staff:{}", member.id),
            "subscription.attached",
            Some(&tenant.id),
            Some(&subscription.account_id),
            serde_json::json!({"subscription": subscription.id, "plan": subscription.plan_slug}),
        )
        .await;
        Ok(redirect_with(
            &format!("/console/customers/{}", subscription.account_id),
            Flash::ok("Subscription attached."),
        ))
    }.await)
}

// ---------------------------------------------------------------------------
// Customers
// ---------------------------------------------------------------------------

pub async fn customers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin",
        &chrome,
        customers_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn customers_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let member = auth::staff(state, headers).await?;
    let search = query.get("q").map(String::as_str).filter(|s| !s.trim().is_empty());
    let accounts = pro_db::accounts::list(&state.db, search, 200, 0).await?;

    let mut rows = String::new();
    for account in &accounts {
        let sites = pro_db::tenants::count_for_account(&state.db, &account.id).await?;
        rows.push_str(&format!(
            r#"<tr><td><a class="lead" href="/console/customers/{id}">{email}</a>
<span class="sub">{name}</span></td>
<td class="num">{sites}</td>
<td>{verified}</td><td>{status}</td>
<td class="muted">{joined}</td></tr>"#,
            id = attr(&account.id),
            email = esc(&account.email),
            name = esc(&account.name),
            sites = view::number(sites),
            verified = if account.is_verified() {
                view::status_pill("Verified", PillKind::Ok)
            } else {
                view::status_pill("Unverified", PillKind::Warn)
            },
            status = if account.is_active() {
                view::status_pill("Active", PillKind::Ok)
            } else {
                view::status_pill("Blocked", PillKind::Bad)
            },
            joined = esc(&view::when(Some(&account.created_at))),
        ));
    }

    let body = format!(
        "{head}{search}{table}",
        head = view::page_head(
            "Customers",
            &format!("{} shown", view::number(accounts.len() as i64)),
            ""
        ),
        search = view::search_form(
            "/console/customers",
            search.unwrap_or(""),
            "Search by email or name"
        ),
        table = if accounts.is_empty() {
            view::empty("No customers match", "Try a different search.", None)
        } else {
            view::table(
                &["Customer", "num:Sites", "Email", "Status", "Joined"],
                &rows,
            )
        },
    );

    Ok(shell(
        state,
        theme,
        "Customers",
        Tab::Customers,
        &member,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

pub async fn customer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin",
        &chrome,
        customer_inner(&state, chrome.theme, &headers, &id, &query).await,
    )
}

async fn customer_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    id: &str,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let member = auth::staff(state, headers).await?;
    let account = pro_db::accounts::get(&state.db, id).await?;
    let tenants = pro_db::tenants::list_for_account(&state.db, &account.id).await?;
    let subscriptions = pro_db::subscriptions::list_for_account(&state.db, &account.id).await?;

    let site_rows: String = if tenants.is_empty() {
        view::empty_row(3, "This customer has no sites.")
    } else {
        tenants
            .iter()
            .map(|t| {
                format!(
                    r#"<tr><td><a class="lead" href="/console/sites/{id}">{name}</a></td>
<td>{status}</td><td>{plan}</td></tr>"#,
                    id = attr(&t.id),
                    name = esc(&t.name),
                    status = view::tenant_pill(t.status),
                    plan = esc(&t.plan_slug),
                )
            })
            .collect()
    };

    let sub_rows: String = if subscriptions.is_empty() {
        view::empty_row(6, "No subscriptions on this account.")
    } else {
        subscriptions
            .iter()
            .map(|s| {
                // A subscription the webhooks could not pin to a site — arrived
                // with no metadata, on an account with several. Only an operator
                // can say which site it pays for; the site list is right there.
                let attach = if s.tenant_id.is_none() && member.can(StaffRole::Admin) {
                    let options = tenants
                        .iter()
                        .map(|t| {
                            format!(
                                r#"<option value="{id}">{name} ({plan})</option>"#,
                                id = attr(&t.id),
                                name = esc(&t.name),
                                plan = esc(&t.plan_slug),
                            )
                        })
                        .collect::<String>();
                    if options.is_empty() {
                        String::new()
                    } else {
                        format!(
                            r#"<form method="post" action="/console/subscriptions/{id}/attach" class="inline" style="gap:6px;align-items:center">
{select}<button class="btn tonal small" type="submit">Attach</button></form>"#,
                            id = attr(&s.id),
                            select = view::selbox("tenant_id", &options, false, "Site"),
                        )
                    }
                } else {
                    String::new()
                };
                format!(
                    r#"<tr><td>{plan}</td><td>{provider}</td>
<td>{status}</td><td class="mono muted wrap">{pid}</td>
<td class="muted">{renews}</td><td class="end">{attach}</td></tr>"#,
                    plan = esc(&s.plan_slug),
                    provider = esc(&s.provider),
                    status = esc(&s.status),
                    pid = esc(s.provider_subscription_id.as_deref().unwrap_or("—")),
                    renews = esc(&view::when(s.current_period_end.as_deref())),
                    attach = attach,
                )
            })
            .collect()
    };

    let actions = if member.can(StaffRole::Admin) {
        format!(
            "{section}{rows}",
            section = view::section("Actions"),
            rows = view::rows(&view::row(
                if account.is_active() { "lock" } else { "check" },
                if account.is_active() { "red" } else { "green" },
                if account.is_active() { "Block this customer" } else { "Unblock" },
                if account.is_active() {
                    "Signs them out and prevents sign-in. Their sites keep running."
                } else {
                    "Restores access to the dashboard."
                },
                &view::action_button(
                    &format!("/console/customers/{}/toggle", account.id),
                    if account.is_active() { "Block" } else { "Unblock" },
                    if account.is_active() { "danger small" } else { "small" },
                    Some(if account.is_active() {
                        "Block this customer?|They are signed out immediately and cannot sign \
                         in again until unblocked."
                    } else {
                        "Unblock this customer?|They will be able to sign in again."
                    }),
                ),
            )),
        )
    } else {
        String::new()
    };

    let body = format!(
        r#"{head}{section_sites}{sites}{section_subs}{subs}{actions}"#,
        head = view::page_head(
            &account.email,
            &format!(
                "{name} · joined {joined} · {verified} {status}",
                name = esc(&account.name),
                joined = esc(&view::when(Some(&account.created_at))),
                verified = if account.is_verified() {
                    view::status_pill("Verified", PillKind::Ok)
                } else {
                    view::status_pill("Unverified", PillKind::Warn)
                },
                status = if account.is_active() {
                    String::new()
                } else {
                    view::status_pill("Blocked", PillKind::Bad)
                },
            ),
            "",
        ),
        section_sites = view::section("Sites"),
        sites = view::table(&["Site", "Status", "Plan"], &site_rows),
        section_subs = view::section("Subscriptions"),
        subs = view::table(
            &["Plan", "Provider", "Status", "Provider ID", "Renews", "end:"],
            &sub_rows
        ),
        actions = actions,
    );

    Ok(shell(
        state,
        theme,
        &account.email,
        Tab::Customers,
        &member,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

pub async fn toggle_customer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/customers/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = auth::staff_with(&state, &headers, StaffRole::Admin).await?;
        let account = pro_db::accounts::get(&state.db, &id).await?;

        let (new_status, message) = if account.is_active() {
            ("blocked", "Customer blocked.")
        } else {
            ("active", "Customer unblocked.")
        };
        pro_db::accounts::set_status(&state.db, &account.id, new_status).await?;
        if new_status == "blocked" {
            // Blocking has to end their sessions, or they stay signed in
            // until the cookie expires.
            pro_db::sessions::destroy_all(
                &state.db,
                pro_db::sessions::Subject::Account,
                &account.id,
            )
            .await?;
        }

        pro_db::audit::record(
            &state.db,
            &format!("staff:{}", member.id),
            if new_status == "blocked" { "account.blocked" } else { "account.unblocked" },
            None,
            Some(&account.id),
            serde_json::json!({}),
        )
        .await;
        Ok(redirect_with(&back, Flash::ok(message)))
    }.await)
}

pub async fn save_email_policy(State(state): State<AppState>, Path(id): Path<String>, headers: HeaderMap, Form(form): Form<HashMap<String,String>>) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/sites/{id}");
    reply::or_redirect(&back,&chrome,async {
        auth::require_same_origin(&headers,&state.config.host())?;
        let actor = auth::staff_with(&state,&headers,StaffRole::Admin).await?;
        let tenant = pro_db::tenants::get(&state.db,&id).await?;
        let policy = match form.get("policy").map(String::as_str) {
            Some("inherit")=>None,Some("stop")=>Some(true),Some("continue")=>Some(false),
            _=>return Err(ProError::BadRequest("Choose a sending policy".into())),
        };
        pro_db::email_usage::set_policy(&state.db,&id,policy).await?;
        pro_db::jobs::enqueue(&state.db,"apply_tenant_mail",Some(&id),serde_json::json!({})).await?;
        pro_db::audit::record(&state.db,&format!("staff:{}",actor.id),"email.policy_changed",Some(&id),Some(&tenant.account_id),serde_json::json!({"auto_stop":policy})).await;
        Ok(redirect_with(&back,Flash::ok("Sending policy saved. The site update is queued.")))
    }.await)
}

pub async fn refresh_email_usage(State(state): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/sites/{id}");
    reply::or_redirect(&back,&chrome,async {
        auth::require_same_origin(&headers,&state.config.host())?;
        auth::staff_with(&state,&headers,StaffRole::Admin).await?;
        let tenant = pro_db::tenants::get(&state.db,&id).await?;
        if !tenant.status.is_routable() {return Err(ProError::BadRequest("This site cannot be started for usage reporting.".into()));}
        crate::email_usage::fetch(&state,&tenant,true).await?;
        Ok(redirect_with(&back,Flash::ok("Email usage refreshed.")))
    }.await)
}

// ---------------------------------------------------------------------------
// Settings — where the payment provider is chosen
// ---------------------------------------------------------------------------

/// A focused settings page, following the site's own settings directory.
struct SettingsSection {
    slug: &'static str,
    title: &'static str,
    group: &'static str,
    icon: &'static str,
    tint: &'static str,
    summary: &'static str,
}

impl SettingsSection {
    fn path(&self) -> String { format!("/console/settings/{}", self.slug) }
}

const SETTINGS_SECTIONS: &[SettingsSection] = &[
    SettingsSection { slug: "general", title: "General settings", group: "Platform", icon: "settings", tint: "pink", summary: "Platform name · Support email · Site hosting" },
    SettingsSection { slug: "signup", title: "Signup & security", group: "Signup", icon: "shield", tint: "teal", summary: "Account registration · Email verification · Disposable addresses" },
    SettingsSection { slug: "billing", title: "Billing", group: "Billing", icon: "billing", tint: "green", summary: "Payment provider · Checkout · Dunning · Plan product IDs" },
    SettingsSection { slug: "stripe", title: "Stripe", group: "Stripe", icon: "key", tint: "purple", summary: "API keys · Webhook signing secret" },
    SettingsSection { slug: "paddle", title: "Paddle", group: "Paddle", icon: "key", tint: "blue", summary: "API key · Client token · Webhooks · Sandbox" },
    SettingsSection { slug: "lemon-squeezy", title: "Lemon Squeezy", group: "Lemon Squeezy", icon: "key", tint: "amber", summary: "API key · Store ID · Webhooks · Test mode" },
    SettingsSection { slug: "gumroad", title: "Gumroad", group: "Gumroad", icon: "key", tint: "red", summary: "Access token · Ping token · Product verification" },
    SettingsSection { slug: "email", title: "Email", group: "Mail", icon: "mail", tint: "blue", summary: "Transactional email · SMTP · Sender identity · Auto-stop" },
    SettingsSection { slug: "ses", title: "Amazon SES", group: "Amazon SES", icon: "mail", tint: "amber", summary: "AWS region · SMTP endpoint & credentials · Newsletter send rate" },
    SettingsSection { slug: "mailgun", title: "Mailgun", group: "Mailgun", icon: "mail", tint: "purple", summary: "Sending domain · API key · Region · Webhooks" },
];

fn settings_section(slug: &str) -> Result<&'static SettingsSection> {
    SETTINGS_SECTIONS.iter().find(|section| section.slug == slug)
        .ok_or_else(|| ProError::NotFound("Settings page not found".into()))
}

fn settings_link(path: &str, title: &str, detail: &str, icon: &str, tint: &str) -> String {
    format!(
        r#"<a class="row" href="{path}"><span class="ic tint-{tint}">{icon}</span>
<div class="body"><span class="title">{title}</span><span class="detail">{detail}</span></div>
<div class="trailing chev">{chevron}</div></a>"#,
        path = attr(path), tint = tint, icon = view::svg(icon),
        title = esc(title), detail = esc(detail), chevron = view::svg("chevron"),
    )
}

pub async fn settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin", &chrome,
        settings_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn settings_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let member = auth::staff(state, headers).await?;
    let search = query.get("q").map(|q| q.trim()).unwrap_or("");
    let words: Vec<String> = search.split_whitespace().map(str::to_lowercase).collect();
    let matches = |text: &str| {
        let text = text.to_lowercase();
        words.iter().all(|word| text.contains(word))
    };
    let mut rows = String::new();
    for section in SETTINGS_SECTIONS {
        if words.is_empty() || matches(&format!("{} {} {}", section.title, section.group, section.summary)) {
            rows.push_str(&settings_link(&section.path(), section.title, section.summary, section.icon, section.tint));
        }
        if !words.is_empty() {
            for def in pro_core::settings::REGISTRY.iter().filter(|def| def.group == section.group && !matches!(def.kind, Kind::Managed)) {
                // Search metadata only; saved values and credentials never enter the index.
                if matches(&format!("{} {} {}", def.label, def.key, def.help)) {
                    rows.push_str(&settings_link(
                        &format!("{}#setting-{}", section.path(), def.key.replace('.', "_")),
                        def.label, section.title, section.icon, section.tint,
                    ));
                }
            }
        }
    }
    for (path, title, detail, icon, tint) in [
        ("/console/staff", "Staff", "People · Roles · Console access", "people", "teal"),
        ("/console/plans", "Plans", "Email allowances · Product IDs · Pricing", "billing", "green"),
        ("/console/activity", "History", "Platform activity · Audit log", "history", "slate"),
    ] {
        if words.is_empty() || matches(&format!("{title} {detail}")) {
            rows.push_str(&settings_link(path, title, detail, icon, tint));
        }
    }
    if rows.is_empty() {
        rows = view::row("search", "slate", "Nothing matches", "Try another word, or clear the search to browse the pages.", "");
    }
    let clear = if words.is_empty() { "" } else {
        r#"<p class="settings-search-reset"><a href="/console/settings">Clear search</a></p>"#
    };
    let body = format!(
        r#"<div class="settings-directory"><header class="page-head"><div class="heading"><h1>Settings</h1><p class="sub">Platform configuration</p></div></header>
{search}{clear}<div class="card rows" id="settings-groups">{rows}</div></div>"#,
        search = view::search_form("/console/settings", search, "Search all settings"),
    );
    Ok(shell(state, theme, "Settings", Tab::Settings, &member, Flash::from_query(query).as_ref(), &body))
}

pub async fn settings_group(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    let result = async {
        let member = auth::staff(&state, &headers).await?;
        let section = settings_section(&slug)?;
        settings_group_inner(&state, chrome.theme, &member, section, &query)
    }.await;
    reply::or_page_or_signin("/console/signin", &chrome, result)
}

fn settings_group_inner(
    state: &AppState,
    theme: Theme,
    member: &Staff,
    section: &SettingsSection,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let live = state.live();
    let views = pro_db::settings::view_for(&live, member.role);
    let fields: Vec<_> = views.iter().filter(|item| item.def.group == section.group && !matches!(item.def.kind, Kind::Managed)).collect();
    let editable = fields.iter().any(|item| item.editable);
    let save = if editable {
        format!(r#"<button class="btn" type="submit" form="settings-form">{} Save</button>"#, view::svg("check"))
    } else { String::new() };
    let mut body = format!(
        r#"<div class="settings-editor"><header class="page-head settings-head">
<a class="icon-btn settings-back" href="/console/settings" aria-label="Back to settings" title="Back to settings">{back}</a>
<div class="heading"><h1>{title}</h1><p class="sub">{summary}</p></div><div class="actions">{save}</div></header>"#,
        back = view::svg("chevron"), title = esc(section.title), summary = esc(section.summary),
    );
    if section.slug == "billing" {
        let provider = live.billing_provider();
        body.push_str(&if provider == "none" {
            view::banner(BannerKind::Info, "No payment provider is selected. Configure a provider below, then select it here when you are ready to accept payments.")
        } else if live.provider_is_configured(&provider) {
            view::banner(BannerKind::Ok, &format!("New checkouts use <strong>{}</strong>. Existing subscriptions keep their current provider.", esc(&provider)))
        } else {
            view::banner(BannerKind::Warn, &format!("<strong>{}</strong> is selected but needs credentials. Open its settings below.", esc(&provider)))
        });
    }
    if !editable {
        body.push_str(&view::banner(BannerKind::Info, "You can view these settings. Fields marked with a role can be changed by staff with that role."));
    }
    body.push_str(&format!(
        r#"<form method="post" action="{path}" id="settings-form">{fields}</form>"#,
        path = section.path(), fields = view::rows(&fields.iter().map(|item| render_setting(item)).collect::<String>()),
    ));
    let mut related = String::new();
    if section.slug == "billing" {
        for provider in SETTINGS_SECTIONS.iter().filter(|s| ["stripe", "paddle", "lemon-squeezy", "gumroad"].contains(&s.slug)) {
            related.push_str(&settings_link(&provider.path(), provider.title, provider.summary, provider.icon, provider.tint));
        }
        related.push_str(&settings_link("/console/plans", "Plans & email allowances", "Monthly email allowances and payment product IDs for every plan.", "key", "teal"));
    } else if section.slug == "email" {
        for provider in SETTINGS_SECTIONS.iter().filter(|s| ["ses", "mailgun"].contains(&s.slug)) {
            related.push_str(&settings_link(&provider.path(), provider.title, provider.summary, provider.icon, provider.tint));
        }
    } else if ["ses", "mailgun"].contains(&section.slug) {
        related.push_str(&settings_link("/console/settings/email", "Email delivery", "Choose the transactional transport, tenant provider, and verified sender.", "mail", "blue"));
    } else if ["stripe", "paddle", "lemon-squeezy", "gumroad"].contains(&section.slug) {
        related.push_str(&settings_link("/console/settings/billing", "Billing", "Choose the provider for new checkouts.", "billing", "green"));
    }
    if !related.is_empty() {
        body.push_str(&view::section("Related settings"));
        body.push_str(&format!(r#"<div class="card rows settings-directory">{related}</div>"#));
    }
    body.push_str("</div>");
    Ok(shell(state, theme, section.title, Tab::Settings, member, Flash::from_query(query).as_ref(), &body))
}

pub async fn save_settings_group(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let chrome = state.chrome(&headers);
    let section = match settings_section(&slug) {
        Ok(section) => section,
        Err(error) => return reply::or_page_or_signin("/console/signin", &chrome, Err(error)),
    };
    reply::or_redirect(&section.path(), &chrome, save_settings_inner(&state, &headers, &body, Some(section)).await)
}

/// Marks a boolean as having been *present* in a submission.
///
/// See the comment in [`render_setting`]: an unchecked checkbox sends nothing,
/// so without this a partial POST would read as every switch being off.
const PRESENT_PREFIX: &str = "__present.";

/// Render one setting from its declaration, as a settings row: the label and
/// its explanation on the left, the control alone in the trailing slot.
fn render_setting(item: &pro_db::settings::SettingView) -> String {
    let def = item.def;
    let id = def.key.replace('.', "_");
    let locked = !item.editable;

    let badges = {
        let mut out = String::new();
        if locked {
            out.push_str(&format!(
                r#"<span class="badge locked">{} only</span>"#,
                esc(def.min_role.label())
            ));
        }
        if item.overridden {
            out.push_str(r#"<span class="badge set">set here</span>"#);
        }
        out
    };

    // An unchecked checkbox submits *nothing at all*, so absence is the only
    // way to read "off". That makes a partial submission dangerous: a POST
    // that simply omits a switch would be read as turning it off, and
    // "signup.enabled" silently becoming false is exactly the kind of outage
    // nobody would attribute to a settings save.
    //
    // This hidden companion is what distinguishes the two cases. It is always
    // submitted alongside the checkbox, so the save path can tell "this form
    // contained the switch and it was off" from "this submission did not
    // mention the switch at all". Only the first changes anything.
    let control = match &def.kind {
        // Never reached: `settings_inner` filters these out, because a map has
        // no single control. Kept as an arm rather than a catch-all so adding
        // a new `Kind` is a compile error here, where the control is drawn.
        Kind::Managed => String::new(),
        Kind::Bool => format!(
            r#"{switch}<input type="hidden" name="{present}" value="1">"#,
            switch = view::switch(
                def.key,
                matches!(item.value.as_str(), "true" | "1" | "on" | "yes"),
                locked,
                def.label,
            ),
            present = attr(&format!("{PRESENT_PREFIX}{}", def.key)),
        ),
        Kind::Select(options) => {
            let opts: String = options
                .iter()
                .map(|(value, label)| {
                    format!(
                        r#"<option value="{}"{}>{}</option>"#,
                        attr(value),
                        view::flag("selected", *value == item.value),
                        esc(label)
                    )
                })
                .collect();
            view::selbox(def.key, &opts, locked, def.label)
        }
        Kind::Int { min, max } => format!(
            r#"<span class="textbox"><input type="number" id="{id}" name="{key}" value="{value}"
 min="{min}" max="{max}" aria-label="{label}"{dis}></span>"#,
            id = attr(&id),
            key = attr(def.key),
            value = attr(&item.value),
            label = attr(def.label),
            dis = view::flag("disabled", locked),
        ),
        Kind::Secret => format!(
            r#"<span class="textbox"><input type="password" id="{id}" name="{key}" value=""
 autocomplete="new-password" placeholder="{placeholder}" aria-label="{label}"{dis}></span>"#,
            id = attr(&id),
            key = attr(def.key),
            // A stored secret is never sent back to the browser; the
            // placeholder is how the console says one exists.
            placeholder = if item.secret_set {
                "•••••••• saved — type to replace"
            } else {
                "not set"
            },
            label = attr(def.label),
            dis = view::flag("disabled", locked),
        ),
        Kind::Text => format!(
            r#"<span class="textbox"><input type="text" id="{id}" name="{key}" value="{value}"
 aria-label="{label}"{dis}></span>"#,
            id = attr(&id),
            key = attr(def.key),
            value = attr(&item.value),
            label = attr(def.label),
            dis = view::flag("disabled", locked),
        ),
    };

    format!(
        r#"<div class="row" id="setting-{id}"><div class="body"><span class="title">{label}{badges}</span>
{help}</div><div class="trailing">{control}</div></div>"#,
        id = attr(&id),
        label = esc(def.label),
        badges = badges,
        help = if def.help.is_empty() {
            String::new()
        } else {
            format!(r#"<span class="detail">{}</span>"#, esc(def.help))
        },
        control = control,
    )
}

pub async fn save_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect(
        "/console/settings",
        &chrome,
        save_settings_inner(&state, &headers, &body, None).await,
    )
}

/// Apply a settings form.
///
/// The body is parsed by hand rather than into a struct, because the field set
/// is the registry and a struct would have to be edited in step with it. Two
/// details matter:
///
/// * a checkbox that is **off** sends nothing at all, so every boolean the
///   actor is allowed to edit has to be explicitly set from presence or
///   absence — otherwise switching one off would never save;
/// * a field the actor may not edit is skipped rather than refused, because
///   the browser submits disabled fields as absent and a locked *secret*
///   would otherwise read as "clear me".
async fn save_settings_inner(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    section: Option<&SettingsSection>,
) -> Result<Response> {
    auth::require_same_origin(headers, &state.config.host())?;
    let member = auth::staff(state, headers).await?;
    let back = section.map(SettingsSection::path).unwrap_or_else(|| "/console/settings".into());

    let submitted: HashMap<String, String> = form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    if let Some(section) = section {
        for key in submitted.keys() {
            let key = key.strip_prefix(PRESENT_PREFIX).unwrap_or(key);
            if pro_core::settings::def(key).is_some_and(|def| def.group != section.group) {
                return Err(ProError::BadRequest("This page only saves its own settings. Open the other section to change those fields.".into()));
            }
        }
    }
    let mut changes: Vec<(String, Option<serde_json::Value>)> = Vec::new();
    for def in pro_core::settings::REGISTRY {
        if section.is_some_and(|section| def.group != section.group) { continue; }
        // Not this actor's to change: leave whatever is stored alone.
        if !pro_core::settings::can_edit(def.key, member.role) {
            continue;
        }
        match &def.kind {
            // Presence of the *companion* decides whether this form carried
            // the switch at all; presence of the checkbox itself decides
            // whether it was on. A submission that mentions neither leaves the
            // stored value untouched.
            Kind::Bool => {
                let present = submitted.contains_key(&format!("{PRESENT_PREFIX}{}", def.key));
                if present {
                    let on = submitted.contains_key(def.key);
                    changes.push((def.key.to_string(), Some(serde_json::Value::Bool(on))));
                }
            }
            // An empty secret means "leave it as it is", not "clear it" —
            // otherwise opening the settings page and saving would wipe every
            // API key, since secrets are never rendered back into the form.
            Kind::Secret => {
                if let Some(raw) = submitted.get(def.key) {
                    if !raw.trim().is_empty() {
                        changes.push((
                            def.key.to_string(),
                            pro_core::settings::validate(def.key, raw)?,
                        ));
                    }
                }
            }
            _ => {
                if let Some(raw) = submitted.get(def.key) {
                    changes.push((def.key.to_string(), pro_core::settings::validate(def.key, raw)?));
                }
            }
        }
    }

    let before = state.live().billing_provider();
    state
        .settings
        .save(&state.db, &changes, member.role, &format!("staff:{}", member.id))
        .await?;
    let after = state.live().billing_provider();
    if changes.iter().any(|(key, _)| key.starts_with("mail.")) {
        pro_db::jobs::enqueue(&state.db, "sync_tenant_mail", None, serde_json::json!({})).await?;
    }

    pro_db::audit::record(
        &state.db,
        &format!("staff:{}", member.id),
        "settings.changed",
        None,
        None,
        // The *keys* that changed, never the values — this table must not
        // become a place where API keys are archived in the clear.
        serde_json::json!({
            "keys": changes.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            "provider_before": before,
            "provider_after": after,
        }),
    )
    .await;

    let mut message = if before != after {
        format!("Settings saved. New checkouts now go to {after}.")
    } else {
        "Settings saved.".to_string()
    };

    if changes.iter().any(|(key, _)| key.starts_with("mail.")) {
        message.push_str(" Tenant email updates are queued; running sites will restart to apply them.");
    }

    // An operator who just filled in SMTP details has to be told here whether
    // mail will now actually leave the machine. Saying only "Settings saved."
    // is how somebody configures SMTP, believes it worked, and finds out from
    // a customer-facing banner that every message is still going to the log —
    // with no indication of which field is missing.
    //
    // Only when this submission touched mail: on a development platform left
    // on the log transport deliberately, every unrelated save would otherwise
    // carry the same complaint. The console's own overview warns about that
    // standing state already.
    let touched_mail = changes.iter().any(|(k, _)| k.starts_with("mail."));
    let mail_problem = touched_mail
        .then(|| state.mailer().not_live_reason())
        .flatten();

    match mail_problem {
        Some(reason) => {
            message.push_str(&format!(" Mail is still not being sent: {reason}."));
            Ok(redirect_with(&back, Flash::warn(message)))
        }
        None => {
            if touched_mail {
                message.push_str(" Platform email delivery is configured.");
            }
            Ok(redirect_with(&back, Flash::ok(message)))
        }
    }
}

// ---------------------------------------------------------------------------
// Plans — which product each plan is, at each provider
// ---------------------------------------------------------------------------

/// The screen that makes a platform able to charge.
///
/// A plan's limits, price and name are deployment decisions and live in
/// `pro.json`. Its *product id at a provider* is not: it is created in the
/// provider's own dashboard, often after the platform is already running, and
/// until it is set that plan cannot be bought. Leaving it in the config file
/// meant an operator had to edit JSON on the server and restart to start
/// selling — so it is here, saved as the `billing.priceIds` override.
pub async fn plans_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin",
        &chrome,
        plans_inner(&state, chrome.theme, &headers, &query).await,
    )
}

/// What each provider calls the thing a plan maps to, and where to find it.
///
/// The wording matters more than it looks: an operator who pastes a Lemon
/// Squeezy *product* id where its *variant* id belongs gets a checkout that
/// 404s, and the two are adjacent in their dashboard.
fn price_id_hint(provider: &str) -> (&'static str, &'static str) {
    match provider {
        "stripe" => (
            "Price ID",
            "price_… from the product's pricing section. Not the product id (prod_…).",
        ),
        "paddle" => ("Price ID", "pri_… from Catalogue → Products → the price."),
        "lemonsqueezy" => (
            "Variant ID",
            "The numeric variant id — Store → Products → the product → the variant. \
             Not the product id: a checkout is created against a variant.",
        ),
        "gumroad" => (
            "Product permalink",
            "The part after /l/ in the product's url. Gumroad has no checkout API, so \
             this is what the customer is sent to.",
        ),
        _ => ("Price ID", "The provider's id for this plan."),
    }
}

async fn plans_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let member = auth::staff(state, headers).await?;
    let live = state.live();
    let is_owner = member.can(StaffRole::Owner);
    let active = live.billing_provider();

    // Every provider with credentials, plus whichever is active even if it is
    // half-configured — an operator fixing a broken provider needs to see its
    // fields, not have them hidden because it does not work yet.
    let mut providers = live.configured_providers();
    if active != "none" && !providers.contains(&active) {
        providers.push(active.clone());
    }

    let mut body = view::page_head(
        "Plans",
        &format!(
            "Monthly email allowances and payment product IDs. {}",
            match active.as_str() {
                "none" => "No provider is selected, so nothing is being charged.".to_string(),
                other => format!(
                    r#"New checkouts go to <strong>{}</strong>."#,
                    esc(other)
                ),
            }
        ),
        "",
    );

    body.push_str(&view::banner(BannerKind::Info, r#"Newsletter recipients and transactional emails share one monthly allowance, including on yearly subscriptions. Configure the default auto-stop behavior in <a href="/console/settings/email">Email settings</a>, or override it in a site's details."#));

    if providers.is_empty() {
        body.push_str(&view::banner(
            BannerKind::Info,
            r#"No payment provider is configured, so there are no product ids to set.
<a href="/console/settings/billing">Choose a provider</a> first."#,
        ));
    }

    // The gap that stops a platform selling: the active provider is set, but
    // no plan names a product at it. Said here, where it can be fixed.
    let sellable: Vec<&pro_core::Plan> = live
        .config()
        .plans
        .iter()
        .filter(|p| live.plan(&p.slug).map(|x| x.is_purchasable(&active)).unwrap_or(false))
        .collect();
    if active != "none" && sellable.is_empty() {
        body.push_str(&view::banner(
            BannerKind::Error,
            &format!(
                "<strong>Nothing can be bought yet.</strong> No plan has a {} id, so every \
                 checkout would fail. Fill one in below.",
                esc(&active)
            ),
        ));
    }

    let plans = live.plans();
    let mut cards = String::new();
    for plan in &plans {
        let fields: String = providers
            .iter()
            .map(|provider| {
                let (label, hint) = price_id_hint(provider);
                let value = plan.price_ids.get(provider).cloned().unwrap_or_default();
                // The field name carries both halves of the key, so the
                // handler can rebuild the map without a second source of
                // truth about which plans and providers exist.
                let name = format!("id.{}.{}", plan.slug, provider);
                format!(
                    r#"<div>{field}<p class="hint">{hint}</p></div>"#,
                    field = view::field_full(
                        &name,
                        &format!("{} — {label}", title_case(provider)),
                        "text",
                        &value,
                        None,
                        &format!(
                            r#" autocapitalize="none" spellcheck="false"{}"#,
                            view::flag("disabled", !is_owner)
                        ),
                    ),
                    hint = esc(hint),
                )
            })
            .collect();

        let status = if active == "none" {
            view::status_pill("Not charged", PillKind::Idle)
        } else if plan.is_purchasable(&active) {
            view::status_pill("On sale", PillKind::Ok)
        } else if plan.hidden {
            view::status_pill("Hidden", PillKind::Idle)
        } else {
            view::status_pill("Cannot be bought", PillKind::Bad)
        };

        cards.push_str(&format!(
            r#"<div class="card padded" style="margin-bottom:16px">
<div class="toggle-row" style="margin-bottom:14px">
<div class="label"><b>{name}</b><span>{price} / {interval} · <code>{slug}</code></span></div>
{status}</div>
<div class="grid2">{fields}</div></div>"#,
            name = esc(&plan.name),
            price = esc(&plan.price_display()),
            interval = esc(&plan.interval),
            slug = esc(&plan.slug),
            status = status,
            fields = format!(r#"<div>{email}<p class="hint">Newsletter recipients and transactional emails per month, including on yearly billing. 0 stops sending when auto-stop is enabled.</p></div>{fields}"#,
                email=view::field_full(&format!("emails.{}",plan.slug),"Monthly email allowance","number",&plan.email_allowance().to_string(),None,&format!(r#" min="0" max="1000000000" step="1"{}"#,view::flag("disabled",!is_owner)))),
        ));
    }

    if !cards.is_empty() {
        body.push_str(&format!(
            r#"<form method="post" action="/console/plans">{cards}
<button class="btn" type="submit"{disabled}>{check} Save plans</button></form>"#,
            cards = cards,
            disabled = view::flag("disabled", !is_owner),
            check = view::svg("check"),
        ));
    }

    // Where each provider has to send its events, spelled out — this is the
    // step most often got wrong, and nothing works without it.
    body.push_str(&view::section("Webhook endpoints"));
    body.push_str(&view::banner(
        BannerKind::Info,
        "A subscription is only kept in step by its provider's webhooks: renewals, \
         cancellations and recovered payments all arrive that way. Add the endpoint for \
         whichever provider you use, with its signing secret in Settings.",
    ));
    let hook_rows: String = ["stripe", "paddle", "lemonsqueezy", "gumroad"]
        .iter()
        .map(|provider| {
            let url = state.url(&format!("/webhooks/{provider}"));
            format!(
                r#"<tr><td><span class="lead">{name}</span>{active_chip}</td>
<td class="mono wrap">{url}</td><td class="wrap muted">{events}</td></tr>"#,
                name = title_case(provider),
                active_chip = if *provider == active {
                    r#" <span class="chip tonal">Active</span>"#
                } else {
                    ""
                },
                url = esc(&if *provider == "gumroad" {
                    // Gumroad signs nothing, so its token rides in the URL.
                    format!("{url}?token=<your ping token>")
                } else {
                    url
                }),
                events = esc(webhook_events(provider)),
            )
        })
        .collect();
    body.push_str(&view::table(&["Provider", "Endpoint", "Events to enable"], &hook_rows));

    if !is_owner {
        body.push_str(&format!(
            r#"<p class="footnote">Product ids can only be changed by the {} role.</p>"#,
            esc(StaffRole::Owner.label())
        ));
    }

    Ok(shell(
        state,
        theme,
        "Plans",
        Tab::Settings,
        &member,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

/// Which events each provider has to be told to send.
///
/// Not a complete list of what it *can* send — a list of what this platform
/// acts on, so an operator ticking boxes in a provider dashboard knows when to
/// stop. Anything else is acknowledged and ignored.
fn webhook_events(provider: &str) -> &'static str {
    match provider {
        "stripe" => {
            "customer.subscription.created, .updated, .deleted; \
             invoice.payment_succeeded, invoice.payment_failed"
        }
        "paddle" => {
            "subscription.created, .updated, .canceled; \
             transaction.completed, transaction.payment_failed"
        }
        "lemonsqueezy" => {
            "subscription_created, subscription_updated, subscription_cancelled, \
             subscription_resumed, subscription_expired, subscription_paused, \
             subscription_unpaused, subscription_plan_changed, \
             subscription_payment_success, subscription_payment_failed, \
             subscription_payment_recovered, subscription_payment_refunded"
        }
        "gumroad" => {
            "sale, refund, dispute, dispute_won, cancellation, subscription_updated, \
             subscription_ended, subscription_restarted"
        }
        _ => "",
    }
}

fn title_case(provider: &str) -> &'static str {
    match provider {
        "stripe" => "Stripe",
        "paddle" => "Paddle",
        "lemonsqueezy" => "Lemon Squeezy",
        "gumroad" => "Gumroad",
        _ => "Provider",
    }
}

/// Save the product ids.
///
/// Parsed by hand like the settings form, and for the same reason: the field
/// set is the plan catalogue crossed with the configured providers, which no
/// struct can be written against. Each field is `id.<plan>.<provider>`.
pub async fn save_plans(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect(
        "/console/plans",
        &chrome,
        save_plans_inner(&state, &headers, &body).await,
    )
}

async fn save_plans_inner(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
) -> Result<Response> {
    auth::require_same_origin(headers, &state.config.host())?;
    // Owner only, like every other billing credential — and enforced here
    // rather than relying on the fields having been rendered disabled.
    let member = auth::staff_with(state, headers, StaffRole::Owner).await?;

    let submitted: HashMap<String, String> = form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    let live = state.live();
    let known_providers = ["stripe", "paddle", "lemonsqueezy", "gumroad"];

    // Start from what is stored, so a submission that carries only the
    // providers currently configured does not wipe the id of one that is
    // temporarily switched off — a platform migrating away from Stripe still
    // needs its price ids to keep billing existing customers.
    let mut map: serde_json::Map<String, serde_json::Value> = live
        .price_id_overrides()
        .into_iter()
        .map(|(slug, ids)| {
            (
                slug,
                serde_json::Value::Object(
                    ids.into_iter()
                        .map(|(p, v)| (p, serde_json::Value::String(v)))
                        .collect(),
                ),
            )
        })
        .collect();

    let mut email_limits: serde_json::Map<String,serde_json::Value> = live.plans().iter().map(|plan|(plan.slug.clone(),serde_json::json!(plan.email_allowance()))).collect();
    let mut limits_changed = false;
    let mut touched = 0usize;
    for (field,value) in &submitted {
        if let Some(slug) = field.strip_prefix("emails.") {
            let plan = live.plan(slug).ok_or_else(||ProError::BadRequest("Unknown plan in email allowances".into()))?;
            let amount = value.parse::<i64>().ok().filter(|n|(0..=1_000_000_000).contains(n)).ok_or_else(||ProError::BadRequest("Email allowances must be whole numbers from 0 to 1000000000".into()))?;
            if amount != plan.email_allowance() { limits_changed=true; touched+=1; }
            email_limits.insert(slug.to_string(),serde_json::json!(amount));
        }
    }
    for (field, value) in &submitted {
        let Some(rest) = field.strip_prefix("id.") else { continue };
        // `id.<plan>.<provider>` — split from the right, because a plan slug
        // may contain dots while a provider name never does.
        let Some((slug, provider)) = rest.rsplit_once('.') else { continue };
        if !known_providers.contains(&provider) {
            return Err(ProError::BadRequest(format!("unknown provider: {provider}")));
        }
        // Only plans that exist, so a crafted field cannot invent one.
        if state.config.plan(slug).is_none() {
            return Err(ProError::BadRequest(format!("unknown plan: {slug}")));
        }

        let entry = map
            .entry(slug.to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        let Some(ids) = entry.as_object_mut() else { continue };

        let trimmed = value.trim();
        let before = ids.get(provider).and_then(|v| v.as_str()).unwrap_or("");
        if trimmed == before {
            continue;
        }
        touched += 1;
        if trimmed.is_empty() {
            ids.remove(provider);
        } else {
            ids.insert(provider.to_string(), serde_json::Value::String(trimmed.to_string()));
        }
    }

    if touched == 0 {
        return Ok(redirect_with("/console/plans", Flash::ok("Nothing to change.")));
    }

    state
        .settings
        .save(
            &state.db,
            &[("billing.priceIds".to_string(), Some(serde_json::Value::Object(map))),
              ("billing.emailLimits".to_string(), Some(serde_json::Value::Object(email_limits)))],
            member.role,
            &format!("staff:{}", member.id),
        )
        .await?;

    pro_db::audit::record(
        &state.db,
        &format!("staff:{}", member.id),
        "settings.plan_ids_changed",
        None,
        None,
        // The count, not the ids: a price id is not a secret, but this table
        // is not where the catalogue belongs either.
        serde_json::json!({"changed": touched}),
    )
    .await;

    if limits_changed { pro_db::jobs::enqueue(&state.db,"sync_tenant_mail",None,serde_json::json!({})).await?; }

    // What the operator actually wants to know: can the platform sell now?
    let live = state.live();
    let provider = live.billing_provider();
    let on_sale = live.purchasable_plans().len();
    let message = if provider == "none" {
        format!("Saved {touched} plan setting(s).")
    } else if on_sale == 0 {
        format!(
            "Saved, but no plan has a {provider} id yet — nothing can be bought."
        )
    } else {
        format!("Saved. {on_sale} plan(s) can now be bought through {provider}. Email allowance updates are queued when changed.")
    };
    Ok(redirect_with("/console/plans", Flash::ok(message)))
}

// ---------------------------------------------------------------------------
// Staff
// ---------------------------------------------------------------------------

pub async fn staff_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin",
        &chrome,
        staff_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn staff_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let member = auth::staff(state, headers).await?;
    let all = pro_db::staff::list(&state.db).await?;
    let is_owner = member.can(StaffRole::Owner);

    let rows: String = all
        .iter()
        .map(|s| {
            // An owner may change anyone's role but their own — demoting
            // yourself out of the console is not a mistake worth offering.
            let role_cell = if is_owner && s.id != member.id {
                let options: String = member
                    .role
                    .assignable()
                    .iter()
                    .map(|r| {
                        format!(
                            r#"<option value="{}"{}>{}</option>"#,
                            attr(r.as_str()),
                            view::flag("selected", *r == s.role),
                            esc(r.label())
                        )
                    })
                    .collect();
                format!(
                    r#"<form method="post" action="/console/staff/{id}/role" class="inline" style="gap:6px">
{select}<button class="btn tonal small" type="submit">Set</button></form>"#,
                    id = attr(&s.id),
                    select = view::selbox("role", &options, false, "Role"),
                )
            } else {
                esc(s.role.label())
            };

            format!(
                r#"<tr><td><span class="lead">{name}</span><span class="sub">{email}</span></td>
<td>{role}</td><td>{status}</td><td class="muted">{seen}</td>
<td class="end">{toggle}</td></tr>"#,
                name = esc(&s.name),
                email = esc(&s.email),
                role = role_cell,
                status = if s.is_active() {
                    view::status_pill("Active", PillKind::Ok)
                } else {
                    view::status_pill("Disabled", PillKind::Idle)
                },
                seen = esc(&view::when(s.last_login_at.as_deref())),
                toggle = if is_owner && s.id != member.id {
                    view::action_button(
                        &format!("/console/staff/{}/toggle", s.id),
                        if s.is_active() { "Disable" } else { "Enable" },
                        if s.is_active() { "text danger small" } else { "text small" },
                        None,
                    )
                } else {
                    String::new()
                },
            )
        })
        .collect();

    let invite = if is_owner {
        format!(
            r#"{section}<div class="card padded">{form}</div>"#,
            section = view::section("Add a staff login"),
            form = view::form(
                "/console/staff",
                &format!(
                    r#"<div class="grid2">{name}{email}</div>
{password}
{role}
<button class="btn" type="submit">{add} Create login</button>"#,
                    name = view::field("name", "Name", "text", "", None, true),
                    email = view::field("email", "Email", "email", "", None, true),
                    password = view::field(
                        "password",
                        "Password",
                        "password",
                        "",
                        Some("At least 12 characters — these logins can suspend every site on the platform."),
                        true
                    ),
                    role = view::select(
                        "role",
                        "Role",
                        r#"<option value="support">Support — read only, can restart a site</option>
<option value="admin">Administrator — manage sites and customers</option>
<option value="owner">Owner — also change payment settings</option>"#,
                        None,
                        false
                    ),
                    add = view::svg("add"),
                )
            ),
        )
    } else {
        String::new()
    };

    let body = format!(
        "{head}{table}{invite}",
        head = view::page_head(
            "Staff",
            "Only an owner may change the payment provider or its credentials.",
            ""
        ),
        table = view::table(
            &["Name", "Role", "Status", "Last sign-in", "end:"],
            &rows
        ),
        invite = invite,
    );

    Ok(shell(
        state,
        theme,
        "Staff",
        Tab::Staff,
        &member,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

#[derive(Deserialize)]
pub struct NewStaffForm {
    pub name: String,
    pub email: String,
    pub password: String,
    pub role: String,
}

pub async fn create_staff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<NewStaffForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/console/staff", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = auth::staff_with(&state, &headers, StaffRole::Owner).await?;
        let role = StaffRole::parse(form.role.trim())
            .ok_or_else(|| ProError::BadRequest("Unknown role.".into()))?;
        if !member.role.assignable().contains(&role) {
            return Err(ProError::Forbidden(format!(
                "You may not grant the {} role.",
                role.label()
            )));
        }

        let created = pro_db::staff::create(
            &state.db,
            form.email.trim(),
            form.name.trim(),
            &form.password,
            role,
        )
        .await?;

        pro_db::audit::record(
            &state.db,
            &format!("staff:{}", member.id),
            "staff.created",
            None,
            None,
            serde_json::json!({"staff": created.id, "role": role.as_str()}),
        )
        .await;
        Ok(redirect_with("/console/staff", Flash::ok("Staff login created.")))
    }.await)
}

#[derive(Deserialize)]
pub struct RoleForm {
    pub role: String,
}

pub async fn set_staff_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RoleForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/console/staff", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = auth::staff_with(&state, &headers, StaffRole::Owner).await?;
        let role = StaffRole::parse(form.role.trim())
            .ok_or_else(|| ProError::BadRequest("Unknown role.".into()))?;

        pro_db::staff::set_role(&state.db, &member, &id, role).await?;
        pro_db::audit::record(
            &state.db,
            &format!("staff:{}", member.id),
            "staff.role_changed",
            None,
            None,
            serde_json::json!({"staff": id, "role": role.as_str()}),
        )
        .await;
        Ok(redirect_with("/console/staff", Flash::ok("Role updated.")))
    }.await)
}

pub async fn toggle_staff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/console/staff", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = auth::staff_with(&state, &headers, StaffRole::Owner).await?;
        let target = pro_db::staff::get(&state.db, &id).await?;
        let next = if target.is_active() { "disabled" } else { "active" };

        pro_db::staff::set_status(&state.db, &member, &id, next).await?;
        pro_db::audit::record(
            &state.db,
            &format!("staff:{}", member.id),
            "staff.status_changed",
            None,
            None,
            serde_json::json!({"staff": id, "status": next}),
        )
        .await;
        Ok(redirect_with("/console/staff", Flash::ok("Updated.")))
    }.await)
}

/// Replay one failed billing webhook.
pub async fn replay_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((provider, event_id)): Path<(String, String)>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/console/activity", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        auth::staff_with(&state, &headers, StaffRole::Admin).await?;
        // An event id in a URL is one an operator read off this screen; the
        // path-decoded value is matched exactly, never interpolated.
        let outcome = crate::webhooks::replay_event(&state, &provider, &event_id).await?;
        Ok(redirect_with(
            "/console/activity",
            Flash::ok(format!("Replayed: {outcome}")),
        ))
    }.await)
}

// ---------------------------------------------------------------------------
// Activity: audit trail, jobs, webhooks
// ---------------------------------------------------------------------------

pub async fn activity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin",
        &chrome,
        activity_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn activity_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let member = auth::staff(state, headers).await?;

    let events = pro_db::audit::list(&state.db, None, None, 100, 0).await?;
    let failed_jobs = pro_db::jobs::list_failed(&state.db, 50).await?;
    let failed_hooks = pro_db::audit::failed_webhooks(&state.db, 50).await?;

    let mut body = view::page_head(
        "Activity",
        "What the platform has done, and anything it could not finish.",
        "",
    );

    if !failed_jobs.is_empty() {
        let job_rows: String = failed_jobs
            .iter()
            .map(|j| {
                format!(
                    r#"<tr><td><span class="lead">{kind}</span></td><td class="mono muted">{tenant}</td>
<td class="muted wrap">{error}</td><td class="muted">{when}</td>
<td class="end">{retry}</td></tr>"#,
                    kind = esc(&j.kind),
                    tenant = esc(j.tenant_id.as_deref().unwrap_or("—")),
                    error = esc(j.last_error.as_deref().unwrap_or("")),
                    when = esc(&view::when(Some(&j.created_at))),
                    retry = view::action_button(
                        &format!("/console/jobs/{}/retry", j.id),
                        "Retry",
                        "tonal small",
                        None
                    ),
                )
            })
            .collect();
        body.push_str(&view::section("Failed background jobs"));
        body.push_str(&view::banner(
            BannerKind::Warn,
            "These have exhausted their retries and are waiting for a decision.",
        ));
        body.push_str(&view::table(
            &["Job", "Site", "Error", "Queued", "end:"],
            &job_rows,
        ));
    }

    if !failed_hooks.is_empty() {
        let hook_rows: String = failed_hooks
            .iter()
            .map(|(provider, event_id, event_type, error)| {
                format!(
                    r#"<tr><td><span class="lead">{provider}</span></td>
<td class="mono muted wrap">{event_id}</td>
<td>{event_type}</td><td class="muted wrap">{error}</td>
<td class="end">{replay}</td></tr>"#,
                    provider = esc(provider),
                    event_id = esc(event_id),
                    event_type = esc(event_type),
                    error = esc(error),
                    replay = view::action_button(
                        &format!(
                            "/console/webhooks/{}/{}/replay",
                            view::urlencode(provider),
                            view::urlencode(event_id)
                        ),
                        "Replay",
                        "tonal small",
                        Some(
                            "Re-run this event?|Its effects apply again. Idempotent effects are \
                             safe, but read the error first."
                        )
                    ),
                )
            })
            .collect();
        body.push_str(&view::section("Billing events that could not be processed"));
        body.push_str(&view::banner(
            BannerKind::Warn,
            "A customer's subscription may be out of step with what they are being charged. \
             Each is kept so it can be replayed.",
        ));
        body.push_str(&view::table(
            &["Provider", "Event", "Type", "Error", "end:"],
            &hook_rows,
        ));
    }

    let event_rows: String = if events.is_empty() {
        view::empty_row(4, "Nothing has happened yet.")
    } else {
        events
            .iter()
            .map(|e| {
                format!(
                    r#"<tr><td><span class="lead">{action}</span></td><td class="mono muted">{actor}</td>
<td class="mono">{tenant}</td><td class="muted">{when}</td></tr>"#,
                    action = esc(&e.action),
                    actor = esc(&e.actor),
                    tenant = e
                        .tenant_id
                        .as_deref()
                        .map(|t| format!(
                            r#"<a href="/console/sites/{}">{}</a>"#,
                            attr(t),
                            esc(&t[..t.len().min(8)])
                        ))
                        .unwrap_or_else(|| "—".into()),
                    when = esc(&view::when(Some(&e.created_at))),
                )
            })
            .collect()
    };
    body.push_str(&view::section("Audit trail"));
    body.push_str(&view::table(
        &["Action", "Actor", "Site", "When"],
        &event_rows,
    ));

    Ok(shell(
        state,
        theme,
        "Activity",
        Tab::Activity,
        &member,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

pub async fn retry_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/console/activity", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        auth::staff_with(&state, &headers, StaffRole::Admin).await?;
        pro_db::jobs::retry(&state.db, &id).await?;
        Ok(redirect_with("/console/activity", Flash::ok("Job requeued.")))
    }.await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_views(role: StaffRole) -> Vec<pro_db::settings::SettingView> {
        let config = {
            let mut c: pro_core::Config = serde_json::from_value(serde_json::json!({
                "url": "https://pro.example.com",
                "subdomainBase": "sites.example.com",
            }))
            .unwrap();
            c.plans = pro_core::plans::default_plans();
            std::sync::Arc::new(c)
        };
        let live = pro_core::Effective::new(config, Default::default());
        pro_db::settings::view_for(&live, role)
    }

    /// Every boolean must render a presence companion.
    ///
    /// Without it, a submission that omits a switch reads as the switch being
    /// off — and `signup.enabled` quietly becoming false is an outage nobody
    /// would trace back to a settings save. This is the regression guard for
    /// exactly that bug.
    #[test]
    fn booleans_render_a_presence_marker() {
        let views = owner_views(StaffRole::Owner);

        let mut checked = 0;
        for item in &views {
            let html = render_setting(item);
            if matches!(item.def.kind, Kind::Bool) {
                checked += 1;
                assert!(
                    html.contains(&format!(r#"name="__present.{}""#, item.def.key)),
                    "{} is a boolean with no presence marker: {html}",
                    item.def.key
                );
                // And it is an M3 switch now, not a bare checkbox.
                assert!(html.contains(r#"class="switch""#), "got {html}");
            } else {
                assert!(
                    !html.contains("__present."),
                    "{} is not a boolean and should have no marker",
                    item.def.key
                );
            }
        }
        assert!(checked >= 5, "expected several booleans, found {checked}");
    }

    /// A locked switch still renders its marker, so an administrator saving
    /// the form does not depend on the field being editable to keep its value
    /// — the save path's role check is what protects it.
    #[test]
    fn locked_booleans_still_render_a_marker() {
        let views = owner_views(StaffRole::Admin);

        let locked_bool = views
            .iter()
            .find(|v| matches!(v.def.kind, Kind::Bool) && !v.editable)
            .expect("an admin should see at least one locked switch");
        let html = render_setting(locked_bool);
        assert!(html.contains("disabled"));
        assert!(html.contains("__present."));
        // And it says *why* it is locked, rather than just being inert.
        assert!(html.contains("badge locked"), "got {html}");
    }

    /// A stored secret is never echoed back into the form — the placeholder is
    /// the only thing that says one exists.
    #[test]
    fn secrets_are_never_rendered_back() {
        let views = owner_views(StaffRole::Owner);
        let secret = views
            .iter()
            .find(|v| matches!(v.def.kind, Kind::Secret))
            .expect("the registry has secrets");
        let html = render_setting(secret);
        assert!(html.contains(r#"value="""#), "got {html}");
        assert!(html.contains(r#"type="password""#));
    }
    #[test]
    fn every_settings_group_has_one_directory_page() {
        for group in pro_core::settings::groups() {
            assert_eq!(SETTINGS_SECTIONS.iter().filter(|section| section.group == group).count(), 1, "{group} must have one page");
        }
        for (index, section) in SETTINGS_SECTIONS.iter().enumerate() {
            assert!(SETTINGS_SECTIONS[index + 1..].iter().all(|other| other.slug != section.slug));
            assert!(view::svg(section.icon).contains("<path"), "missing icon for {}", section.slug);
        }
    }

}
