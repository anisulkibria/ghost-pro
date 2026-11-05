//! The pages anybody can reach: pricing, signup, sign-in, verification.
//!
//! Two shapes, both from `crate::view`'s Material 3 system: the sales page is
//! a top bar over a hero and a row of filled plan cards, and everything that
//! asks for a password is the centred `auth` surface — one card floating over
//! three blurred tonal blobs, which is where Ghost's own sign-in screen puts
//! its form too.
use axum::extract::{Form, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use pro_core::{ProError, Result};
use pro_db::sessions::{self, Subject};
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth;
use crate::reply::{self, redirect_with, redirect_with_cookie};
use crate::state::AppState;
use crate::view::{self, esc, BannerKind, Flash, NavItem, Page, Theme};

/// The top bar's links. The last one is the call to action, which `active`
/// is what marks on a public page.
fn nav() -> Vec<NavItem> {
    vec![
        NavItem::new("Pricing", "/pricing", "billing", "green", false),
        NavItem::new("Sign in", "/dashboard/signin", "person", "blue", false),
        NavItem::new("Get started", "/signup", "add", "pink", true),
    ]
}

/// The sales page's shell.
fn shell(state: &AppState, theme: Theme, title: &str, flash: Option<&Flash>, body: &str) -> Response {
    let platform = state.platform_name();
    let nav = nav();
    reply::html(view::public_page(Page {
        title,
        platform: &platform,
        tag: None,
        brand_href: "/",
        nav: &nav,
        who: None,
        flash,
        theme,
        body,
    }))
}

/// A centred page that asks for something.
fn gate(
    state: &AppState,
    theme: Theme,
    title: &str,
    heading: &str,
    sub: &str,
    flash: Option<&Flash>,
    card: &str,
    foot: &str,
) -> Response {
    reply::html(view::auth_page(
        title,
        &state.platform_name(),
        theme,
        heading,
        sub,
        flash,
        card,
        foot,
    ))
}

// ---------------------------------------------------------------------------
// Appearance
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AppearanceForm {
    pub theme: String,
    #[serde(default)]
    pub next: String,
}

/// Remember which colour scheme this visitor wants.
///
/// A POST and a cookie rather than a script: every page here is served under
/// `default-src 'none'` (see [`crate::reply`]), so there is nothing to run
/// `localStorage` or flip a class — the choice has to be known to the server
/// before the page is rendered. `system` is the one value it still cannot
/// resolve, and the stylesheet settles that one with a media query.
pub async fn appearance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AppearanceForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let theme = Theme::parse(&form.theme)
            .ok_or_else(|| ProError::BadRequest("Unknown appearance.".into()))?;

        // Back to the page the switcher was opened from — but only if that is
        // a path on this platform. An open redirect is an open redirect even
        // when the parameter looks harmless, and `//evil.example` is a
        // protocol-relative URL that a naive `starts_with('/')` would pass.
        let next = form.next.trim();
        let back = if next.starts_with('/') && !next.starts_with("//") {
            next
        } else {
            "/"
        };

        Ok(redirect_with_cookie(
            back,
            &auth::set_plain_cookie(view::THEME_COOKIE, theme.as_str(), state.secure_cookies()),
        ))
    }
    .await)
}

// ---------------------------------------------------------------------------
// The sales page
// ---------------------------------------------------------------------------

/// The front page.
///
/// Doubles as the pricing page, because a hosting platform's landing page and
/// its pricing page want to say the same thing.
pub async fn home(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    // Somebody already signed in wants their sites, not the sales page.
    if auth::account(&state, &headers).await.is_ok() {
        return reply::redirect("/dashboard");
    }
    pricing(State(state), headers, Query(query)).await
}

