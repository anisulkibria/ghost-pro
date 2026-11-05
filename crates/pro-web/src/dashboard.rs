//! The customer's panel: `/dashboard`.
//!
//! What a subscriber does here: install a Ghost site, attach their own domain,
//! see what their plan allows, and manage their subscription. The operator
//! console is a separate surface at `/console` with a separate login — see
//! [`crate::console`].
//!
//! Drawn with the Material 3 shell in [`crate::view`]: a nav rail down the
//! left, a sticky app bar per page, stat tiles for the figures, and settings
//! rows for anything with a control beside it.
use axum::extract::{Form, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use pro_core::{Account, DomainKind, ProError, Result, Tenant, TenantStatus};
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth;
use crate::reply::{self, redirect_with};
use crate::state::AppState;
use crate::view::{self, attr, esc, BannerKind, Flash, NavItem, Page, PillKind, Theme, Who};

/// Which rail entry is lit.
#[derive(PartialEq, Clone, Copy)]
pub(crate) enum Tab {
    Sites,
    Billing,
    Account,
}

/// The dashboard's navigation.
pub(crate) fn nav(active: Tab) -> Vec<NavItem> {
    vec![
        NavItem::new("Sites", "/dashboard", "sites", "blue", active == Tab::Sites),
        NavItem::new("Billing", "/dashboard/billing", "billing", "green", active == Tab::Billing),
        NavItem::new("Account", "/dashboard/account", "person", "purple", active == Tab::Account),
    ]
}

/// The account block in the rail's foot.
pub(crate) fn who(account: &Account) -> Who {
    Who {
        name: account.name.clone(),
        detail: account.email.clone(),
        href: Some("/dashboard/account".into()),
        signout: "/dashboard/signout".into(),
    }
}

fn shell(
    state: &AppState,
    theme: Theme,
    title: &str,
    tab: Tab,
    account: &Account,
    flash: Option<&Flash>,
    body: &str,
) -> Response {
    let platform = state.platform_name();
    let nav = nav(tab);
    let who = who(account);
    reply::html(view::page(Page {
        title,
        platform: &platform,
        tag: None,
        brand_href: "/dashboard",
        nav: &nav,
        who: Some(&who),
        flash,
        theme,
        body,
    }))
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
        "/dashboard/signin",
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
    let account = auth::account(state, headers).await?;
    let tenants = pro_db::tenants::list_for_account(&state.db, &account.id).await?;
    let live = state.live();

    // A customer who has not confirmed their address cannot provision yet,
    // and saying so plainly is better than a Create button that refuses.
    let unverified = live.require_email_verification() && !account.is_verified();
    let notice = if unverified {
        crate::public::verify_notice(&account.email)
    } else {
        String::new()
    };

    let body = if tenants.is_empty() {
        format!(
            "{head}{notice}{empty}",
            head = view::page_head(
                &format!("Welcome, {}", first_name(&account)),
                "You have no sites yet.",
                ""
            ),
            notice = notice,
            empty = view::empty(
                "Install your first site",
                "A Ghost publication on your own address, ready to write in, in a few seconds.",
                if unverified { None } else { Some(("Create a site", "/dashboard/new")) },
            ),
        )
    } else {
        let mut rows = String::new();
        for tenant in &tenants {
            let primary = pro_db::domains::primary_for_tenant(&state.db, &tenant.id)
                .await
                .unwrap_or(None);
            let host = primary
                .map(|d| d.domain)
                .unwrap_or_else(|| state.config.tenant_host(&tenant.slug));
            let running = state.supervisor.is_running(&tenant.id);
            let plan_name = live
                .config()
                .plan(&tenant.plan_slug)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| tenant.plan_slug.clone());
            let scheme = if state.config.public_tls() { "https" } else { "http" };

            rows.push_str(&format!(
                r#"<tr>
<td><a class="lead" href="/dashboard/sites/{id}">{name}</a>
<span class="sub mono">{host}</span></td>
<td>{status} {sleeping}</td>
<td>{plan}</td>
<td class="muted">{seen}</td>
<td class="end"><a class="btn tonal small" href="{scheme}://{host}/ghost/" rel="noopener" target="_blank">{open} Admin</a></td>
</tr>"#,
                id = view::attr(&tenant.id),
                name = esc(&tenant.name),
                host = esc(&host),
                scheme = scheme,
                open = view::svg("open"),
                status = view::tenant_pill(tenant.status),
                sleeping = if tenant.status != TenantStatus::Active {
                    String::new()
                } else if running {
                    view::status_pill("Running", PillKind::Ok)
                } else {
                    view::status_pill("Sleeping", PillKind::Idle)
                },
                plan = esc(&plan_name),
                seen = esc(&view::when(tenant.last_request_at.as_deref())),
            ));
        }

        format!(
            r#"{head}{notice}{table}
<p class="footnote">Sites with no recent traffic are paused to save resources, and wake
automatically on the next visit — usually within a second.</p>"#,
            head = view::page_head(
                "Your sites",
                &format!("{} site{}", tenants.len(), if tenants.len() == 1 { "" } else { "s" }),
                &format!(
                    r#"<a class="btn" href="/dashboard/new">{} New site</a>"#,
                    view::svg("add")
                ),
            ),
            notice = notice,
            table = view::table(
                &["Site", "Status", "Plan", "Last visit", "end:"],
                &rows
            ),
        )
    };

    Ok(shell(
        state,
        theme,
        "Sites",
        Tab::Sites,
        &account,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

fn first_name(account: &Account) -> &str {
    account.name.split_whitespace().next().unwrap_or(&account.name)
}

// ---------------------------------------------------------------------------
// Creating a site
// ---------------------------------------------------------------------------

pub async fn new_site_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/dashboard/signin",
        &chrome,
        new_site_form_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn new_site_form_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let account = auth::account(state, headers).await?;
    let live = state.live();

    if live.require_email_verification() && !account.is_verified() {
        return Err(ProError::Forbidden(
            "Please confirm your email address before creating a site.".into(),
        ));
    }

    let plans = live.purchasable_plans();
    if plans.is_empty() {
        return Err(ProError::Config("no plans are available".into()));
    }
    let chosen = query
        .get("plan")
        .and_then(|slug| live.plan(slug))
        .or_else(|| plans.first().cloned())
        .ok_or_else(|| ProError::Config("no plan to select".into()))?;

    let plan_options: String = plans
        .iter()
        .map(|p| {
            format!(
                r#"<option value="{}"{}>{} — {} / {}</option>"#,
                view::attr(&p.slug),
                view::flag("selected", p.slug == chosen.slug),
                esc(&p.name),
                esc(&p.price_display()),
                esc(&p.interval),
            )
        })
        .collect();

    let suggested = pro_core::ids::slugify(&account.name);
    let zone = esc(state.config.subdomain_base.trim_matches('.'));

    let body = format!(
        r#"{head}
<div class="card padded">{form}</div>
<p class="footnote">Your site's own password is separate from your {platform} account
password — it signs you in to the publication's admin.</p>"#,
        head = view::page_head("New site", "This takes a few seconds.", ""),
        platform = esc(&state.platform_name()),
        form = view::form(
            "/dashboard/new",
            &format!(
                r#"{title}
{slug}
<p class="hint" style="margin-top:-10px;margin-bottom:16px">Your address will be
<code>{suggested_host}</code>. Lowercase letters, numbers and hyphens; you can add your own
domain afterwards.</p>
{plan}
<p class="section" style="padding:12px 0 10px">Your site's login</p>
{owner_name}
{owner_email}
{password}
<button class="btn block" type="submit">{rocket} Create site</button>"#,
                title = view::field("title", "Site title", "text", "", None, true),
                slug = view::field_full(
                    "slug",
                    &format!("Address (.{zone})"),
                    "text",
                    &suggested,
                    None,
                    r#" required pattern="[a-z0-9-]+" maxlength="63" autocapitalize="none" spellcheck="false""#,
                ),
                suggested_host = esc(&format!("{suggested}.{zone}")),
                plan = view::select("plan", "Plan", &plan_options, None, false),
                owner_name = view::field("owner_name", "Your name", "text", &account.name, None, true),
                owner_email = view::field(
                    "owner_email",
                    "Email",
                    "email",
                    &account.email,
                    Some("You will sign in to your site's admin with this."),
                    true
                ),
                password = view::field(
                    "owner_password",
                    "Site password",
                    "password",
                    "",
                    Some("At least 10 characters."),
                    true
                ),
                rocket = view::svg("rocket"),
            )
        ),
    );

    Ok(shell(
        state,
        theme,
        "New site",
        Tab::Sites,
        &account,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

#[derive(Deserialize)]
pub struct NewSiteForm {
    pub title: String,
    pub slug: String,
    pub plan: String,
    pub owner_name: String,
    pub owner_email: String,
    pub owner_password: String,
}

pub async fn create_site(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<NewSiteForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect(
        "/dashboard/new",
        &chrome,
        create_site_inner(&state, &headers, form).await,
    )
}

async fn create_site_inner(
    state: &AppState,
    headers: &HeaderMap,
    form: NewSiteForm,
) -> Result<Response> {
    auth::require_same_origin(headers, &state.config.host())?;
    let account = auth::account(state, headers).await?;
    let live = state.live();

    if live.require_email_verification() && !account.is_verified() {
        return Err(ProError::Forbidden(
            "Please confirm your email address before creating a site.".into(),
        ));
    }

    let plan = live
        .config()
        .plan(form.plan.trim())
        .ok_or_else(|| ProError::BadRequest("Choose a plan.".into()))?
        .clone();

    let title = form.title.trim();
    if title.is_empty() {
        return Err(ProError::BadRequest("Give your site a title.".into()));
    }

    let slug = form.slug.trim().to_lowercase();
    state.config.slug_available(&slug)?;
    // Checked before building anything, though the unique index is the real
    // authority — two signups can always race between here and the insert.
    if pro_db::tenants::find_by_slug(&state.db, &slug).await?.is_some() {
        return Err(ProError::Conflict(format!(
            "“{slug}” is taken. Please choose another address."
        )));
    }

    let period_start = pro_core::now_iso();
    let tenant = pro_provision::create_site(
        &state.db,
        &state.config,
        &state.supervisor,
        &live.mail(),
        pro_provision::NewSite {
            account_id: &account.id,
            slug: &slug,
            title,
            plan: &plan,
            owner_name: form.owner_name.trim(),
            owner_email: form.owner_email.trim(),
            owner_password: &form.owner_password,
            period_start: &period_start,
        },
    )
    .await?;

    // The edge caches hostname lookups, and this tenant's subdomain is new.
    state.hosts.invalidate_all();

    // A trial subscription so the plan's periodic limits have a start date and
    // the billing screens have something to show. With no payment provider
    // configured this is simply a record that the site exists on a plan.
    let provider = live.billing_provider();
    if provider == "none" {
        let _ = pro_db::subscriptions::create_manual(
            &state.db,
            &account.id,
            Some(&tenant.id),
            &plan.slug,
        )
        .await;
    } else if plan.trial_days > 0 && !live.require_payment_up_front() {
        let _ = pro_db::subscriptions::upsert(
            &state.db,
            pro_db::subscriptions::Upsert {
                account_id: &account.id,
                tenant_id: Some(&tenant.id),
                plan_slug: Some(&plan.slug),
                provider: "manual",
                status: Some("trialing"),
                current_period_start: Some(&period_start),
                trial_ends_at: Some(&pro_core::days_from_now(plan.trial_days)),
                ..Default::default()
            },
        )
        .await;
    }

    pro_db::audit::record(
        &state.db,
        &format!("account:{}", account.id),
        "tenant.created",
        Some(&tenant.id),
        Some(&account.id),
        serde_json::json!({"slug": slug, "plan": plan.slug}),
    )
    .await;

    let site_url = state.config.tenant_url(&tenant.slug);
    let mailer = state.mailer();
    let _ = mailer
        .send(pro_mail::templates::site_ready(
            &live.platform_name(),
            &account.email,
            &account.name,
            &site_url,
        ))
        .await;

    Ok(redirect_with(
        &format!("/dashboard/sites/{}", tenant.id),
        Flash::ok("Your site is ready."),
    ))
}

// ---------------------------------------------------------------------------
// One site
// ---------------------------------------------------------------------------

pub async fn site(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/dashboard/signin",
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
    let account = auth::account(state, headers).await?;
    let tenant = auth::owned_tenant(state, &account, id).await?;
    let live = state.live();

    let plan = live.plan(&tenant.plan_slug).unwrap_or_else(|| {
        // A plan removed from the catalogue while a site is on it: show what
        // is known rather than failing the page.
        let mut placeholder = pro_core::plans::suspended_plan();
        placeholder.name = tenant.plan_slug.clone();
        placeholder
    });

    let domains = pro_db::domains::list_for_tenant(&state.db, &tenant.id).await?;
    let primary = domains.iter().find(|d| d.is_primary);
    let host = primary
        .map(|d| d.domain.clone())
        .unwrap_or_else(|| state.config.tenant_host(&tenant.slug));
    let scheme = if state.config.public_tls() { "https" } else { "http" };
    let usage = pro_db::usage::latest(&state.db, &tenant.id).await.unwrap_or(None);
    let running = state.supervisor.is_running(&tenant.id);
    let subscription = pro_db::subscriptions::for_tenant(&state.db, &tenant.id).await?;

    // ---- header -----------------------------------------------------------
    let mut body = view::page_head(
        &tenant.name,
        &format!(
            r#"<a href="{scheme}://{host}" rel="noopener" target="_blank" class="mono">{host}</a>
&nbsp;{status} {sleeping}"#,
            scheme = scheme,
            host = esc(&host),
            status = view::tenant_pill(tenant.status),
            sleeping = if tenant.status != TenantStatus::Active {
                String::new()
            } else if running {
                view::status_pill("Running", PillKind::Ok)
            } else {
                view::status_pill("Sleeping", PillKind::Idle)
            },
        ),
        &format!(
            r#"<a class="btn" href="{scheme}://{host}/ghost/" rel="noopener" target="_blank">{open} Open admin</a>"#,
            scheme = scheme,
            host = attr(&host),
            open = view::svg("open"),
        ),
    );

    if tenant.status == TenantStatus::Suspended {
        body.push_str(&view::banner(
            BannerKind::Error,
            &format!(
                r#"<strong>This site is not visible to the public.</strong> {reason} Nothing has been
deleted — your posts, members and settings are all intact, and the site comes back as soon
as this is resolved. <a href="/dashboard/billing">Go to billing</a>"#,
                reason = esc(tenant.suspended_reason.as_deref().unwrap_or("")),
            ),
        ));
    }
    if tenant.status == TenantStatus::PendingDeletion {
        body.push_str(&view::banner(
            BannerKind::Warn,
            &format!(
                r#"<strong>Scheduled for deletion {when}.</strong> You can still cancel this. {keep}"#,
                when = esc(&view::when(tenant.delete_after.as_deref())),
                keep = view::action_button(
                    &format!("/dashboard/sites/{}/restore", tenant.id),
                    "Keep this site",
                    "tonal small",
                    None
                ),
            ),
        ));
    }
    if tenant.status == TenantStatus::Failed {
        body.push_str(&view::banner(
            BannerKind::Error,
            "<strong>Setup did not finish.</strong> Our team has been notified. Please contact \
             support — your site can usually be recovered without starting again.",
        ));
    }

    // ---- usage ------------------------------------------------------------
    let members = usage.as_ref().map(|u| u.members).unwrap_or(0);
    let posts = usage.as_ref().map(|u| u.posts).unwrap_or(0);
    body.push_str(&view::section("This month"));
    body.push_str(r#"<div class="tiles">"#);
    body.push_str(&view::meter("Members", members, plan.max_members, view::number));
    body.push_str(&view::metric("Posts", &view::number(posts), None));
    body.push_str(&view::meter(
        "Storage",
        tenant.storage_bytes,
        plan.max_storage_bytes,
        pro_core::plans::bytes_display,
    ));
    body.push_str(&view::meter(
        "Emails",
        usage.as_ref().map(|u| u.emails_sent).unwrap_or(0),
        plan.max_emails_per_period,
        view::number,
    ));
    body.push_str("</div>");
    body.push_str(&match &usage {
        Some(u) => format!(
            r#"<p class="hint">Measured {}.</p>"#,
            esc(&view::when(Some(&u.sampled_at)))
        ),
        None => r#"<p class="hint">Usage is measured daily; figures appear within a day of
creating a site.</p>"#
            .to_string(),
    });

    // ---- domains ----------------------------------------------------------
    body.push_str(&domains_section(state, &tenant, &domains, &plan).await?);

    // ---- plan -------------------------------------------------------------
    body.push_str(&view::section("Plan"));
    body.push_str(&view::rows(&view::row(
        "billing",
        "green",
        &plan.name,
        &format!(
            "{price} / {interval} · {sub}",
            price = esc(&plan.price_display()),
            interval = esc(&plan.interval),
            sub = esc(&match &subscription {
                Some(s) if s.is_trialing() =>
                    format!("trial ends {}", view::when(s.trial_ends_at.as_deref())),
                Some(s) if s.status == "past_due" => "payment outstanding".to_string(),
                Some(s) if s.cancel_at_period_end =>
                    format!("ends {}", view::when(s.current_period_end.as_deref())),
                Some(s) if s.status == "active" =>
                    format!("renews {}", view::when(s.current_period_end.as_deref())),
                Some(s) => s.status.clone(),
                None => "no subscription".to_string(),
            }),
        ),
        r#"<a class="btn outlined small" href="/dashboard/billing">Change plan</a>"#,
    )));

    // ---- maintenance ------------------------------------------------------
    body.push_str(&view::section("Maintenance"));
    body.push_str(&view::rows(&format!(
        "{restart}{delete}",
        restart = view::row(
            "restart",
            "amber",
            "Restart",
            "Stops and starts the site. Visitors see a brief pause.",
            &view::action_button(
                &format!("/dashboard/sites/{}/restart", tenant.id),
                "Restart",
                "outlined small",
                None
            ),
        ),
        delete = if tenant.status == TenantStatus::PendingDeletion {
            String::new()
        } else {
            view::row(
                "delete",
                "red",
                "Delete this site",
                "Kept for 30 days first, so this can be undone.",
                &view::action_button(
                    &format!("/dashboard/sites/{}/delete", tenant.id),
                    "Delete site",
                    "danger small",
                    Some(
                        "Delete this site?|It will be kept for 30 days and can be restored \
                         within that time.",
                    ),
                ),
            )
        },
    )));

    body.push_str(&backups_section(state, &tenant).await?);

    Ok(shell(
        state,
        theme,
        &tenant.name,
        Tab::Sites,
        &account,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

/// The backups card: recent archives, each downloadable.
///
/// The marketing page promises backups "downloadable at any time"; this table
/// is that promise being kept. A handful are kept, oldest pruned first, so
/// "recent" here is all there is.
async fn backups_section(state: &AppState, tenant: &Tenant) -> Result<String> {
    let backups = pro_db::backups::list_for_tenant(&state.db, &tenant.id, 5).await?;

    let rows = if backups.is_empty() {
        view::empty_row(
            4,
            "No backups yet. The first nightly one is never more than a day away.",
        )
    } else {
        backups
            .iter()
            .map(|b| {
                format!(
                    r#"<tr><td>{when}</td><td class="num">{size}</td><td>{reason}</td>
<td class="end"><a class="btn tonal small" href="/dashboard/sites/{tenant}/backups/{id}">{dl} Download</a></td></tr>"#,
                    when = esc(&view::when(Some(&b.created_at))),
                    size = esc(&pro_core::plans::bytes_display(b.size_bytes)),
                    reason = esc(pro_db::backups::reason_label(&b.reason)),
                    tenant = attr(&tenant.id),
                    id = attr(&b.id),
                    dl = view::svg("download"),
                )
            })
            .collect()
    };

    Ok(format!(
        r#"{section}
<p class="hint" style="margin:0 0 12px">A full snapshot of the database, uploads and themes.
A few are kept; the oldest falls away. {create}</p>
{table}"#,
        section = view::section("Backups"),
        create = view::action_button(
            &format!("/dashboard/sites/{}/backup", tenant.id),
            "Create backup",
            "outlined small",
            None
        ),
        table = view::table(&["Taken", "num:Size", "Type", "end:"], &rows),
    ))
}

/// The domains card, including the DNS instructions for anything pending.
async fn domains_section(
    state: &AppState,
    tenant: &Tenant,
    domains: &[pro_core::Domain],
    plan: &pro_core::Plan,
) -> Result<String> {
    let custom_count = domains.iter().filter(|d| d.kind == DomainKind::Custom).count() as i64;
    let can_add = custom_count < plan.max_custom_domains;

    let mut rows = String::new();
    for domain in domains {
        let is_subdomain = domain.kind == DomainKind::Subdomain;
        rows.push_str(&format!(
            r#"<tr><td class="wrap"><span class="lead mono">{domain}</span>{primary}</td>
<td>{status}</td><td class="muted">{kind}</td>
<td class="end">{actions}</td></tr>"#,
            domain = esc(&domain.domain),
            primary = if domain.is_primary {
                r#" <span class="chip tonal">Primary</span>"#
            } else {
                ""
            },
            status = view::domain_pill(domain.status),
            kind = if is_subdomain { "Built in" } else { "Your domain" },
            actions = {
                let mut buttons = String::new();
                if !domain.is_primary && domain.status == pro_core::DomainStatus::Active {
                    buttons.push_str(&view::action_button(
                        &format!("/dashboard/sites/{}/domains/{}/primary", tenant.id, domain.id),
                        "Make primary",
                        "outlined small",
                        Some(
                            "Make this your main address?|Existing links will redirect to it.",
                        ),
                    ));
                }
                if !is_subdomain {
                    if domain.status != pro_core::DomainStatus::Active {
                        buttons.push_str(&view::action_button(
                            &format!("/dashboard/sites/{}/domains/{}/check", tenant.id, domain.id),
                            "Check now",
                            "tonal small",
                            None,
                        ));
                    }
                    buttons.push_str(&view::action_button(
                        &format!("/dashboard/sites/{}/domains/{}/remove", tenant.id, domain.id),
                        "Remove",
                        "text danger small",
                        Some("Remove this domain?|Visitors using it will stop reaching your site."),
                    ));
                }
                buttons
            },
        ));
    }

    // Instructions for whichever domain is still waiting.
    let pending = domains
        .iter()
        .find(|d| d.kind == DomainKind::Custom && d.status != pro_core::DomainStatus::Active);
    let instructions = match pending {
        Some(domain) => {
            let expect = crate::domain_expectation(state);
            let dns =
                pro_proxy::dns::instructions(&domain.domain, &domain.verification_token, &expect);
            let records: String = dns
                .records
                .iter()
                .map(|r| {
                    format!(
                        r#"<tr><td><span class="chip outline">{}</span></td><td class="mono">{}</td>
<td class="mono wrap">{}</td><td class="muted wrap">{}</td></tr>"#,
                        esc(r.kind),
                        esc(&r.name),
                        esc(&r.value),
                        esc(r.note),
                    )
                })
                .collect();
            format!(
                r#"<div class="card padded" style="margin-bottom:12px">
<p class="pop-title" style="margin:0 0 4px;padding:0">Point {domain} here</p>
<p class="hint" style="margin:0 0 14px">Add these records at your domain provider.{last_error}</p>
<div class="tbl-wrap"><table class="tbl">
<thead><tr><th>Type</th><th>Name</th><th>Value</th><th>Note</th></tr></thead>
<tbody>{records}</tbody></table></div>
<p class="hint">DNS changes can take up to an hour. We check automatically, and your
certificate is issued as soon as the records are visible.</p></div>"#,
                domain = esc(&domain.domain),
                last_error = domain
                    .last_error
                    .as_deref()
                    .map(|e| format!(" Last check: {}", esc(e)))
                    .unwrap_or_default(),
                records = records,
            )
        }
        None => String::new(),
    };

    let add_form = if can_add {
        format!(
            r#"<div class="card padded">{}</div>"#,
            view::form(
                &format!("/dashboard/sites/{}/domains", tenant.id),
                &format!(
                    r#"<div class="inline-form">{field}
<button class="btn" type="submit">{add} Add domain</button></div>
<p class="hint">For example <code>blog.yourcompany.com</code>, or <code>yourcompany.com</code>.</p>"#,
                    field = view::field("domain", "Your own domain", "text", "", None, true),
                    add = view::svg("add"),
                ),
            )
        )
    } else if plan.max_custom_domains == 0 {
        view::banner(
            BannerKind::Info,
            &format!(
                r#"Your {} plan uses the built-in address.
<a href="/dashboard/billing">Upgrade</a> to connect your own domain."#,
                esc(&plan.name)
            ),
        )
    } else {
        view::banner(
            BannerKind::Info,
            &format!(
                r#"You have used all {} custom domain{} on your {} plan.
<a href="/dashboard/billing">Upgrade</a> for more."#,
                plan.max_custom_domains,
                if plan.max_custom_domains == 1 { "" } else { "s" },
                esc(&plan.name)
            ),
        )
    };

    Ok(format!(
        "{section}{table}{instructions}{add_form}",
        section = view::section("Addresses"),
        table = view::table(&["Domain", "Status", "Type", "end:"], &rows),
    ))
}

#[derive(Deserialize)]
pub struct AddDomainForm {
    pub domain: String,
}

pub async fn add_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<AddDomainForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{id}");
    reply::or_redirect(
        &back,
        &chrome,
        add_domain_inner(&state, &headers, &id, &form.domain).await,
    )
}

async fn add_domain_inner(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    domain: &str,
) -> Result<Response> {
    auth::require_same_origin(headers, &state.config.host())?;
    let account = auth::account(state, headers).await?;
    let tenant = auth::owned_tenant(state, &account, tenant_id).await?;
    let live = state.live();

    let plan = live
        .config()
        .plan(&tenant.plan_slug)
        .ok_or_else(|| ProError::Config("this site's plan is no longer in the catalogue".into()))?;

    // The platform's own allowance, which Ghost knows nothing about — domains
    // are the platform's to route.
    let used = pro_db::domains::count_custom(&state.db, &tenant.id).await?;
    if used >= plan.max_custom_domains {
        return Err(ProError::PlanLimit {
            message: if plan.max_custom_domains == 0 {
                format!("Your {} plan does not include a custom domain.", plan.name)
            } else {
                format!(
                    "Your {} plan includes {} custom domain{}.",
                    plan.name,
                    plan.max_custom_domains,
                    if plan.max_custom_domains == 1 { "" } else { "s" }
                )
            },
            upgrade_url: Some("/dashboard/billing".into()),
        });
    }

    // A hostname under the platform's own zone would shadow a tenant
    // subdomain — or the platform itself.
    let normalised = pro_db::domains::validate_hostname(domain)?;
    if state.hosts.is_tenant_zone(&normalised)
        || normalised == pro_core::normalize_host(&state.config.subdomain_base)
        || normalised == state.config.host()
    {
        return Err(ProError::BadRequest(
            "That address belongs to this platform. Enter a domain you own.".into(),
        ));
    }

    let added =
        pro_db::domains::add(&state.db, &tenant.id, &normalised, DomainKind::Custom, false).await?;

    pro_db::audit::record(
        &state.db,
        &format!("account:{}", account.id),
        "domain.added",
        Some(&tenant.id),
        Some(&account.id),
        serde_json::json!({"domain": normalised}),
    )
    .await;

    // Check straight away: a customer who set their DNS up in advance should
    // not have to wait for the sweeper.
    pro_db::jobs::enqueue(
        &state.db,
        "verify_domain",
        Some(&tenant.id),
        serde_json::json!({"domain_id": added.id}),
    )
    .await?;

    Ok(redirect_with(
        &format!("/dashboard/sites/{tenant_id}"),
        Flash::ok(format!("{normalised} added. Now point its DNS here.")),
    ))
}

pub async fn check_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_id, domain_id)): Path<(String, String)>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{tenant_id}");
    reply::or_redirect(
        &back,
        &chrome,
        check_domain_inner(&state, &headers, &tenant_id, &domain_id).await,
    )
}

async fn check_domain_inner(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    domain_id: &str,
) -> Result<Response> {
    auth::require_same_origin(headers, &state.config.host())?;
    let account = auth::account(state, headers).await?;
    let tenant = auth::owned_tenant(state, &account, tenant_id).await?;

    let domain = pro_db::domains::get(&state.db, domain_id).await?;
    if domain.tenant_id != tenant.id {
        return Err(ProError::NotFound("No such domain on this site.".into()));
    }

    let verdict = crate::verify_domain_now(state, &domain).await?;
    let back = format!("/dashboard/sites/{tenant_id}");
    Ok(if verdict.is_servable() {
        redirect_with(&back, Flash::ok(format!("{} is live.", domain.domain)))
    } else {
        redirect_with(&back, Flash::err(verdict.message()))
    })
}

pub async fn set_primary_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_id, domain_id)): Path<(String, String)>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{tenant_id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let tenant = auth::owned_tenant(&state, &account, &tenant_id).await?;

        pro_db::domains::set_primary(&state.db, &tenant.id, &domain_id).await?;
        // The tenant's `url` is what every link it emits is built from, so it
        // has to be rewritten and the process restarted.
        pro_provision::update_primary_domain(
            &state.db,
            &state.config,
            &state.supervisor,
            &tenant,
        )
        .await?;
        state.hosts.invalidate_all();

        pro_db::audit::record(
            &state.db,
            &format!("account:{}", account.id),
            "domain.primary_changed",
            Some(&tenant.id),
            Some(&account.id),
            serde_json::json!({"domain_id": domain_id}),
        )
        .await;

        Ok(redirect_with(&back, Flash::ok("Primary address updated.")))
    }.await)
}

pub async fn remove_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_id, domain_id)): Path<(String, String)>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{tenant_id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let tenant = auth::owned_tenant(&state, &account, &tenant_id).await?;

        let domain = pro_db::domains::get(&state.db, &domain_id).await?;
        let was_primary = domain.is_primary;
        pro_db::domains::remove(&state.db, &tenant.id, &domain_id).await?;

        // Removing the canonical hostname moves the site back to its built-in
        // address, which means rewriting `url`.
        if was_primary {
            pro_provision::update_primary_domain(
                &state.db,
                &state.config,
                &state.supervisor,
                &tenant,
            )
            .await?;
        }
        state.hosts.invalidate(&domain.domain);
        state.hosts.invalidate_all();

        pro_db::audit::record(
            &state.db,
            &format!("account:{}", account.id),
            "domain.removed",
            Some(&tenant.id),
            Some(&account.id),
            serde_json::json!({"domain": domain.domain}),
        )
        .await;

        Ok(redirect_with(&back, Flash::ok("Domain removed.")))
    }.await)
}

