//! End-to-end tests for the two panels.
//!
//! The unit tests in `view` prove each component renders what it should. These
//! prove the *pages* do: that every screen a customer or an operator can reach
//! returns 200, draws the Material 3 shell, and carries the things that screen
//! exists to show. That is the coverage the redesign most needed — a page that
//! compiles but panics on a `None`, or quietly loses its nav rail, is exactly
//! the kind of break a `cargo build` cannot see.
//!
//! Everything runs against an in-memory database and a supervisor pointed at
//! `/bin/sh`, so no Ghost binary and no network are involved. The requests go
//! through the real router, so the host gate, the cookies and the
//! post-redirect-get flow are all genuinely exercised.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use pro_core::{Config, StaffRole};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "pro.localhost:8080";

/// A platform with one plan catalogue, in a temporary directory.
async fn platform() -> (pro_web::AppState, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    let mut config: Config = serde_json::from_value(serde_json::json!({
        "url": format!("http://{HOST}"),
        "subdomainBase": "localhost",
        // Every platform has /bin/sh, so the supervisor constructs without a
        // Ghost build present. Nothing in these tests starts a tenant.
        "paths": { "ghostBinary": "/bin/sh", "dataDir": dir.path().to_str().unwrap() },
        "ssl": { "enabled": false },
        "mail": { "transport": "log" },
        "billing": { "provider": "none" },
    }))
    .unwrap();
    config.plans = pro_core::plans::default_plans();
    let config = Arc::new(config);

    let db = pro_db::pool::open_in_memory().await.unwrap();
    let settings = Arc::new(pro_db::SettingsStore::load(&db, config.clone()).await.unwrap());
    let supervisor = Arc::new(pro_supervisor::Supervisor::new(config.clone()).unwrap());
    let hosts = Arc::new(pro_proxy::Router::new(
        config.host(),
        config.subdomain_base.clone(),
    ));

    let state = pro_web::AppState {
        db,
        config,
        settings,
        supervisor,
        hosts,
        forwarder: pro_proxy::Forwarder::new(),
    };
    (state, dir)
}

/// One request through the real router, with the platform's own `Host` so the
/// host gate lets it reach a panel rather than proxying it to a tenant.
async fn send(
    state: &pro_web::AppState,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    form: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST)
        // Same-origin, which every state-changing POST is checked for.
        .header("origin", format!("http://{HOST}"));
    if let Some(c) = cookie {
        builder = builder.header(header::COOKIE, c);
    }
    let request = match form {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };

    let response = pro_web::entry(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024).await.unwrap();
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
}

async fn get(state: &pro_web::AppState, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let (status, _, body) = send(state, "GET", path, cookie, None).await;
    (status, body)
}

/// The `name=value` pair out of a `Set-Cookie`, ready to send back.
fn session_from(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .expect("a sign-in must set a session cookie")
        .to_string()
}

