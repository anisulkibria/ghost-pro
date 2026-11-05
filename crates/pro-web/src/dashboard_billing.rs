//! The customer's billing and account screens.
//!
//! Same Material 3 shell as the rest of the dashboard: one filled card per
//! site's subscription, the plan chooser as a row of M3 plan cards, and the
//! account page as three small forms over the list of signed-in devices.
use axum::extract::{Form, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use pro_core::{Account, ProError, Result};
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth;
use crate::dashboard::{self, Tab};
use crate::reply::{self, redirect_with};
use crate::state::AppState;
use crate::view::{self, esc, Flash, Page, Theme};

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
    let nav = dashboard::nav(tab);
    let who = dashboard::who(account);
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
// Billing
// ---------------------------------------------------------------------------

pub async fn billing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/dashboard/signin",
        &chrome,
        billing_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn billing_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let account = auth::account(state, headers).await?;
    let live = state.live();
    let tenants = pro_db::tenants::list_for_account(&state.db, &account.id).await?;
    let subscriptions = pro_db::subscriptions::list_for_account(&state.db, &account.id).await?;

    let mut body = view::page_head(
        "Billing",
        if live.billing_provider() == "none" {
            "This platform is not currently charging for sites."
        } else {
            "One subscription per site."
        },
        "",
    );

    // ---- current subscriptions, one row per site --------------------------
    if tenants.is_empty() {
        body.push_str(&view::empty(
            "Nothing to bill yet",
            "Create a site and its subscription will appear here.",
            Some(("Create a site", "/dashboard/new")),
        ));
    } else {
        for tenant in &tenants {
            let subscription = subscriptions
                .iter()
                .find(|s| s.tenant_id.as_deref() == Some(tenant.id.as_str()));
            let plan = live.plan(&tenant.plan_slug);
            let plan_name = plan
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| tenant.plan_slug.clone());
            let price = plan
                .as_ref()
                .map(|p| p.price_display())
                .unwrap_or_else(|| "—".into());
            let interval = plan.as_ref().map(|p| p.interval.clone()).unwrap_or_default();

            let (state_label, detail) = match subscription {
                Some(s) if s.is_trialing() => (
                    view::status_pill("Trial", view::PillKind::Warn),
                    format!("Trial ends {}", view::when(s.trial_ends_at.as_deref())),
                ),
                Some(s) if s.status == "past_due" => (
                    view::status_pill("Payment due", view::PillKind::Bad),
                    format!(
                        "A payment failed. Your site goes offline {} unless this is resolved.",
                        view::when(
                            s.past_due_since
                                .as_deref()
                                .map(|since| {
                                    pro_core::parse_iso(since)
                                        .map(|d| {
                                            (d + chrono::Duration::days(
                                                live.dunning_grace_days(),
                                            ))
                                            .format("%Y-%m-%dT%H:%M:%SZ")
                                            .to_string()
                                        })
                                        .unwrap_or_default()
                                })
                                .as_deref()
                        )
                    ),
                ),
                Some(s) if s.cancel_at_period_end => (
                    view::status_pill("Ending", view::PillKind::Warn),
                    format!("Ends {}", view::when(s.current_period_end.as_deref())),
                ),
                Some(s) if s.status == "active" => (
                    view::status_pill("Active", view::PillKind::Ok),
                    format!("Renews {}", view::when(s.current_period_end.as_deref())),
                ),
                Some(s) if s.provider == "manual" => (
                    view::status_pill("Included", view::PillKind::Ok),
                    "Granted by the platform.".to_string(),
                ),
                Some(s) => (view::status_pill(&s.status, view::PillKind::Idle), String::new()),
                None => (
                    view::status_pill("No subscription", view::PillKind::Idle),
                    String::new(),
                ),
            };

            let (manage, cancel) = match subscription {
                Some(s) if s.provider != "manual" && s.provider_subscription_id.is_some() => {
                    // Built from the subscription's *own* provider, not the
                    // platform's current one: what a customer can do with a
                    // subscription is decided by where it lives, and a
                    // platform that has since switched provider must not
                    // change the controls on an existing one.
                    let provider = pro_billing::build(&s.provider, &live);

                    let manage = view::action_button(
                        &format!("/dashboard/billing/manage/{}", s.id),
                        "Manage payment",
                        "outlined small",
                        None,
                    );

                    // Offered while the subscription still runs; once it is
                    // ending there is nothing left to cancel, and the
                    // provider's own page is the way to change one's mind.
                    let could_cancel = matches!(
                        s.status.as_str(),
                        "active" | "trialing" | "past_due"
                    ) && !s.cancel_at_period_end;

                    // Some providers have no cancellation endpoint at all —
                    // Gumroad is one. A button that always errors is worse
                    // than a sentence saying where to go, so the capability
                    // is asked for rather than assumed.
                    let supported = provider.as_ref().map(|p| p.can_cancel()).unwrap_or(true);

                    let cancel = match (could_cancel, supported) {
                        (true, true) => view::action_button(
                            &format!("/dashboard/billing/cancel/{}", s.id),
                            "Cancel",
                            "text danger small",
                            Some(
                                "Cancel this subscription?|The site keeps running to the end of \
                                 what you have already paid for.",
                            ),
                        ),
                        (true, false) => format!(
                            r#"<span class="chip outline" title="This provider has no cancellation API">{} Cancel at {}</span>"#,
                            view::svg("open"),
                            esc(&s.provider),
                        ),
                        _ => String::new(),
                    };
                    (manage, cancel)
                }
                _ => (String::new(), String::new()),
            };

            body.push_str(&view::rows(&view::row(
                "sites",
                "blue",
                &tenant.name,
                &format!(
                    "{label} {plan_name} · {price} / {interval}{detail}",
                    label = state_label,
                    plan_name = esc(&plan_name),
                    price = esc(&price),
                    interval = esc(&interval),
                    detail = if detail.is_empty() {
                        String::new()
                    } else {
                        format!("<br>{}", esc(&detail))
                    },
                ),
                &format!(
                    r#"{manage}{cancel}<a class="btn tonal small" href="/dashboard/billing?site={id}#plans">Change plan</a>"#,
                    manage = manage,
                    cancel = cancel,
                    id = view::attr(&tenant.id),
                ),
            )));
        }
    }

    // ---- the plan chooser -------------------------------------------------
    let plans = live.purchasable_plans();
    if !plans.is_empty() && !tenants.is_empty() {
        // Which site a plan change applies to. Pre-selected from the link the
        // customer followed, so the common path is one click.
        let selected_site = query
            .get("site")
            .and_then(|id| tenants.iter().find(|t| &t.id == id))
            .or_else(|| tenants.first());

        let site_options: String = tenants
            .iter()
            .map(|t| {
                format!(
                    r#"<option value="{}"{}>{}</option>"#,
                    view::attr(&t.id),
                    view::flag(
                        "selected",
                        selected_site.map(|s| s.id == t.id).unwrap_or(false)
                    ),
                    esc(&t.name)
                )
            })
            .collect();

        let current_plan = selected_site.map(|t| t.plan_slug.clone()).unwrap_or_default();

        let cards: String = plans
            .iter()
            .map(|plan| {
                let is_current = plan.slug == current_plan;
                let features: String =
                    plan.features().iter().map(|f| format!("<li>{}</li>", esc(f))).collect();
                format!(
                    r#"<div class="plan{on}">
<h3>{name}</h3>
<div class="price">{price}<span> / {interval}</span></div>
<p class="desc">{description}</p>
<ul>{features}</ul>
<div class="spacer"></div>
{button}</div>"#,
                    on = if is_current { " on" } else { "" },
                    name = esc(&plan.name),
                    price = esc(&plan.price_display()),
                    interval = esc(&plan.interval),
                    description = esc(&plan.description),
                    features = features,
                    button = if is_current {
                        format!(
                            r#"<button class="btn tonal" disabled>{} Current plan</button>"#,
                            view::svg("check")
                        )
                    } else {
                        format!(
                            r#"<button class="btn" type="submit" name="plan" value="{}">Choose {}</button>"#,
                            view::attr(&plan.slug),
                            esc(&plan.name)
                        )
                    },
                )
            })
            .collect();

        body.push_str(&format!(
            r#"<p class="section" id="plans">Plans</p>
<form method="post" action="/dashboard/billing/change">
<div class="card padded" style="margin-bottom:16px">{site}</div>
<div class="plans">{cards}</div>
</form>"#,
            site = view::select(
                "tenant_id",
                "Apply to",
                &site_options,
                Some("Choosing a plan below applies it to this site."),
                false
            ),
            cards = cards,
        ));
    }

    Ok(shell(
        state,
        theme,
        "Billing",
        Tab::Billing,
        &account,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

#[derive(Deserialize)]
pub struct ChangePlanForm {
    pub tenant_id: String,
    pub plan: String,
}

/// Start a plan change.
///
/// Where this goes depends on the platform's configuration, and the three
/// cases are genuinely different:
///
/// * **no provider** — the plan is applied directly; nothing is charged.
/// * **a downgrade or an upgrade with a live subscription** — the customer is
///   sent to the provider to change what they pay, and the plan is applied
///   when the webhook confirms it. Applying it first would hand out a bigger
///   plan before being paid for it.
/// * **no live subscription** — a fresh checkout.
pub async fn change_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ChangePlanForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect(
        "/dashboard/billing",
        &chrome,
        change_plan_inner(&state, &headers, form).await,
    )
}

async fn change_plan_inner(
    state: &AppState,
    headers: &HeaderMap,
    form: ChangePlanForm,
) -> Result<Response> {
    auth::require_same_origin(headers, &state.config.host())?;
    let account = auth::account(state, headers).await?;
    let tenant = auth::owned_tenant(state, &account, &form.tenant_id).await?;
    let live = state.live();

    let plan = live
        .config()
        .plan(form.plan.trim())
        .ok_or_else(|| ProError::BadRequest("That plan is not available.".into()))?
        .clone();

    if plan.slug == tenant.plan_slug {
        return Ok(redirect_with(
            "/dashboard/billing",
            Flash::ok("That is already your plan."),
        ));
    }

    // Downgrading below what a site already uses would leave it over its new
    // ceiling the moment it restarted. Ghost itself handles being over a limit
    // gracefully — it refuses *new* members rather than deleting any — so this
    // is a warning rather than a refusal, but the customer should know.
    let usage = pro_db::usage::latest(&state.db, &tenant.id).await.unwrap_or(None);
    let over = match (plan.max_members, usage.as_ref().map(|u| u.members)) {
        (Some(max), Some(have)) if have > max => Some((have, max)),
        _ => None,
    };

    let provider_name = live.billing_provider();
    if provider_name == "none" {
        pro_provision::apply_plan(&state.db, &state.config, &state.supervisor, &tenant, &plan)
            .await?;
        pro_db::audit::record(
            &state.db,
            &format!("account:{}", account.id),
            "tenant.plan_changed",
            Some(&tenant.id),
            Some(&account.id),
            serde_json::json!({"to": plan.slug, "billing": "none"}),
        )
        .await;
        let message = match over {
            Some((have, max)) => format!(
                "Moved to {}. You have {have} members and the new plan includes {max} — \
                 existing members keep working, but you cannot add more until you upgrade.",
                plan.name
            ),
            None => format!("Moved to {}.", plan.name),
        };
        return Ok(redirect_with("/dashboard/billing", Flash::ok(message)));
    }

    let provider = pro_billing::require(&live)?;

    // End the subscription this site already has before starting another,
    // or a plan change becomes two live subscriptions and a double bill. The
    // old one is asked to run to its period end, so a customer who abandons
    // the new checkout keeps what they have already paid for rather than
    // losing the site mid-change.
    if let Some(existing) = pro_db::subscriptions::for_tenant(&state.db, &tenant.id).await? {
        let live_sub = matches!(existing.status.as_str(), "active" | "trialing" | "past_due");
        if live_sub
            && existing.provider == provider.name()
            && existing.provider_subscription_id.is_some()
            && !existing.cancel_at_period_end
        {
            provider.cancel_subscription(&existing).await?;
            pro_db::audit::record(
                &state.db,
                &format!("account:{}", account.id),
                "subscription.cancelled_for_plan_change",
                Some(&tenant.id),
                Some(&account.id),
                serde_json::json!({
                    "from": tenant.plan_slug,
                    "to": plan.slug,
                    "provider": existing.provider,
                }),
            )
            .await;
        }
    }

    let success = state.url(&format!(
        "/dashboard/billing?ok={}",
        view::urlencode("Thank you — your plan is being updated.")
    ));
    let cancel = state.url("/dashboard/billing");

    let checkout = provider
        .create_checkout(pro_billing::CheckoutRequest {
            account_id: &account.id,
            account_email: &account.email,
            tenant_id: Some(&tenant.id),
            plan: &plan,
            success_url: &success,
            cancel_url: &cancel,
            trial_days: if live.require_payment_up_front() { 0 } else { plan.trial_days },
        })
        .await?;

    pro_db::audit::record(
        &state.db,
        &format!("account:{}", account.id),
        "checkout.started",
        Some(&tenant.id),
        Some(&account.id),
        serde_json::json!({"plan": plan.slug, "provider": provider.name()}),
    )
    .await;

    Ok(reply::redirect(&checkout.url))
}

/// Send the customer to their provider's own management page.
pub async fn manage_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(subscription_id): axum::extract::Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/dashboard/billing", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let subscription = pro_db::subscriptions::get(&state.db, &subscription_id).await?;

        // A subscription id in a URL must not let one customer open another's
        // billing portal.
        if subscription.account_id != account.id {
            return Err(ProError::NotFound("No such subscription.".into()));
        }

        let live = state.live();
        // Built from the subscription's *own* provider, not the platform's
        // current one — that is the whole reason a provider switch does not
        // break existing customers.
        let provider = pro_billing::build(&subscription.provider, &live).ok_or_else(|| {
            ProError::Unavailable(
                "This subscription's payment provider is not currently configured. \
                 Please contact support."
                    .into(),
            )
        })?;

        match provider.management_url(&subscription).await? {
            Some(url) => Ok(reply::redirect(&url)),
            None => Err(ProError::Unavailable(
                "That provider has no self-service page. Please contact support.".into(),
            )),
        }
    }.await)
}