// ---------------------------------------------------------------------------
// Site maintenance
// ---------------------------------------------------------------------------

pub async fn restart_site(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let tenant = auth::owned_tenant(&state, &account, &id).await?;
        state.supervisor.restart(&tenant).await?;
        Ok(redirect_with(&back, Flash::ok("Site restarted.")))
    }.await)
}

pub async fn backup_site(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let tenant = auth::owned_tenant(&state, &account, &id).await?;

        // Queued rather than run inline: archiving a large site takes longer
        // than a request should, and two at once would race on one directory.
        match pro_db::jobs::enqueue_unique(
            &state.db,
            "backup_tenant",
            Some(&tenant.id),
            serde_json::json!({"reason": "manual"}),
        )
        .await?
        {
            Some(_) => Ok(redirect_with(
                &back,
                Flash::ok("Backup started. It will appear here shortly."),
            )),
            None => Ok(redirect_with(&back, Flash::ok("A backup is already in progress."))),
        }
    }.await)
}

/// Download one of this account's site archives.
pub async fn download_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, backup_id)): Path<(String, String)>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let tenant = auth::owned_tenant(&state, &account, &id).await?;

        let (backup, file) = crate::open_backup(&state, &tenant.id, &backup_id).await?;
        let filename = std::path::Path::new(&backup.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("{}.zip", tenant.slug));
        Ok(reply::download(&filename, file).await)
    }.await)
}