pub async fn pricing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let live = state.live();
    let plans = live.purchasable_plans();

    let cards: String = plans
        .iter()
        .enumerate()
        .map(|(i, plan)| {
            let features: String =
                plan.features().iter().map(|f| format!("<li>{}</li>", esc(f))).collect();
            let trial = if plan.trial_days > 0 && !live.require_payment_up_front() {
                format!(r#"<p class="trial">{} day free trial</p>"#, plan.trial_days)
            } else {
                String::new()
            };
            format!(
                r#"<div class="plan rise" style="--i:{i}">
<h3>{name}</h3>
<div class="price">{price}<span> / {interval}</span></div>
<p class="desc">{description}</p>
<ul>{features}</ul>
<div class="spacer"></div>
{trial}
<a class="btn" href="/signup?plan={slug}">Get started</a></div>"#,
                i = i,
                name = esc(&plan.name),
                price = esc(&plan.price_display()),
                interval = esc(&plan.interval),
                description = esc(&plan.description),
                features = features,
                trial = trial,
                slug = view::urlencode(&plan.slug),
            )
        })
        .collect();

    let body = format!(
        r#"<div class="hero">
<h1>Managed Ghost hosting</h1>
<p>A publication on your own domain, with HTTPS, backups and updates handled.
Installed in seconds.</p>
</div>
<div class="plans">{cards}</div>
<div class="feature-grid">
<div class="feature"><span class="ic tint-blue">{globe}</span><div>
<h3>Your own domain</h3><p>Point your DNS here and the certificate is issued and
renewed automatically — nothing to install, nothing to remember.</p></div></div>
<div class="feature"><span class="ic tint-green">{archive}</span><div>
<h3>Automatic backups</h3><p>A full nightly snapshot of your database, uploads and
theme, downloadable from your dashboard at any time.</p></div></div>
<div class="feature"><span class="ic tint-purple">{shield}</span><div>
<h3>Nothing to maintain</h3><p>No servers to patch, no Ghost upgrades to run, no
certificate renewals to miss.</p></div></div>
</div>
<p class="footnote" style="text-align:center">Already have an account?
<a href="/dashboard/signin">Sign in</a>.</p>"#,
        cards = cards,
        globe = view::svg("globe"),
        archive = view::svg("archive"),
        shield = view::svg("shield"),
    );

    shell(
        &state,
        auth::theme(&headers),
        "Pricing",
        Flash::from_query(&query).as_ref(),
        &body,
    )
}

// ---------------------------------------------------------------------------
// Signup
// ---------------------------------------------------------------------------

pub async fn signup_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let live = state.live();
    let theme = auth::theme(&headers);

    if !live.signup_enabled() {
        return gate(
            &state,
            theme,
            "Signups closed",
            "Signups are closed",
            "New accounts are not being created at the moment.",
            None,
            r#"<p class="hint" style="margin:0 0 16px">Please check back later — or sign in if you
already have an account.</p>
<a class="btn tonal block" href="/dashboard/signin">Sign in</a>"#,
            "",
        );
    }

    let chosen = query.get("plan").cloned().unwrap_or_default();
    let plans = live.purchasable_plans();
    let sub = plans
        .iter()
        .find(|p| p.slug == chosen)
        .map(|p| {
            format!(
                "Starting on {} — {} / {}{}.",
                p.name,
                p.price_display(),
                p.interval,
                if p.trial_days > 0 && !live.require_payment_up_front() {
                    format!(", after a {} day free trial", p.trial_days)
                } else {
                    String::new()
                }
            )
        })
        .unwrap_or_else(|| "Choose a plan when you create your first site.".into());

    let card = view::form(
        "/signup",
        &format!(
            r#"{name}{email}{password}
<input type="hidden" name="plan" value="{plan}">
<button class="btn" type="submit">Create account</button>"#,
            name = view::field("name", "Your name", "text", "", None, true),
            email = view::field("email", "Email", "email", "", None, true),
            password = view::field(
                "password",
                "Password",
                "password",
                "",
                Some("At least 10 characters."),
                true
            ),
            plan = view::attr(&chosen),
        ),
    );

    gate(
        &state,
        theme,
        "Create your account",
        "Create your account",
        &sub,
        Flash::from_query(&query).as_ref(),
        &card,
        r#"<p class="auth-foot">Already have an account? <a href="/dashboard/signin">Sign in</a></p>"#,
    )
}