/// Cancel the subscription this account holds for one of its sites.
pub async fn cancel_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(subscription_id): axum::extract::Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/dashboard/billing", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let subscription = pro_db::subscriptions::get(&state.db, &subscription_id).await?;
        if subscription.account_id != account.id {
            return Err(ProError::NotFound("No such subscription.".into()));
        }
        if subscription.provider == "manual" {
            return Err(ProError::BadRequest(
                "This subscription was granted by the platform. Contact support to end it.".into(),
            ));
        }

        // The subscription's own provider, for the same reason as the manage
        // link: switching providers must not orphan the customers of the old
        // one.
        let live = state.live();
        let provider = pro_billing::build(&subscription.provider, &live).ok_or_else(|| {
            ProError::Unavailable(
                "This subscription's payment provider is not currently configured. \
                 Please contact support."
                    .into(),
            )
        })?;

        let at_period_end = provider.cancel_subscription(&subscription).await?;

        // The provider's webhook will state the end when it lands; this is
        // the platform's own record of what was asked for, so the page can
        // say "Ending" without waiting for it.
        pro_db::subscriptions::upsert(
            &state.db,
            pro_db::subscriptions::Upsert {
                account_id: &subscription.account_id,
                tenant_id: subscription.tenant_id.as_deref(),
                plan_slug: Some(&subscription.plan_slug),
                provider: &subscription.provider,
                provider_subscription_id: subscription.provider_subscription_id.as_deref(),
                provider_customer_id: subscription.provider_customer_id.as_deref(),
                status: Some(&subscription.status),
                current_period_start: subscription.current_period_start.as_deref(),
                current_period_end: subscription.current_period_end.as_deref(),
                trial_ends_at: subscription.trial_ends_at.as_deref(),
                cancel_at_period_end: Some(at_period_end),
                canceled_at: (!at_period_end).then(pro_core::now_iso).as_deref(),
            },
        )
        .await?;

        pro_db::audit::record(
            &state.db,
            &format!("account:{}", account.id),
            "subscription.cancelled",
            subscription.tenant_id.as_deref(),
            Some(&account.id),
            serde_json::json!({
                "provider": subscription.provider,
                "at_period_end": at_period_end,
            }),
        )
        .await;

        let message = if at_period_end {
            format!(
                "Your subscription will end {}.",
                view::when(subscription.current_period_end.as_deref())
            )
        } else {
            "Your subscription has been cancelled.".to_string()
        };
        Ok(redirect_with("/dashboard/billing", Flash::ok(message)))
    }.await)
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

