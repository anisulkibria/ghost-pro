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
    Domains,
    Billing,
    Account,
}

/// The dashboard's navigation.
pub(crate) fn nav(active: Tab) -> Vec<NavItem> {
    vec![
        NavItem::new("Sites", "/dashboard", "sites", "blue", active == Tab::Sites),
        NavItem::new("Domains", "/dashboard/domains", "globe", "amber", active == Tab::Domains),
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

pub(crate) fn shell(
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
        crate::public::verify_notice(&account.email, state.mailer().is_live())
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
            let origin = state.config.public_origin(&host);
            let address = state.config.public_authority(&host);

            rows.push_str(&format!(
                r#"<tr>
<td><a class="lead" href="/dashboard/sites/{id}">{name}</a>
<span class="sub mono">{address}</span></td>
<td>{status} {sleeping}</td>
<td>{plan}</td>
<td class="muted">{seen}</td>
<td class="end"><a class="btn tonal small" href="{origin}/ghost/" rel="noopener" target="_blank">{open} Admin</a></td>
</tr>"#,
                id = view::attr(&tenant.id),
                name = esc(&tenant.name),
                address = esc(&address),
                origin = view::attr(&origin),
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

    // On a platform that takes payment first, say so on the form rather than
    // at the end of it: the next screen is the provider's, not the site's, and
    // a customer who has typed a password expects to know that before the
    // button moves them somewhere they have to put a card in.
    let payment_first = live.require_payment_up_front() && live.billing_provider() != "none";
    let (lede, action, note) = if payment_first {
        (
            "Payment first, then your site opens.",
            "Continue to payment",
            format!(
                r#"<p class="hint" style="margin:0 0 16px">Your address is reserved as soon as you
continue. The site stays closed to the public until the payment for
<b>{price} / {interval}</b> is confirmed, which is usually immediate.</p>"#,
                price = esc(&chosen.price_display()),
                interval = esc(&chosen.interval),
            ),
        )
    } else {
        ("This takes a few seconds.", "Create site", String::new())
    };

    let body = format!(
        r#"{head}
<div class="card padded">{form}</div>
<p class="footnote">Your site's own password is separate from your {platform} account
password — it signs you in to the publication's admin.</p>"#,
        head = view::page_head("New site", lede, ""),
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
{note}
<button class="btn block" type="submit">{rocket} {action}</button>"#,
                note = note,
                action = action,
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

    // Whether this site has to be paid for before it opens to the public.
    //
    // A platform on `provider = "none"` does not charge at all, so the setting
    // cannot mean anything there — asking for a payment nobody can make would
    // just stop sites being created. The provider is resolved *before* anything
    // is built, so a platform that demands payment it cannot actually take
    // fails here rather than leaving a site nobody can pay for.
    let payment_first = live.require_payment_up_front() && live.billing_provider() != "none";
    let provider = if payment_first { Some(pro_billing::require(&live)?) } else { None };

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

    pro_db::audit::record(
        &state.db,
        &format!("account:{}", account.id),
        "tenant.created",
        Some(&tenant.id),
        Some(&account.id),
        serde_json::json!({"slug": slug, "plan": plan.slug, "paid_first": payment_first}),
    )
    .await;

    // Payment first: the site stays closed and the customer goes straight to
    // the checkout, rather than getting a working site and a bill later.
    if let Some(provider) = provider {
        return start_paid_site(state, provider, &account, &plan, &tenant).await;
    }

    // A trial subscription so the plan's periodic limits have a start date and
    // the billing screens have something to show. With no payment provider
    // configured this is simply a record that the site exists on a plan.
    let provider_name = live.billing_provider();
    if provider_name == "none" {
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

/// A site on a platform that takes payment before the site opens: built,
/// closed, and handed to the provider's checkout.
///
/// The site is provisioned rather than deferred because the address has to be
/// claimed the moment the customer picks it — a reservation that lived only
/// until a webhook arrived would let two customers be sold the same subdomain.
/// What it is *not* is open: it is suspended, which is the same state a lapsed
/// site sits in, so the payment webhook that reopens one reopens this one too
/// and no second lifecycle has to be invented. The public half is closed and
/// the admin still answers, which is what lets the owner come back and pay.
///
/// No `site_ready` email goes out here. The site is not ready, and saying so
/// is the provider's job once the money has moved.
async fn start_paid_site(
    state: &AppState,
    provider: Box<dyn pro_billing::Provider>,
    account: &Account,
    plan: &pro_core::Plan,
    tenant: &Tenant,
) -> Result<Response> {
    pro_provision::suspend_site(
        &state.db,
        &state.config,
        &state.supervisor,
        tenant,
        pro_core::suspension::AWAITING_PAYMENT,
    )
    .await?;

    to_checkout(state, provider.as_ref(), account, plan, tenant, "site creation").await
}

/// Hand the customer to the provider's checkout for this site's own plan.
///
/// Shared by the first payment and by resuming an abandoned one, because they
/// differ only in what has already happened — the request to the provider is
/// the same, and so is what has to be true afterwards.
async fn to_checkout(
    state: &AppState,
    provider: &dyn pro_billing::Provider,
    account: &Account,
    plan: &pro_core::Plan,
    tenant: &Tenant,
    reason: &'static str,
) -> Result<Response> {
    let here = format!("/dashboard/sites/{}", tenant.id);
    let success = state.url(&format!(
        "{here}?ok={}",
        view::urlencode("Thank you — your site opens as soon as the payment is confirmed.")
    ));
    let cancel = state.url(&here);

    let checkout = provider
        .create_checkout(pro_billing::CheckoutRequest {
            account_id: &account.id,
            account_email: &account.email,
            tenant_id: Some(&tenant.id),
            plan,
            success_url: &success,
            cancel_url: &cancel,
            // No trial. Demanding payment before the site opens is the whole
            // of what this setting means, and a trial is the opposite of it.
            trial_days: 0,
        })
        .await;

    pro_db::audit::record(
        &state.db,
        &format!("account:{}", account.id),
        "checkout.started",
        Some(&tenant.id),
        Some(&account.id),
        serde_json::json!({
            "plan": plan.slug,
            "provider": provider.name(),
            "reason": reason,
            "started": checkout.is_ok(),
        }),
    )
    .await;

    match checkout {
        Ok(checkout) => Ok(reply::redirect(&checkout.url)),
        // Recoverable, not fatal. The site exists and is closed, so the
        // customer is shown it with the banner that offers the payment again.
        // Failing the request outright would leave them a site they could
        // neither see nor pay for.
        Err(e) => {
            tracing::error!(tenant = %tenant.slug, "could not start the checkout: {e}");
            Ok(redirect_with(
                &here,
                Flash::err(
                    "The checkout could not be started. Your site is reserved — \
                     please try the payment again.",
                ),
            ))
        }
    }
}

/// Start the payment for a site that is closed waiting for one.
///
/// Its own route rather than the billing screen's plan change, because that
/// one refuses a plan the site is already on — which is every site waiting for
/// its first payment.
pub async fn pay_site(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{id}");
    reply::or_redirect(&back, &chrome, pay_site_inner(&state, &headers, &id).await)
}

async fn pay_site_inner(state: &AppState, headers: &HeaderMap, id: &str) -> Result<Response> {
    auth::require_same_origin(headers, &state.config.host())?;
    let account = auth::account(state, headers).await?;
    let tenant = auth::owned_tenant(state, &account, id).await?;
    let live = state.live();

    // Only a site billing has closed. An operator's suspension is not a thing
    // a customer can pay their way out of.
    if !tenant.is_suspended_for_billing() {
        return Err(ProError::BadRequest(
            "This site is not waiting for a payment.".into(),
        ));
    }

    let provider = pro_billing::require(&live)?;
    let plan = live
        .plan(&tenant.plan_slug)
        .ok_or_else(|| ProError::Config(format!("plan {:?} is gone", tenant.plan_slug)))?;

    to_checkout(state, provider.as_ref(), &account, &plan, &tenant, "resumed").await
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
    // The address as the customer would type it, and the origin to link to —
    // both carry the edge's port, which in development is not 80.
    let address = state.config.public_authority(&host);
    let origin = state.config.public_origin(&host);
    let usage = pro_db::usage::latest(&state.db, &tenant.id).await.unwrap_or(None);
    let running = state.supervisor.is_running(&tenant.id);
    let subscription = pro_db::subscriptions::for_tenant(&state.db, &tenant.id).await?;

    // ---- header -----------------------------------------------------------
    let mut body = view::page_head(
        &tenant.name,
        &format!(
            r#"<a href="{origin}" rel="noopener" target="_blank" class="mono">{address}</a>
&nbsp;{status} {sleeping}"#,
            origin = attr(&origin),
            address = esc(&address),
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
            r#"<a class="btn" href="{origin}/ghost/" rel="noopener" target="_blank">{open} Open admin</a>"#,
            origin = attr(&origin),
            open = view::svg("open"),
        ),
    );

    // A site that has never been paid for reads differently from one that
    // lapsed: there is nothing yet to reassure the owner about, and the thing
    // to do is finish the payment rather than go and inspect a subscription.
    let awaiting_payment = tenant.suspended_reason.as_deref()
        == Some(pro_core::suspension::AWAITING_PAYMENT);
    if tenant.status == TenantStatus::Suspended && awaiting_payment {
        body.push_str(&view::banner(
            BannerKind::Warn,
            &format!(
                r#"<strong>Waiting for your payment.</strong> Your address is reserved and the admin
is ready, but the site stays closed to the public until the payment is confirmed. {pay}"#,
                pay = view::action_button(
                    &format!("/dashboard/sites/{}/pay", tenant.id),
                    "Complete payment",
                    "small",
                    None
                ),
            ),
        ));
    } else if tenant.status == TenantStatus::Suspended {
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

    if pro_provision::click::Settings::load(&state.config)?.is_some() {
        body.push_str(&view::section("Vuuut Click analytics"));
        type ClickSyncRow=(String,i64,i64,Option<String>,Option<String>);
        let sync:Option<ClickSyncRow>=sqlx::query_as("SELECT state,revision,synced_revision,last_error,verification_token FROM click_sync WHERE tenant_id=?").bind(&tenant.id).fetch_optional(&state.db).await.map_err(ProError::internal)?;
        if let Some((status,revision,synced,error,verification))=sync {
            body.push_str(&format!("<p>Status: {} · revision {synced}/{revision}</p>",esc(&status)));
            if let Some(error)=error {body.push_str(&format!("<p role=\"status\">{}</p>",esc(&error)));}
            if status=="waiting_dns" {
                if let Some(token)=verification {body.push_str(&format!("<p>Add a TXT record at <code>_vuuut-click.{}</code> with value <code>vuuut-click={}</code>.</p>",esc(&host),esc(&token)));}
            }
        }
        body.push_str(&format!(r#"<p><a href="{origin}/ghost/analytics/click/" target="_blank" rel="noopener">Open native analytics in your site's Admin</a></p><form method="post" action="/dashboard/sites/{id}/click"><button class="btn outlined small" name="action" value="retry">Retry sync</button> <button class="btn outlined small" name="action" value="rotate">Rotate credential</button> <button class="btn outlined small" name="action" value="disconnect">Disconnect</button> <button class="btn outlined small" name="action" value="connect">Reconnect</button></form>"#,origin=attr(&origin),id=attr(&tenant.id)));
    }
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
            r#"<tr><td class="wrap"><a class="lead mono" href="/dashboard/domains/{id}">{domain}</a>{primary}</td>
<td>{status}</td><td class="muted">{kind}</td>
<td class="end">{actions}</td></tr>"#,
            id = attr(&domain.id),
            domain = esc(&domain.domain),
            primary = if domain.is_primary {
                r#" <span class="chip tonal">Primary</span>"#
            } else {
                ""
            },
            status = if tenant.status.serves_public() {
                view::domain_pill(domain.status)
            } else {
                view::domain_pill_closed(domain.status)
            },
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

/// Where a domain action sends the customer afterwards.
///
/// The same buttons sit on the site page and on the domain's own page, and
/// each should come back to whichever one it was pressed on. Only these two
/// fixed values are honoured, never a path from the query: a redirect target
/// taken from the request is an open redirect.
#[derive(Deserialize, Default)]
pub struct DomainBack {
    #[serde(default)]
    pub back: Option<String>,
}

impl DomainBack {
    fn path(&self, tenant_id: &str, domain_id: &str) -> String {
        match self.back.as_deref() {
            Some("domain") => format!("/dashboard/domains/{domain_id}"),
            Some("domains") => "/dashboard/domains".into(),
            _ => format!("/dashboard/sites/{tenant_id}"),
        }
    }
}

pub async fn add_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<AddDomainForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/dashboard/sites/{id}");
    reply::or_redirect(&back, &chrome, async {
        let added = add_domain_inner(&state, &headers, &id, &form.domain).await?;
        Ok(redirect_with(
            &format!("/dashboard/domains/{}", added.id),
            Flash::ok(format!("{} added. Now point its DNS here.", added.domain)),
        ))
    }.await)
}

/// Attach a customer's own domain to one of their sites.
///
/// Shared by the site page's form and the Domains screen's, which differ only
/// in where the site id comes from.
pub(crate) async fn add_domain_inner(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    domain: &str,
) -> Result<pro_core::Domain> {
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
        serde_json::json!({"domain": normalised, "domain_id": added.id}),
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

    Ok(added)
}

pub async fn check_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_id, domain_id)): Path<(String, String)>,
    Query(to): Query<DomainBack>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = to.path(&tenant_id, &domain_id);
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let tenant = auth::owned_tenant(&state, &account, &tenant_id).await?;
        let domain = crate::domains::of_tenant(&state, &tenant, &domain_id).await?;
        Ok(crate::domains::check_and_report(&state, &domain, &back).await?)
    }.await)
}

pub async fn set_primary_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_id, domain_id)): Path<(String, String)>,
    Query(to): Query<DomainBack>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = to.path(&tenant_id, &domain_id);
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let tenant = auth::owned_tenant(&state, &account, &tenant_id).await?;
        crate::domains::make_primary(&state, &tenant, &domain_id, &format!("account:{}", account.id))
            .await?;
        Ok(redirect_with(&back, Flash::ok("Primary address updated.")))
    }.await)
}