#[derive(Deserialize)]
pub struct SignupForm {
    pub name: String,
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub plan: String,
}

pub async fn signup(
    State(state): State<AppState>,
    headers: HeaderMap,
    client: crate::client_ip::ClientAddr,
    Form(form): Form<SignupForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = if form.plan.is_empty() {
        "/signup".to_string()
    } else {
        format!("/signup?plan={}", view::urlencode(&form.plan))
    };
    reply::or_redirect(&back, &chrome, signup_inner(&state, &headers, client, form).await)
}

async fn signup_inner(
    state: &AppState,
    headers: &HeaderMap,
    client: crate::client_ip::ClientAddr,
    form: SignupForm,
) -> Result<Response> {
    auth::require_same_origin(headers, &state.config.host())?;
    let live = state.live();
    if !live.signup_enabled() {
        return Err(ProError::Forbidden("Signups are closed at the moment.".into()));
    }

    let name = form.name.trim();
    let email = form.email.trim().to_lowercase();
    if name.is_empty() {
        return Err(ProError::BadRequest("Enter your name.".into()));
    }
    if !pro_core::looks_like_email(&email) {
        return Err(ProError::BadRequest("That does not look like an email address.".into()));
    }
    if form.password.len() < 10 {
        return Err(ProError::BadRequest(
            "Choose a password of at least 10 characters.".into(),
        ));
    }

    let hash = pro_db::accounts::hash_password(&form.password)?;
    let account = pro_db::accounts::create(&state.db, &email, name, &hash).await?;

    pro_db::audit::record(
        &state.db,
        &format!("account:{}", account.id),
        "account.created",
        None,
        Some(&account.id),
        serde_json::json!({"plan_interest": form.plan}),
    )
    .await;

    // Sign them in straight away. The verification gate is on *provisioning*,
    // not on the dashboard: making somebody check their email before they can
    // even see where they landed is friction with nothing to show for it.
    let ip = client.ip_string();
    let user_agent = headers.get("user-agent").and_then(|v| v.to_str().ok());
    let token = sessions::create(
        &state.db,
        Subject::Account,
        &account.id,
        ip.as_deref(),
        user_agent,
    )
    .await?;

    let mailer = state.mailer();
    if live.require_email_verification() {
        let verify_token = pro_db::tokens::issue(
            &state.db,
            pro_db::tokens::VERIFY_EMAIL,
            "account",
            &account.id,
            serde_json::json!({}),
        )
        .await?;
        let link = state.url(&format!("/verify?token={}", view::urlencode(&verify_token)));
        let _ = mailer
            .send(pro_mail::templates::verify_email(
                &live.platform_name(),
                &account.email,
                &account.name,
                &link,
            ))
            .await;

        // On a platform with no working mail server, an operator testing the
        // flow would otherwise be stuck forever. The link is in the log.
        if !mailer.is_live() {
            tracing::warn!(
                "mail is not configured, so {} cannot receive their confirmation link. \
                 It is: {link}",
                account.email
            );
        }
    } else {
        pro_db::accounts::mark_verified(&state.db, &account.id).await?;
    }

    let cookie = auth::set_cookie(
        sessions::account_cookie_name(state.secure_cookies()),
        &token,
        state.secure_cookies(),
        sessions::ACCOUNT_TTL_DAYS,
    );

    let destination = if form.plan.is_empty() {
        "/dashboard".to_string()
    } else {
        format!("/dashboard/new?plan={}", view::urlencode(&form.plan))
    };
    Ok(redirect_with_cookie(&destination, &cookie))
}

// ---------------------------------------------------------------------------
// Sign in / out
// ---------------------------------------------------------------------------