pub async fn account_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/dashboard/signin",
        &chrome,
        account_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn account_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let account = auth::account(state, headers).await?;
    let sessions =
        pro_db::sessions::list_for(&state.db, pro_db::sessions::Subject::Account, &account.id)
            .await?;

    let session_rows: String = sessions
        .iter()
        .map(|s| {
            format!(
                r#"<tr><td class="wrap">{ua}</td><td class="mono">{ip}</td>
<td class="muted">{seen}</td><td class="end">{revoke}</td></tr>"#,
                ua = esc(s.user_agent.as_deref().unwrap_or("Unknown device")),
                ip = esc(s.ip.as_deref().unwrap_or("—")),
                seen = esc(&view::when(s.last_seen_at.as_deref())),
                revoke = view::action_button(
                    &format!("/dashboard/account/sessions/{}/revoke", s.id),
                    "Sign out",
                    "text small",
                    None
                ),
            )
        })
        .collect();

    let unverified = state.live().require_email_verification() && !account.is_verified();

    let body = format!(
        r#"{head}
{notice}
{section_you}
<div class="card padded">{details}</div>
{section_email}
<div class="card padded">{email_card}</div>
{section_password}
<div class="card padded">{password}</div>
{section_devices}
{sessions}
<p class="footnote">These are sessions for your {platform} account. Your sites have
their own separate logins.</p>"#,
        head = view::page_head("Account", &esc(&account.email), ""),
        notice = if unverified {
            crate::public::verify_notice(&account.email)
        } else {
            String::new()
        },
        section_you = view::section("Your details"),
        section_email = view::section("Change email"),
        section_password = view::section("Change password"),
        section_devices = view::section("Where you are signed in"),
        platform = esc(&state.platform_name()),
        details = view::form(
            "/dashboard/account",
            &format!(
                r#"{name}<button class="btn" type="submit">Save</button>"#,
                name = view::field("name", "Name", "text", &account.name, None, true),
            )
        ),
        email_card = view::form(
            "/dashboard/account/email",
            &format!(
                r#"{email}{password}<button class="btn" type="submit">Change email</button>"#,
                email = view::field(
                    "email",
                    "New email",
                    "email",
                    &account.email,
                    Some("The new address must be confirmed before it counts."),
                    true
                ),
                password = view::field(
                    "current_password",
                    "Current password",
                    "password",
                    "",
                    None,
                    true
                ),
            )
        ),
        password = view::form(
            "/dashboard/account/password",
            &format!(
                r#"{current}{new}<button class="btn" type="submit">Change password</button>"#,
                current = view::field("current", "Current password", "password", "", None, true),
                new = view::field(
                    "new_password",
                    "New password",
                    "password",
                    "",
                    Some("At least 10 characters. This signs you out everywhere else."),
                    true
                ),
            )
        ),
        sessions = view::table(&["Device", "Address", "Last seen", "end:"], &session_rows),
    );

    Ok(shell(
        state,
        theme,
        "Account",
        Tab::Account,
        &account,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

#[derive(Deserialize)]
pub struct AccountForm {
    pub name: String,
}

pub async fn update_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AccountForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/dashboard/account", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let name = form.name.trim();
        if name.is_empty() {
            return Err(ProError::BadRequest("Enter your name.".into()));
        }
        pro_db::accounts::rename(&state.db, &account.id, name).await?;
        Ok(redirect_with("/dashboard/account", Flash::ok("Saved.")))
    }.await)
}