pub async fn remove_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_id, domain_id)): Path<(String, String)>,
    Query(to): Query<DomainBack>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = to.path(&tenant_id, &domain_id);
    // The domain's own page is gone once it is removed, so a removal started
    // there lands on the list instead.
    let after = if to.back.as_deref() == Some("domain") {
        "/dashboard/domains".to_string()
    } else {
        back.clone()
    };
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let tenant = auth::owned_tenant(&state, &account, &tenant_id).await?;
        let removed =
            crate::domains::detach(&state, &tenant, &domain_id, &format!("account:{}", account.id))
                .await?;
        Ok(redirect_with(&after, Flash::ok(format!("{} removed.", removed.domain))))
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

/// Customer controls are scoped through the same tenant ownership check as
/// restarts and billing; no remote or tenant credentials enter the response.
pub async fn click_action(State(state):State<AppState>,headers:HeaderMap,Path(id):Path<String>,Form(form):Form<HashMap<String,String>>)->Response {
    let chrome=state.chrome(&headers);
    let back=format!("/dashboard/sites/{id}");
    reply::or_redirect(&back,&chrome,async {
        auth::require_same_origin(&headers,&state.config.host())?;
        let account=auth::account(&state,&headers).await?;
        auth::owned_tenant(&state,&account,&id).await?;
        if pro_provision::click::Settings::load(&state.config)?.is_none() {return Err(ProError::BadRequest("The platform has not configured Click".into()));}
        let action=form.get("action").map(String::as_str).unwrap_or("");
        pro_provision::click::action(&state.db,&id,action).await?;
        pro_db::audit::record(&state.db,&format!("account:{}",account.id),&format!("click.{action}"),Some(&id),Some(&account.id),serde_json::json!({})).await;
        Ok(redirect_with(&back,Flash::ok("Analytics change queued.")))
    }.await)
}