pub async fn signin_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let card = view::form(
        "/dashboard/signin",
        &format!(
            r#"{email}{password}<button class="btn" type="submit">Sign in</button>"#,
            email = view::field("email", "Email", "email", "", None, true),
            password = view::field("password", "Password", "password", "", None, true),
        ),
    );
    gate(
        &state,
        auth::theme(&headers),
        "Sign in",
        "Welcome back",
        "Sign in to manage your sites.",
        Flash::from_query(&query).as_ref(),
        &card,
        r#"<p class="auth-foot"><a href="/forgot">Forgotten your password?</a> ·
<a href="/signup">Create an account</a></p>"#,
    )
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
    reply::or_redirect("/dashboard/signin", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account =
            pro_db::accounts::authenticate(&state.db, &form.email, &form.password).await?;
        let ip = client.ip_string();
        let user_agent = headers.get("user-agent").and_then(|v| v.to_str().ok());
        let token = sessions::create(
            &state.db,
            Subject::Account,
            &account.id,
            ip.as_deref(),
            user_agent,
        )
        .await?;
        let cookie = auth::set_cookie(
            sessions::account_cookie_name(state.secure_cookies()),
            &token,
            state.secure_cookies(),
            sessions::ACCOUNT_TTL_DAYS,
        );
        Ok(redirect_with_cookie("/dashboard", &cookie))
    }.await)
}

pub async fn signout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let name = sessions::account_cookie_name(state.secure_cookies());
    if let Some(token) = auth::cookie(&headers, name) {
        let _ = sessions::destroy(&state.db, &token).await;
    }
    redirect_with_cookie("/", &auth::clear_cookie(name, state.secure_cookies()))
}

// ---------------------------------------------------------------------------
// Email verification
// ---------------------------------------------------------------------------

pub async fn verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/dashboard", &chrome, async {
        let token = query
            .get("token")
            .map(String::as_str)
            .ok_or_else(|| ProError::BadRequest("That link is not valid.".into()))?;

        let claim =
            pro_db::tokens::consume(&state.db, pro_db::tokens::VERIFY_EMAIL, token).await?;
        pro_db::accounts::mark_verified(&state.db, &claim.subject_id).await?;

        pro_db::audit::record(
            &state.db,
            &format!("account:{}", claim.subject_id),
            "account.email_verified",
            None,
            Some(&claim.subject_id),
            serde_json::json!({}),
        )
        .await;

        Ok(redirect_with(
            "/dashboard",
            Flash::ok("Email confirmed. You can create your first site."),
        ))
    }.await)
}

pub async fn resend_verification(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/dashboard", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        if account.is_verified() {
            return Ok(redirect_with("/dashboard", Flash::ok("Your email is already confirmed.")));
        }

        // Reissue, which invalidates the previous link: several live
        // confirmation links per account is several independent chances of one
        // being intercepted, and only the newest is ever used.
        let token = pro_db::tokens::reissue(
            &state.db,
            pro_db::tokens::VERIFY_EMAIL,
            "account",
            &account.id,
            serde_json::json!({}),
        )
        .await?;
        let link = state.url(&format!("/verify?token={}", view::urlencode(&token)));
        let live = state.live();
        let mailer = state.mailer();
        if !mailer.is_live() {
            tracing::warn!("mail is not configured; confirmation link for {}: {link}", account.email);
        }
        let _ = mailer
            .send(pro_mail::templates::verify_email(
                &live.platform_name(),
                &account.email,
                &account.name,
                &link,
            ))
            .await;

        Ok(redirect_with(
            "/dashboard",
            Flash::ok(format!("Confirmation link sent to {}.", account.email)),
        ))
    }.await)
}

// ---------------------------------------------------------------------------
// Password reset
// ---------------------------------------------------------------------------

pub async fn forgot_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let card = view::form(
        "/forgot",
        &format!(
            r#"{email}<button class="btn" type="submit">Send reset link</button>"#,
            email = view::field("email", "Email", "email", "", None, true),
        ),
    );
    gate(
        &state,
        auth::theme(&headers),
        "Reset your password",
        "Reset your password",
        "We will email you a link.",
        Flash::from_query(&query).as_ref(),
        &card,
        r#"<p class="auth-foot"><a href="/dashboard/signin">Back to sign in</a></p>"#,
    )
}

#[derive(Deserialize)]
pub struct ForgotForm {
    pub email: String,
}