/// Sign a customer in through the real form, and hand back their cookie.
async fn as_customer(state: &pro_web::AppState) -> (pro_core::Account, String) {
    let hash = pro_db::accounts::hash_password("a-long-enough-password").unwrap();
    let account = pro_db::accounts::create(&state.db, "owner@example.com", "Ada Lovelace", &hash)
        .await
        .unwrap();
    pro_db::accounts::mark_verified(&state.db, &account.id).await.unwrap();

    let (status, headers, _) = send(
        state,
        "POST",
        "/dashboard/signin",
        None,
        Some("email=owner@example.com&password=a-long-enough-password"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "a good password signs in");
    (account, session_from(&headers))
}

/// Sign an operator in through the real form, and hand back their cookie.
async fn as_operator(state: &pro_web::AppState, role: StaffRole) -> String {
    pro_db::staff::create(
        &state.db,
        "ops@example.com",
        "Grace Hopper",
        "a-very-long-operator-password",
        role,
    )
    .await
    .unwrap();

    let (status, headers, _) = send(
        state,
        "POST",
        "/console/signin",
        None,
        Some("email=ops@example.com&password=a-very-long-operator-password"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "a good password signs in");
    session_from(&headers)
}

/// A site on this account, without running a Ghost process for it.
async fn seed_site(state: &pro_web::AppState, account_id: &str, slug: &str) -> pro_core::Tenant {
    let tenant = pro_db::tenants::create(
        &state.db,
        account_id,
        slug,
        "Ada's Journal",
        "starter",
        &pro_core::now_iso(),
        21_000,
        28_999,
    )
    .await
    .unwrap();
    pro_db::tenants::set_status(&state.db, &tenant.id, pro_core::TenantStatus::Active)
        .await
        .unwrap();
    pro_db::domains::add(
        &state.db,
        &tenant.id,
        &format!("{slug}.localhost"),
        pro_core::DomainKind::Subdomain,
        true,
    )
    .await
    .unwrap();
    pro_db::tenants::get(&state.db, &tenant.id).await.unwrap()
}

/// What makes a page the Material 3 shell rather than a bare document.
fn assert_m3_shell(body: &str, where_: &str) {
    assert!(body.contains(r#"<aside class="rail">"#), "{where_}: no nav rail");
    assert!(body.contains("--primary: #984059"), "{where_}: no tonal palette");
    assert!(body.contains(r#"class="page-head"#), "{where_}: no app bar");
    // The whole design system is script-free on purpose: these pages are
    // served under `default-src 'none'`, which would block one anyway.
    assert!(!body.to_lowercase().contains("<script"), "{where_}: a script crept in");
    // And the appearance switcher is there to switch with.
    assert!(body.contains(r#"action="/appearance""#), "{where_}: no appearance menu");
}

// ---------------------------------------------------------------------------
// The customer dashboard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_dashboard_page_renders() {
    let (state, _dir) = platform().await;
    let (account, cookie) = as_customer(&state).await;
    let tenant = seed_site(&state, &account.id, "adas-journal").await;

    for path in [
        "/dashboard",
        "/dashboard/new",
        "/dashboard/billing",
        "/dashboard/account",
        &format!("/dashboard/sites/{}", tenant.id),
    ] {
        let (status, body) = get(&state, path, Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK, "{path} did not render");
        assert_m3_shell(&body, path);
        // The rail's own entries, so a page cannot lose its navigation.
        assert!(body.contains(r#"href="/dashboard/billing""#), "{path}: no Billing link");
        assert!(body.contains(r#"href="/dashboard/account""#), "{path}: no Account link");
    }
}

/// The sites list is the dashboard's front door, and it has two completely
/// different shapes depending on whether there is anything in it.
#[tokio::test]
async fn the_sites_list_has_both_of_its_shapes() {
    let (state, _dir) = platform().await;
    let (account, cookie) = as_customer(&state).await;

    let (status, empty) = get(&state, "/dashboard", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(empty.contains("Install your first site"), "the empty state");
    assert!(empty.contains("Welcome, Ada"), "greeted by first name");
    assert!(!empty.contains("<table"), "nothing to tabulate yet");

    seed_site(&state, &account.id, "adas-journal").await;
    let (status, filled) = get(&state, "/dashboard", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(filled.contains("Ada&#39;s Journal"), "the site's name, escaped");
    assert!(filled.contains("adas-journal.localhost"), "its address");
    assert!(filled.contains(r#"class="tbl""#), "as a table");
    assert!(filled.contains("1 site"), "counted in the singular");
    assert!(!filled.contains("Install your first site"));
}

/// A site page has to survive a site with no usage sample, no custom domain
/// and no subscription — which is every site for its first day.
#[tokio::test]
async fn a_brand_new_site_page_renders() {
    let (state, _dir) = platform().await;
    let (account, cookie) = as_customer(&state).await;
    let tenant = seed_site(&state, &account.id, "fresh").await;

    let (status, body) = get(&state, &format!("/dashboard/sites/{}", tenant.id), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_m3_shell(&body, "site page");
    // The meters fall back to "unlimited"/"of N" rather than dividing by a
    // missing sample.
    assert!(body.contains(r#"class="tile""#), "the usage tiles");
    assert!(body.contains("Usage is measured daily"), "says why the figures are empty");
    assert!(body.contains("no subscription"), "and that nothing is paying for it yet");
    assert!(body.contains("Create backup"));
}

/// Another customer's site id must be a 404, not somebody else's dashboard.
#[tokio::test]
async fn one_customer_cannot_open_anothers_site() {
    let (state, _dir) = platform().await;
    let (_, cookie) = as_customer(&state).await;

    let hash = pro_db::accounts::hash_password("another-long-password").unwrap();
    let other = pro_db::accounts::create(&state.db, "other@example.com", "Someone Else", &hash)
        .await
        .unwrap();
    let theirs = seed_site(&state, &other.id, "theirs").await;

    let (status, body) = get(&state, &format!("/dashboard/sites/{}", theirs.id), Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "404, so ids cannot be probed");
    assert!(!body.contains("Ada&#39;s Journal"));
}

#[tokio::test]
async fn a_signed_out_visitor_is_sent_to_sign_in() {
    let (state, _dir) = platform().await;
    for path in ["/dashboard", "/dashboard/billing", "/dashboard/account"] {
        let (status, _) = get(&state, path, None).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{path} must detour, not 401");
    }
    for path in ["/console", "/console/sites", "/console/settings"] {
        let (status, _) = get(&state, path, None).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{path} must detour, not 401");
    }
}

// ---------------------------------------------------------------------------
// The operator console
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_console_page_renders() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Owner).await;
    let hash = pro_db::accounts::hash_password("a-long-enough-password").unwrap();
    let account = pro_db::accounts::create(&state.db, "cust@example.com", "A Customer", &hash)
        .await
        .unwrap();
    let tenant = seed_site(&state, &account.id, "somebodys-site").await;

    for path in [
        "/console",
        "/console/sites",
        "/console/customers",
        "/console/settings",
        "/console/staff",
        "/console/activity",
        &format!("/console/sites/{}", tenant.id),
        &format!("/console/customers/{}", account.id),
    ] {
        let (status, body) = get(&state, path, Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK, "{path} did not render");
        assert_m3_shell(&body, path);
        // The tag that tells an operator which panel they are in. This is the
        // one piece of chrome that must never go missing: the console and the
        // dashboard otherwise look alike, and one of them has Suspend in it.
        assert!(body.contains(r#"class="tag">Console"#), "{path}: untagged as the console");
    }
}

/// The settings screen is rendered from the registry, so it has to show every
/// group — and a secret must never be echoed back into the form.
#[tokio::test]
async fn the_settings_screen_renders_the_whole_registry() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Owner).await;

    let (status, body) = get(&state, "/console/settings", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    for group in pro_core::settings::groups() {
        assert!(body.contains(group), "the {group} group is missing");
    }
    for def in pro_core::settings::REGISTRY {
        if matches!(def.kind, pro_core::settings::Kind::Managed) {
            // A map, edited on its own screen. It must *not* be a field here —
            // a text box containing JSON is how a whole catalogue gets wiped
            // by one stray keystroke.
            assert!(
                !body.contains(&format!(r#"name="{}""#, def.key)),
                "{} is managed and must not be a form field",
                def.key
            );
            continue;
        }
        assert!(
            body.contains(&format!(r#"name="{}""#, def.key)),
            "{} is not on the form",
            def.key
        );
    }
    // Booleans are M3 switches, each with its presence companion.
    assert!(body.contains(r#"class="switch""#));
    assert!(body.contains("__present."));
    // And the managed setting is still reachable, or it would be invisible.
    assert!(body.contains(r#"href="/console/plans""#), "no way to reach the Plans screen");
}

/// The screen that makes a platform able to charge.
///
/// A plan's product id at a provider is created in that provider's dashboard,
/// often after this platform is already running — so it has to be settable
/// without editing `pro.json` and restarting. This is that path end to end.
#[tokio::test]
async fn plan_product_ids_are_settable_without_a_restart() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Owner).await;

    // Lemon Squeezy selected, with credentials but no variant ids: the state a
    // platform is in on its first day.
    state
        .settings
        .save(
            &state.db,
            &[
                ("billing.lemonsqueezy.apiKey".into(), Some(serde_json::json!("ls_key"))),
                ("billing.lemonsqueezy.storeId".into(), Some(serde_json::json!("4242"))),
                ("billing.provider".into(), Some(serde_json::json!("lemonsqueezy"))),
            ],
            StaffRole::Owner,
            "test",
        )
        .await
        .unwrap();

    assert_eq!(
        state.live().purchasable_plans().len(),
        0,
        "nothing can be bought until a variant id is set"
    );

    let (status, body) = get(&state, "/console/plans", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_m3_shell(&body, "/console/plans");
    // It says plainly that the platform cannot sell, which is the whole point
    // of the screen existing.
    assert!(body.contains("Nothing can be bought yet"), "got no warning");
    // A field per plan, named for the plan and the provider, and labelled with
    // what Lemon Squeezy actually calls the thing.
    assert!(body.contains(r#"name="id.starter.lemonsqueezy""#), "got {body}");
    assert!(body.contains("Variant ID"));
    // And the endpoint the operator has to paste into Lemon Squeezy.
    assert!(body.contains("/webhooks/lemonsqueezy"));
    assert!(body.contains("subscription_payment_recovered"), "the events to enable");

    // Set one, the way the form posts it.
    let (status, _, _) = send(
        &state,
        "POST",
        "/console/plans",
        Some(&cookie),
        Some("id.starter.lemonsqueezy=812345&id.creator.lemonsqueezy="),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Live immediately — no restart, which is the requirement.
    let live = state.live();
    assert_eq!(
        live.plan("starter").unwrap().price_id("lemonsqueezy"),
        Some("812345"),
        "the id is in the live catalogue"
    );
    assert_eq!(live.purchasable_plans().len(), 1, "and that plan is now on sale");
    // The blank one was not invented.
    assert_eq!(live.plan("creator").unwrap().price_id("lemonsqueezy"), None);

    // The webhook path can now resolve a plan from that variant, which is
    // what makes a renewal apply the right limits.
    assert_eq!(
        pro_billing::plan_for_price(&live.plans(), "lemonsqueezy", "812345")
            .map(|p| p.slug.clone()),
        Some("starter".to_string())
    );

    // The screen now reflects it, and stops warning.
    let (_, body) = get(&state, "/console/plans", Some(&cookie)).await;
    assert!(body.contains(r#"value="812345""#));
    assert!(!body.contains("Nothing can be bought yet"));
    assert!(body.contains("On sale"));

    // Clearing it removes the id rather than storing a blank that masks the
    // config file.
    let (status, _, _) = send(
        &state,
        "POST",
        "/console/plans",
        Some(&cookie),
        Some("id.starter.lemonsqueezy="),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(state.live().plan("starter").unwrap().price_id("lemonsqueezy"), None);
}

/// Product ids are billing credentials in all but name, so they are the
/// owner's alone — and the check is the handler's, not the disabled attribute
/// the page happens to render.
#[tokio::test]
async fn only_an_owner_may_set_product_ids() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Admin).await;

    let (status, body) = get(&state, "/console/plans", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK, "an admin may look");
    assert!(body.contains("can only be changed by the Owner"), "and is told why not");

    let (status, _, _) = send(
        &state,
        "POST",
        "/console/plans",
        Some(&cookie),
        Some("id.starter.lemonsqueezy=812345"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "refused, back to the page");
    assert_eq!(
        state.live().plan("starter").unwrap().price_id("lemonsqueezy"),
        None,
        "nothing was written"
    );
}

/// A crafted field must not invent a plan or a provider.
#[tokio::test]
async fn the_plans_form_refuses_what_it_does_not_know() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Owner).await;

    for hostile in ["id.no-such-plan.lemonsqueezy=x", "id.starter.notaprovider=x"] {
        let (status, _, _) =
            send(&state, "POST", "/console/plans", Some(&cookie), Some(hostile)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{hostile}");
    }
    // And the catalogue is untouched.
    let live = state.live();
    assert!(live.plan("no-such-plan").is_none());
    assert_eq!(live.plan("starter").unwrap().price_id("notaprovider"), None);
}

/// Saving the settings form must round-trip through the console and change the
/// live settings without a restart — including switching a boolean *off*,
/// which is the case an absent checkbox makes dangerous.
#[tokio::test]
async fn settings_save_and_take_effect_immediately() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Owner).await;

    assert!(state.live().signup_enabled(), "signups are on by default");

    // The switch is off, so the browser sends only the companion.
    let (status, _, _) = send(
        &state,
        "POST",
        "/console/settings",
        Some(&cookie),
        Some("__present.signup.enabled=1"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(!state.live().signup_enabled(), "the switch turned signups off");

    // And on again, with the checkbox present.
    let (status, _, _) = send(
        &state,
        "POST",
        "/console/settings",
        Some(&cookie),
        Some("__present.signup.enabled=1&signup.enabled=1"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(state.live().signup_enabled());

    // A submission mentioning neither leaves it exactly as it was — this is
    // the guard against a partial POST reading as "every switch off".
    let (status, _, _) =
        send(&state, "POST", "/console/settings", Some(&cookie), Some("platform.name=Acme")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(state.live().signup_enabled(), "an unmentioned switch must not flip");
    assert_eq!(state.live().platform_name(), "Acme", "and the named field saved");
}

/// A support operator sees the console but may not act in it. The screens must
/// withhold the controls rather than offering buttons that refuse.
#[tokio::test]
async fn a_support_operator_is_shown_no_controls_they_cannot_use() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Support).await;
    let hash = pro_db::accounts::hash_password("a-long-enough-password").unwrap();
    let account = pro_db::accounts::create(&state.db, "cust@example.com", "A Customer", &hash)
        .await
        .unwrap();
    let tenant = seed_site(&state, &account.id, "somebodys-site").await;

    let (status, body) = get(&state, &format!("/console/sites/{}", tenant.id), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("/suspend"), "suspend is Admin and above");
    assert!(body.contains("Apply plan"), "the plan control is shown…");
    assert!(body.contains("disabled"), "…but locked");

    // And the handler refuses regardless of what the page offered.
    let (status, _, _) = send(
        &state,
        "POST",
        &format!("/console/sites/{}/suspend", tenant.id),
        Some(&cookie),
        Some("reason=because"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "refused, back to the page");
    let tenant = pro_db::tenants::get(&state.db, &tenant.id).await.unwrap();
    assert_eq!(tenant.status, pro_core::TenantStatus::Active, "still running");

    // Staff management is an owner's, so the invite form is not drawn.
    let (status, staff) = get(&state, "/console/staff", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!staff.contains("Add a staff login"));
}

/// A customer's session must not open the console, and an operator's must not
/// open the dashboard. They are different populations in different tables.
#[tokio::test]
async fn the_two_panels_do_not_share_sessions() {
    let (state, _dir) = platform().await;
    let (_, customer) = as_customer(&state).await;
    let operator = as_operator(&state, StaffRole::Owner).await;

    let (status, _) = get(&state, "/console", Some(&customer)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "a customer cookie is no operator");

    let (status, _) = get(&state, "/dashboard", Some(&operator)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "and the reverse");
}

// ---------------------------------------------------------------------------
// Public pages, appearance and the host gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_public_page_renders() {
    let (state, _dir) = platform().await;
    for path in ["/", "/pricing", "/signup", "/dashboard/signin", "/forgot", "/reset"] {
        let (status, body) = get(&state, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path} did not render");
        assert!(body.contains("--primary: #984059"), "{path}: no tonal palette");
        assert!(!body.to_lowercase().contains("<script"), "{path}: a script crept in");
    }

    // The sales page shows the catalogue.
    let (_, pricing) = get(&state, "/pricing", None).await;
    for plan in pro_core::plans::default_plans() {
        assert!(pricing.contains(&plan.name), "{} is not on the pricing page", plan.name);
    }
    assert!(pricing.contains(r#"class="plans""#));

    // The password screens are the centred auth surface, not the rail.
    let (_, signin) = get(&state, "/dashboard/signin", None).await;
    assert!(signin.contains(r#"<div class="auth">"#));
    assert!(!signin.contains(r#"<aside class="rail">"#));
}

/// A colour scheme has to be rendered into `<html>` server-side, because there
/// is no script to put it there — that is the whole reason it is a cookie.
#[tokio::test]
async fn the_appearance_choice_round_trips() {
    let (state, _dir) = platform().await;

    let (status, headers, _) =
        send(&state, "POST", "/appearance", None, Some("theme=dark&next=/pricing")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/pricing");
    let cookie = session_from(&headers);
    assert!(cookie.contains("pro_theme=dark"));

    let (_, body) = get(&state, "/pricing", Some(&cookie)).await;
    assert!(body.contains(r#"class="dark""#), "dark paints on the first frame");

    // `system` is the one choice the server cannot settle, so it defers to the
    // stylesheet's media query.
    let (_, headers, _) =
        send(&state, "POST", "/appearance", None, Some("theme=system&next=/pricing")).await;
    let (_, body) = get(&state, "/pricing", Some(&session_from(&headers))).await;
    assert!(body.contains(r#"class="system""#));
    assert!(body.contains("prefers-color-scheme: dark"));
}

/// `next` is a destination taken from a form, so it has to be a path on this
/// platform and nothing else.
#[tokio::test]
async fn the_appearance_switcher_is_not_an_open_redirect() {
    let (state, _dir) = platform().await;
    for hostile in [
        "https://evil.example/x",
        "//evil.example/x",
        "http://evil.example",
    ] {
        let (status, headers, _) = send(
            &state,
            "POST",
            "/appearance",
            None,
            Some(&format!("theme=dark&next={hostile}")),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            headers.get(header::LOCATION).unwrap(),
            "/",
            "{hostile} must not be followed"
        );
    }
}

/// Switching the scheme must not cost the page you were reading.
///
/// The rail and the top bar are rendered once per panel, so the form's hidden
/// `next` can only ever name that panel's root — it would throw a visitor off
/// a site's detail page, or out of a half-filled sign-in form. The `Referer`
/// is the page they were actually on, and under this platform's
/// `Referrer-Policy` only a same-origin request carries one with a path.
#[tokio::test]
async fn the_switcher_comes_back_to_the_page_it_was_used_on() {
    let (state, _dir) = platform().await;

    // A deep page, and one with a query string, which must survive intact.
    for (referer, expected) in [
        ("/dashboard/sites/abc", "/dashboard/sites/abc"),
        ("/console/customers?page=2", "/console/customers?page=2"),
        ("/dashboard/signin", "/dashboard/signin"),
        // A fragment is the browser's business, not the server's.
        ("/pricing#plans", "/pricing"),
    ] {
        let (status, headers, _) =
            appearance_from(&state, &format!("http://{HOST}{referer}"), "theme=dark&next=/").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            headers.get(header::LOCATION).unwrap(),
            expected,
            "a switch on {referer} must come back to it"
        );
    }
}

/// The `Referer` is a header, so it is attacker-controlled in exactly the way
/// the `next` field is — and it gets exactly the same treatment. A path only
/// comes back out of it when the header names this platform's own host.
#[tokio::test]
async fn a_referer_from_elsewhere_is_not_followed() {
    let (state, _dir) = platform().await;

    for hostile in [
        "https://evil.example/dashboard",
        "http://evil.example:8080/console",
        // A host that merely *starts* with ours is a different host.
        "http://pro.localhost.evil.example/console",
        "not-a-url",
    ] {
        let (status, headers, _) =
            appearance_from(&state, hostile, "theme=dark&next=/pricing").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        // Falls back to the form's own `next`, validated as it always was.
        assert_eq!(
            headers.get(header::LOCATION).unwrap(),
            "/pricing",
            "{hostile} must not be followed"
        );
    }

    // And a hostile `Referer` cannot smuggle a destination past the `next`
    // check either: both have to fail closed at once.
    let (_, headers, _) =
        appearance_from(&state, "https://evil.example/x", "theme=dark&next=//evil.example").await;
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/");
}

/// One POST to the switcher, carrying a `Referer`.
///
/// `send` deliberately sends none, which is what covers the fallback; this is
/// the same request with the header a browser would actually attach.
async fn appearance_from(
    state: &pro_web::AppState,
    referer: &str,
    form: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let request = Request::builder()
        .method("POST")
        .uri("/appearance")
        .header(header::HOST, HOST)
        .header("origin", format!("http://{HOST}"))
        .header(header::REFERER, referer)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap();
    let response = pro_web::entry(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
}

/// A signed-out visitor only ever sees the public pages and the centred auth
/// pages. A switcher missing from those is a switcher they do not have, which
/// is what made dark mode unreachable before signing in.
#[tokio::test]
async fn a_signed_out_visitor_can_reach_the_switcher_everywhere() {
    let (state, _dir) = platform().await;

    for path in ["/", "/pricing", "/signup", "/dashboard/signin", "/console/signin", "/forgot"] {
        let (status, body) = get(&state, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            body.contains(r#"<details class="theme-menu">"#),
            "{path} has no appearance switcher"
        );
        assert!(body.contains(r#"action="/appearance""#), "{path}");
    }

    // And the choice actually paints, on the auth surface as much as the
    // public one — the cookie is read the same way on every page.
    let (_, headers, _) =
        send(&state, "POST", "/appearance", None, Some("theme=dark&next=/")).await;
    let cookie = session_from(&headers);
    for path in ["/", "/dashboard/signin", "/signup"] {
        let (_, body) = get(&state, path, Some(&cookie)).await;
        assert!(
            body.contains(r#"<html lang="en" class="dark" data-theme="dark">"#),
            "{path} did not render dark"
        );
    }
}

/// The console must not be *addressable* from a hostname a customer controls:
/// that is enough to host a convincing copy of its sign-in page on a domain
/// their own visitors already trust.
#[tokio::test]
async fn a_customers_hostname_cannot_reach_the_console() {
    let (state, _dir) = platform().await;
    let request = Request::builder()
        .method("GET")
        .uri("/console/signin")
        .header(header::HOST, "a-customers-domain.com")
        .body(Body::empty())
        .unwrap();
    let response = pro_web::entry(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(!body.contains("Operator sign in"), "no console on a foreign host");
    assert!(body.contains("Domain not connected"), "and it says what to do instead");
}

/// Every page carries the policy that makes the script-free design system a
/// security property rather than a style choice.
#[tokio::test]
async fn every_page_is_hardened() {
    let (state, _dir) = platform().await;
    let (_, headers, _) = send(&state, "GET", "/pricing", None, None).await;
    let csp = headers.get("content-security-policy").unwrap().to_str().unwrap();
    assert!(csp.contains("default-src 'none'"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
    assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
}

/// A destructive action asks first, and the question is a `<details>` rather
/// than a `confirm()` the policy above would block outright.
#[tokio::test]
async fn destructive_actions_ask_before_acting() {
    let (state, _dir) = platform().await;
    let (account, cookie) = as_customer(&state).await;
    let tenant = seed_site(&state, &account.id, "adas-journal").await;

    let (_, body) = get(&state, &format!("/dashboard/sites/{}", tenant.id), Some(&cookie)).await;
    assert!(body.contains(r#"<details class="confirm">"#), "no confirmation step");
    assert!(body.contains("Delete this site?"));
    assert!(!body.contains("onsubmit"), "an inline handler would be blocked by the CSP");

    // The POST itself still works — the confirmation is a UI step, not the
    // authorisation.
    let (status, _, _) = send(
        &state,
        "POST",
        &format!("/dashboard/sites/{}/delete", tenant.id),
        Some(&cookie),
        Some(""),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let tenant = pro_db::tenants::get(&state.db, &tenant.id).await.unwrap();
    assert_eq!(tenant.status, pro_core::TenantStatus::PendingDeletion);
    assert!(tenant.delete_after.is_some(), "with a grace period, not at once");

    // And the site page then offers the way back.
    let (_, body) = get(&state, &format!("/dashboard/sites/{}", tenant.id), Some(&cookie)).await;
    assert!(body.contains("Keep this site"));
    assert!(body.contains("Scheduled for deletion"));
}

/// A customer's own site title reaches an operator's console, so it is the
/// obvious injection path between the two populations.
#[tokio::test]
async fn a_hostile_site_title_cannot_attack_the_console() {
    let (state, _dir) = platform().await;
    let hash = pro_db::accounts::hash_password("a-long-enough-password").unwrap();
    let account = pro_db::accounts::create(&state.db, "cust@example.com", "A Customer", &hash)
        .await
        .unwrap();
    let tenant = pro_db::tenants::create(
        &state.db,
        &account.id,
        "hostile",
        r#"<img src=x onerror="alert(1)">"#,
        "starter",
        &pro_core::now_iso(),
        21_000,
        28_999,
    )
    .await
    .unwrap();

    let cookie = as_operator(&state, StaffRole::Owner).await;
    for path in ["/console/sites", &format!("/console/sites/{}", tenant.id)] {
        let (status, body) = get(&state, path, Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("<img src=x"), "{path}: the tag survived unescaped");
        assert!(body.contains("&lt;img src=x"), "{path}: escaped instead");
        assert!(!body.contains(r#"onerror="alert(1)""#), "{path}: a handler got through");
    }
}

/// A tiny temporary directory, so these tests leave nothing behind.
///
/// Written out rather than taken from a crate: `dev-dependencies` for four
/// lines of `std::fs` is a dependency to audit and upgrade forever, and the
/// only requirement here is a unique path that is removed on drop.
mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("pro-web-test-{unique}"));
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