#[derive(Deserialize)]
pub struct EmailForm {
    pub email: String,
    pub current_password: String,
}

pub async fn change_email(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<EmailForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/dashboard/account", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;

        let email = form.email.trim().to_lowercase();
        if email == account.email {
            return Ok(redirect_with("/dashboard/account", Flash::ok("Nothing to change.")));
        }
        if !pro_core::looks_like_email(&email) {
            return Err(ProError::BadRequest("That does not look like an email address.".into()));
        }
        // Password, not just the session: a stolen cookie must not be enough
        // to move somebody's account to an address they control.
        if !pro_db::accounts::verify_password(&form.current_password, &account.password_hash) {
            return Err(ProError::BadRequest("Your current password is not correct.".into()));
        }

        pro_db::accounts::set_email(&state.db, &account.id, &email).await?;

        // Prove the new address before it is trusted. Same token, same gate,
        // as signup — and if mail is log-only, the link is in the log for
        // whoever is running the platform.
        let live = state.live();
        let token = pro_db::tokens::issue(
            &state.db,
            pro_db::tokens::VERIFY_EMAIL,
            "account",
            &account.id,
            serde_json::json!({}),
        )
        .await?;
        let link = state.url(&format!("/verify?token={}", view::urlencode(&token)));
        let mailer = state.mailer();
        let _ = mailer
            .send(pro_mail::templates::verify_email(
                &live.platform_name(),
                &email,
                &account.name,
                &link,
            ))
            .await;
        if !mailer.is_live() {
            tracing::warn!(
                "mail is not configured, so {} cannot receive their new confirmation link. \
                 It is: {link}",
                email
            );
        }

        pro_db::audit::record(
            &state.db,
            &format!("account:{}", account.id),
            "account.email_changed",
            None,
            Some(&account.id),
            serde_json::json!({"to": email}),
        )
        .await;

        Ok(redirect_with(
            "/dashboard/account",
            Flash::ok("Email updated. Check the new address to confirm it."),
        ))
    }.await)
}