pub async fn forgot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ForgotForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/forgot", &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let live = state.live();

        // The same answer whether or not the address is registered. Saying
        // "no such account" here would turn this form into a way to test
        // whether somebody is a customer.
        if let Ok(Some(account)) = pro_db::accounts::find_by_email(&state.db, &form.email).await {
            let token = pro_db::tokens::reissue(
                &state.db,
                pro_db::tokens::RESET_PASSWORD,
                "account",
                &account.id,
                serde_json::json!({}),
            )
            .await?;
            let link = state.url(&format!("/reset?token={}", view::urlencode(&token)));
            let mailer = state.mailer();
            if !mailer.is_live() {
                tracing::warn!("mail is not configured; reset link for {}: {link}", account.email);
            }
            let _ = mailer
                .send(pro_mail::templates::reset_password(
                    &live.platform_name(),
                    &account.email,
                    &account.name,
                    &link,
                ))
                .await;
        }

        Ok(redirect_with(
            "/dashboard/signin",
            Flash::ok("If that address has an account, a reset link is on its way."),
        ))
    }.await)
}

pub async fn reset_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let token = query.get("token").cloned().unwrap_or_default();
    let card = view::form(
        "/reset",
        &format!(
            r#"<input type="hidden" name="token" value="{token}">
{password}<button class="btn" type="submit">Set password</button>"#,
            token = view::attr(&token),
            password = view::field(
                "password",
                "New password",
                "password",
                "",
                Some("At least 10 characters."),
                true
            ),
        ),
    );
    gate(
        &state,
        auth::theme(&headers),
        "Choose a new password",
        "Choose a new password",
        "This signs you out everywhere else.",
        Flash::from_query(&query).as_ref(),
        &card,
        "",
    )
}

#[derive(Deserialize)]
pub struct ResetForm {
    pub token: String,
    pub password: String,
}

pub async fn reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ResetForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/reset?token={}", view::urlencode(&form.token));
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        if form.password.len() < 10 {
            return Err(ProError::BadRequest(
                "Choose a password of at least 10 characters.".into(),
            ));
        }

        let claim =
            pro_db::tokens::consume(&state.db, pro_db::tokens::RESET_PASSWORD, &form.token)
                .await?;
        let hash = pro_db::accounts::hash_password(&form.password)?;
        pro_db::accounts::set_password(&state.db, &claim.subject_id, &hash).await?;

        // Anybody signed in with the old password is signed out, which is the
        // whole point of a reset.
        pro_db::sessions::destroy_all(
            &state.db,
            pro_db::sessions::Subject::Account,
            &claim.subject_id,
        )
        .await?;

        pro_db::audit::record(
            &state.db,
            "system",
            "account.password_reset",
            None,
            Some(&claim.subject_id),
            serde_json::json!({}),
        )
        .await;

        Ok(redirect_with(
            "/dashboard/signin",
            Flash::ok("Password set. Please sign in."),
        ))
    }.await)
}

/// `GET /health` — for a load balancer or an uptime check.
///
/// Reports the database being reachable, because that is the dependency whose
/// failure makes the platform useless while the process still looks alive.
pub async fn health(State(state): State<AppState>) -> Response {
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.db)
        .await
        .is_ok();

    reply::json(
        if db_ok {
            axum::http::StatusCode::OK
        } else {
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        },
        serde_json::json!({
            "status": if db_ok { "ok" } else { "degraded" },
            "database": db_ok,
            "sites_running": state.supervisor.running_count(),
        }),
    )
}

/// The notice a customer who has not confirmed their address sees.
///
/// Lives here rather than in the dashboard because the resend endpoint it
/// posts to is this module's.
pub fn verify_notice(email: &str) -> String {
    view::banner(
        BannerKind::Warn,
        &format!(
            r#"<strong>Confirm your email.</strong> We sent a link to {email}. Confirm it to
create your first site. {resend}"#,
            email = esc(email),
            resend = view::action_button(
                "/dashboard/resend-verification",
                "Resend link",
                "tonal small",
                None
            ),
        ),
    )
}