pub async fn delete_site(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let tenant = auth::owned_tenant(&state, &account, &id).await?;

        // A snapshot before the clock starts, so even a customer who deletes
        // and then changes their mind after the window has something to
        // restore from.
        let _ = pro_db::jobs::enqueue(
            &state.db,
            "backup_tenant",
            Some(&tenant.id),
            serde_json::json!({"reason": "pre_delete"}),
        )
        .await;

        pro_db::tenants::schedule_deletion(&state.db, &tenant.id, 30).await?;
        pro_db::audit::record(
            &state.db,
            &format!("account:{}", account.id),
            "tenant.deletion_scheduled",
            Some(&tenant.id),
            Some(&account.id),
            serde_json::json!({"grace_days": 30}),
        )
        .await;

        Ok(redirect_with(
            &back,
            Flash::ok("Site scheduled for deletion. You can undo this for 30 days."),
        ))
    }.await)
}

pub async fn restore_site(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let tenant = auth::owned_tenant(&state, &account, &id).await?;

        if tenant.status != TenantStatus::PendingDeletion {
            return Err(ProError::BadRequest("This site is not scheduled for deletion.".into()));
        }
        pro_db::tenants::clear_deletion(&state.db, &tenant.id).await?;
        pro_db::audit::record(
            &state.db,
            &format!("account:{}", account.id),
            "tenant.deletion_cancelled",
            Some(&tenant.id),
            Some(&account.id),
            serde_json::json!({}),
        )
        .await;
        Ok(redirect_with(&back, Flash::ok("Deletion cancelled. Your site is safe.")))
    }.await)
}