#[derive(Deserialize)]
pub struct PasswordForm {
    pub current: String,
    pub new_password: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/dashboard/account", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;

        // The current password is required even though the session proves
        // identity: a session is what an attacker with a stolen cookie has,
        // and without this they could lock the owner out by changing it.
        if !pro_db::accounts::verify_password(&form.current, &account.password_hash) {
            return Err(ProError::BadRequest("Your current password is not correct.".into()));
        }
        if form.new_password.len() < 10 {
            return Err(ProError::BadRequest(
                "Choose a password of at least 10 characters.".into(),
            ));
        }

        let hash = pro_db::accounts::hash_password(&form.new_password)?;
        pro_db::accounts::set_password(&state.db, &account.id, &hash).await?;
        // Every other device is signed out, which is the point of a password
        // change.
        pro_db::sessions::destroy_all(
            &state.db,
            pro_db::sessions::Subject::Account,
            &account.id,
        )
        .await?;

        pro_db::audit::record(
            &state.db,
            &format!("account:{}", account.id),
            "account.password_changed",
            None,
            Some(&account.id),
            serde_json::json!({}),
        )
        .await;

        Ok(redirect_with(
            "/dashboard/signin",
            Flash::ok("Password changed. Please sign in again."),
        ))
    }.await)
}

pub async fn revoke_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/dashboard/account", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        pro_db::sessions::revoke(
            &state.db,
            pro_db::sessions::Subject::Account,
            &account.id,
            &session_id,
        )
        .await?;
        Ok(redirect_with("/dashboard/account", Flash::ok("Signed out of that device.")))
    }.await)
}
