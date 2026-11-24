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

const HOST: &str = "localhost:8080";

#[tokio::test]
async fn old_console_urls_redirect_to_owner_without_losing_queries_or_post_methods() {
    let (state, _dir) = platform().await;
    for (old, new) in [
        ("/console", "/owner/dashboard"),
        ("/console/", "/owner/dashboard"),
        ("/console?theme=dark", "/owner/dashboard?theme=dark"),
        ("/console/signin", "/owner/signin"),
        ("/console/customers?page=2", "/owner/customers?page=2"),
        ("/console/sites/site-id/suspend", "/owner/sites/site-id/suspend"),
    ] {
        for method in ["GET", "POST"] {
            let (status, headers, _) = send(&state, method, old, None, None).await;
            assert_eq!(status, StatusCode::PERMANENT_REDIRECT, "{method} {old}");
            assert_eq!(headers[header::LOCATION], new);
        }
    }
    let (status, headers, _) = send(&state, "GET", "/owner/dashboard", None, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/owner/signin");
    let (status, body) = get(&state, "/owner/signin", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"action="/owner/signin""#));
    assert!(!body.contains("/console"));
}

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
        "/owner/signin",
        None,
        Some("email=ops@example.com&password=a-very-long-operator-password"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "a good password signs in");
    assert_eq!(headers[header::LOCATION], "/owner/dashboard");
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
    assert!(body.contains(".shell::before") && body.contains("--glass-blur"), "{where_}: no glass surfaces");
    assert!(body.contains(r#"aria-current="page""#), "{where_}: no current navigation link");
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
    let builtin = pro_db::domains::primary_for_tenant(&state.db, &tenant.id).await.unwrap().unwrap();

    for path in [
        "/dashboard",
        "/dashboard/new",
        "/dashboard/billing",
        "/dashboard/account",
        "/dashboard/domains",
        &format!("/dashboard/sites/{}", tenant.id),
        &format!("/dashboard/domains/{}", builtin.id),
    ] {
        let (status, body) = get(&state, path, Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK, "{path} did not render");
        assert_m3_shell(&body, path);
        // The rail's own entries, so a page cannot lose its navigation.
        assert!(body.contains(r#"href="/dashboard/billing""#), "{path}: no Billing link");
        assert!(body.contains(r#"href="/dashboard/account""#), "{path}: no Account link");
        assert!(body.contains(r#"href="/dashboard/domains""#), "{path}: no Domains link");
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

/// Every link to a tenant goes through the same edge as the panel itself, so
/// it has to carry that edge's port. On this platform — and on any development
/// machine — that port is 8080, and a link to `http://fresh.localhost/ghost/`
/// reaches nothing at all: the customer clicks "Open admin" and the browser
/// fails to connect. The address shown beside the link is the same promise in
/// text, so it carries the port too.
#[tokio::test]
async fn links_to_a_site_carry_the_edge_port() {
    let (state, _dir) = platform().await;
    let (account, cookie) = as_customer(&state).await;
    let tenant = seed_site(&state, &account.id, "fresh").await;
    let operator = as_operator(&state, StaffRole::Owner).await;
    let builtin = pro_db::domains::primary_for_tenant(&state.db, &tenant.id).await.unwrap().unwrap();

    for (path, who) in [
        ("/dashboard".to_string(), &cookie),
        (format!("/dashboard/sites/{}", tenant.id), &cookie),
        (format!("/dashboard/domains/{}", builtin.id), &cookie),
        ("/owner/sites".to_string(), &operator),
        (format!("/owner/sites/{}", tenant.id), &operator),
        (format!("/owner/domains/{}", builtin.id), &operator),
    ] {
        let (status, body) = get(&state, &path, Some(who)).await;
        assert_eq!(status, StatusCode::OK, "{path} did not render");
        assert!(body.contains("fresh.localhost:8080"), "{path}: the port is missing");
        // Not one url anywhere on the page keeps the portless spelling. The
        // bare domain on its own is fine and expected — it is a DNS name in
        // the domains table and in the records to add — so this looks only at
        // what follows a scheme.
        for portless in ["://fresh.localhost/", "://fresh.localhost\"", "://fresh.localhost<"] {
            assert!(!body.contains(portless), "{path}: a url drops the port ({portless})");
        }
    }

    // And the button itself, on both panels.
    for (path, who) in [
        (format!("/dashboard/sites/{}", tenant.id), &cookie),
        (format!("/owner/sites/{}", tenant.id), &operator),
    ] {
        let (_, body) = get(&state, &path, Some(who)).await;
        assert!(
            body.contains(r#"href="http://fresh.localhost:8080/ghost/""#),
            "{path}: Open admin does not point at the admin"
        );
    }
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
    for path in ["/owner/dashboard", "/owner/sites", "/owner/settings"] {
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
    let builtin = pro_db::domains::primary_for_tenant(&state.db, &tenant.id).await.unwrap().unwrap();

    for path in [
        "/owner/dashboard",
        "/owner/sites",
        "/owner/domains",
        &format!("/owner/domains/{}", builtin.id),
        "/owner/customers",
        "/owner/settings",
        "/owner/staff",
        "/owner/activity",
        "/owner/account",
        &format!("/owner/sites/{}", tenant.id),
        &format!("/owner/customers/{}", account.id),
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

/// Every registered field belongs to a focused page; the directory only links to them.
#[tokio::test]
async fn settings_directory_and_pages_cover_the_registry() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Owner).await;
    let (status, directory) = get(&state, "/owner/settings", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(directory.contains("Search all settings"));
    assert!(directory.contains(r#"href="/owner/staff""#));
    assert!(directory.contains(r#"href="/owner/plans""#));
    let mut preview = Vec::new();
    for (slug, group) in [
        ("general", "Platform"), ("signup", "Signup"), ("billing", "Billing"),
        ("email", "Mail"),
    ] {
        let path = format!("/owner/settings/{slug}");
        assert!(directory.contains(&format!(r#"href="{path}""#)));
        let (status, page) = get(&state, &path, Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(page.contains(&format!(r#"action="{path}""#)));
        assert!(page.contains("Back to settings"));
        assert!(page.contains(r#"form="settings-form""#));
        for def in pro_core::settings::REGISTRY {
            let field = format!(r#"name="{}""#, def.key);
            assert!(!directory.contains(&field), "the directory must not contain editable fields");
            let belongs = (def.group == group
                || (slug == "billing" && def.key.starts_with("billing."))
                || (slug == "email" && def.key.starts_with("mail.")))
                && !matches!(def.kind, pro_core::settings::Kind::Managed);
            assert_eq!(page.contains(&field), belongs, "{} is on the wrong page: {slug}", def.key);
        }
        preview.push((slug.to_string(), page));
    }
    let old_pages = pro_core::config::BILLING_PROVIDERS.iter().map(|provider| ("billing", *provider))
        .chain([("email", "ses"), ("email", "mailgun")]);
    for (section, provider) in old_pages {
        let path = format!("/owner/settings/{provider}");
        assert!(!directory.contains(&format!(r#"href="{path}""#)));
        let destination = format!("/owner/settings/{section}?provider={provider}#{section}-{provider}");
        for method in ["GET", "POST"] {
            let (status, headers, _) = send(&state, method, &path, Some(&cookie), None).await;
            assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
            assert_eq!(headers[header::LOCATION], destination);
        }
    }
    // Optional local artifacts contain fixture data only and no real account sessions.
    if let Some(dir) = std::env::var_os("VUUUT_SETTINGS_PREVIEW_DIR") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.html"), directory).unwrap();
        for (slug, html) in preview { std::fs::write(dir.join(format!("{slug}.html")), html).unwrap(); }
    }
}

#[tokio::test]
async fn settings_search_finds_fields_and_escapes_the_query() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Owner).await;
    let (status, body) = get(&state, "/owner/settings?q=SMTP%20password", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/owner/settings/email?provider=ses#setting-mail_ses_pass"));
    assert!(body.contains("/owner/settings/email#setting-mail_pass"));
    assert!(!body.contains("provider=stripe"));
    let (_, body) = get(&state, "/owner/settings?q=Mailgun%20API%20key", Some(&cookie)).await;
    assert!(body.contains("/owner/settings/email?provider=mailgun#setting-mail_mailgun_apiKey"));
    let (_, body) = get(&state, "/owner/settings/email?provider=mailgun", Some(&cookie)).await;
    assert!(body.contains(r#"<details id="email-mailgun" open>"#));
    let (_, body) = get(&state, "/owner/settings?q=Polar%20access%20token", Some(&cookie)).await;
    assert!(body.contains("/owner/settings/billing?provider=polar#setting-billing_polar_accessToken"));
    let (_, body) = get(&state, "/owner/settings/billing?provider=polar", Some(&cookie)).await;
    assert!(body.contains(r#"<details id="billing-polar" open>"#));
    let (_, body) = get(&state, "/owner/settings?q=%3Cscript%3E", Some(&cookie)).await;
    assert!(body.contains("Nothing matches"));
    assert!(body.contains("&lt;script&gt;"));
    assert!(!body.contains("<script>"));
}

#[tokio::test]
async fn section_saves_stay_on_the_page_and_cannot_write_other_sections() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Owner).await;
    let (status, headers, _) = send(&state, "POST", "/owner/settings/general", Some(&cookie), Some("platform.name=Acme")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers["location"].to_str().unwrap().starts_with("/owner/settings/general?"));
    assert_eq!(state.live().platform_name(), "Acme");
    assert!(state.live().signup_enabled());
    let (_, headers, _) = send(&state, "POST", "/owner/settings/general", Some(&cookie), Some("platform.name=MustNotSave&__present.signup.enabled=1")).await;
    assert!(headers["location"].to_str().unwrap().starts_with("/owner/settings/general?"));
    assert_eq!(state.live().platform_name(), "Acme", "a forged cross-section submission must be atomic");
    assert!(state.live().signup_enabled());
    let (_, headers, _) = send(&state, "POST", "/owner/settings/signup", Some(&cookie), Some("__present.signup.enabled=1")).await;
    assert!(headers["location"].to_str().unwrap().starts_with("/owner/settings/signup?"));
    assert!(!state.live().signup_enabled());
    let (_, headers, _) = send(&state, "POST", "/owner/settings/signup", Some(&cookie), Some("__present.signup.enabled=1&signup.enabled=1")).await;
    assert!(headers["location"].to_str().unwrap().starts_with("/owner/settings/signup?"));
    assert!(state.live().signup_enabled());
    let (status, _) = get(&state, "/owner/settings/not-a-section", Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn section_secrets_are_write_only_and_survive_blank_saves() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Admin).await;
    let path = "/owner/settings/email";
    let (status, headers, _) = send(&state, "POST", path, Some(&cookie), Some(concat!(
        "mail.tenantProvider=ses&mail.fromAddress=delivery%40example.com",
        "&mail.ses.region=us-east-1&mail.ses.user=ses-private-user",
        "&mail.ses.pass=ses-private-password",
        "&mail.ses.endpoint=example.fips.region.mail-manager-smtp.amazonaws.com",
    ))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers[header::LOCATION].to_str().unwrap().starts_with("/owner/settings/email?"));
    let (_, html) = get(&state, path, Some(&cookie)).await;
    assert!(html.contains(r#"<details id="email-ses" open>"#));
    assert_eq!(state.live().mail().tenant_provider, "ses");
    assert!(!html.contains("ses-private-user"));
    assert!(!html.contains("ses-private-password"));
    assert!(html.contains("example.fips.region.mail-manager-smtp.amazonaws.com"));
    assert_eq!(state.live().mail().ses_smtp_host().as_deref(), Some("example.fips.region.mail-manager-smtp.amazonaws.com"));
    let _ = send(&state, "POST", path, Some(&cookie), Some("mail.ses.user=&mail.ses.pass=&mail.ses.region=eu-west-1")).await;
    assert_eq!(state.live().mail().ses.pass.as_deref(), Some("ses-private-password"));
    assert_eq!(state.live().mail().ses.region.as_deref(), Some("eu-west-1"));
    let (_, headers, _) = send(&state, "POST", path, Some(&cookie), Some(concat!(
        "mail.tenantProvider=mailgun&mail.mailgun.domain=mg.example.com",
        "&mail.mailgun.apiKey=mailgun-private-key&mail.mailgun.webhookSigningKey=mailgun-private-signing-key",
    ))).await;
    assert!(headers[header::LOCATION].to_str().unwrap().starts_with("/owner/settings/email?"));
    let (_, html) = get(&state, path, Some(&cookie)).await;
    assert!(html.contains(r#"<details id="email-mailgun" open>"#));
    assert!(!html.contains("mailgun-private-key"));
    assert!(!html.contains("mailgun-private-signing-key"));
    assert_eq!(state.live().mail().tenant_provider, "mailgun");
    assert_eq!(state.live().mail().ses.pass.as_deref(), Some("ses-private-password"));
    let (_, html) = get(&state, "/owner/settings/billing", Some(&cookie)).await;
    assert!(!html.contains(r#"form="settings-form""#), "admin must not receive an owner-only Save button");
    assert!(html.contains("Owner only"));
}

#[tokio::test]
async fn billing_saves_provider_and_credentials_together() {
    let (state, _dir) = platform().await;
    let owner = as_operator(&state, StaffRole::Owner).await;
    let path = "/owner/settings/billing";

    let (status, headers, _) = send(&state, "POST", path, Some(&owner), Some(concat!(
        "billing.provider=polar&billing.polar.accessToken=fixture-token",
        "&billing.polar.webhookSecret=fixture-secret",
        "&__present.billing.polar.sandbox=1&billing.polar.sandbox=1",
        "&__present.billing.paddle.sandbox=1",
    ))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers[header::LOCATION].to_str().unwrap().starts_with("/owner/settings/billing?ok="));
    assert_eq!(state.live().billing_provider(), "polar");
    assert!(state.live().polar().sandbox);
    assert!(!state.live().is_overridden("billing.paddle.sandbox"));

    let (_, page) = get(&state, path, Some(&owner)).await;
    assert!(page.contains(r#"<details id="billing-polar" open>"#));
    assert!(page.contains(r#"<details id="billing-stripe">"#));
    assert!(!page.contains("fixture-token"));
    assert!(!page.contains("fixture-secret"));

    let _ = send(&state, "POST", path, Some(&owner), Some(concat!(
        "billing.provider=polar&billing.polar.accessToken=&billing.polar.webhookSecret=",
        "&__present.billing.polar.sandbox=1&billing.polar.sandbox=1",
    ))).await;
    assert!(state.live().provider_is_configured("polar"));

    let _ = send(&state, "POST", path, Some(&owner), Some(
        "billing.polar.accessToken=should-not-save&platform.name=should-not-save",
    )).await;
    assert_eq!(state.live().polar().access_token.as_deref(), Some("fixture-token"));
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

    // Polar selected, with credentials but no product ids: the state a
    // platform is in on its first day.
    state
        .settings
        .save(
            &state.db,
            &[
                ("billing.polar.accessToken".into(), Some(serde_json::json!("polar_oat_test"))),
                ("billing.polar.webhookSecret".into(), Some(serde_json::json!("polar_whs_test"))),
                ("billing.provider".into(), Some(serde_json::json!("polar"))),
            ],
            StaffRole::Owner,
            "test",
        )
        .await
        .unwrap();

    assert_eq!(
        state.live().purchasable_plans().len(),
        0,
        "nothing can be bought until a product id is set"
    );

    let (status, body) = get(&state, "/owner/plans", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_m3_shell(&body, "/owner/settings");
    // It says plainly that the platform cannot sell, which is the whole point
    // of the screen existing.
    assert!(body.contains("Nothing can be bought yet"), "got no warning");
    // A field per plan, named for the plan and the provider, and labelled with
    // what Polar actually calls the thing.
    assert!(body.contains(r#"name="id.starter.polar""#), "got {body}");
    assert!(body.contains("Product ID"));
    // And the endpoint the operator has to paste into Polar.
    assert!(body.contains("/webhooks/polar"));
    assert!(body.contains("subscription.created, subscription.updated"), "the events to enable");

    // Set one, the way the form posts it.
    let (status, _, _) = send(
        &state,
        "POST",
        "/owner/plans",
        Some(&cookie),
        Some("id.starter.polar=07521167-2b19-420a-9714-754d4b1d84ac&id.creator.polar="),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Live immediately — no restart, which is the requirement.
    let live = state.live();
    assert_eq!(
        live.plan("starter").unwrap().price_id("polar"),
        Some("07521167-2b19-420a-9714-754d4b1d84ac"),
        "the id is in the live catalogue"
    );
    assert_eq!(live.purchasable_plans().len(), 1, "and that plan is now on sale");
    // The blank one was not invented.
    assert_eq!(live.plan("creator").unwrap().price_id("polar"), None);

    // The webhook path can now resolve a plan from that product, which is
    // what makes a renewal apply the right limits.
    assert_eq!(
        pro_billing::plan_for_price(&live.plans(), "polar", "07521167-2b19-420a-9714-754d4b1d84ac")
            .map(|p| p.slug.clone()),
        Some("starter".to_string())
    );

    // The screen now reflects it, and stops warning.
    let (_, body) = get(&state, "/owner/plans", Some(&cookie)).await;
    assert!(body.contains(r#"value="07521167-2b19-420a-9714-754d4b1d84ac""#));
    assert!(!body.contains("Nothing can be bought yet"));
    assert!(body.contains("On sale"));

    // Clearing it removes the id rather than storing a blank that masks the
    // config file.
    let (status, _, _) = send(
        &state,
        "POST",
        "/owner/plans",
        Some(&cookie),
        Some("id.starter.polar="),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(state.live().plan("starter").unwrap().price_id("polar"), None);
}

/// Product ids are billing credentials in all but name, so they are the
/// owner's alone — and the check is the handler's, not the disabled attribute
/// the page happens to render.
#[tokio::test]
async fn only_an_owner_may_set_product_ids() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Admin).await;

    let (status, body) = get(&state, "/owner/plans", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK, "an admin may look");
    assert!(body.contains("can only be changed by the Owner"), "and is told why not");

    let (status, _, _) = send(
        &state,
        "POST",
        "/owner/plans",
        Some(&cookie),
        Some("id.starter.polar=07521167-2b19-420a-9714-754d4b1d84ac"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "refused, back to the page");
    assert_eq!(
        state.live().plan("starter").unwrap().price_id("polar"),
        None,
        "nothing was written"
    );
}

/// A crafted field must not invent a plan or a provider.
#[tokio::test]
async fn the_plans_form_refuses_what_it_does_not_know() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Owner).await;

    for hostile in ["id.no-such-plan.polar=x", "id.starter.notaprovider=x"] {
        let (status, _, _) =
            send(&state, "POST", "/owner/plans", Some(&cookie), Some(hostile)).await;
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
        "/owner/settings",
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
        "/owner/settings",
        Some(&cookie),
        Some("__present.signup.enabled=1&signup.enabled=1"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(state.live().signup_enabled());

    // A submission mentioning neither leaves it exactly as it was — this is
    // the guard against a partial POST reading as "every switch off".
    let (status, _, _) =
        send(&state, "POST", "/owner/settings", Some(&cookie), Some("platform.name=Acme")).await;
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

    let (status, body) = get(&state, &format!("/owner/sites/{}", tenant.id), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("/suspend"), "suspend is Admin and above");
    assert!(body.contains("Apply plan"), "the plan control is shown…");
    assert!(body.contains("disabled"), "…but locked");

    // And the handler refuses regardless of what the page offered.
    let (status, _, _) = send(
        &state,
        "POST",
        &format!("/owner/sites/{}/suspend", tenant.id),
        Some(&cookie),
        Some("reason=because"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "refused, back to the page");
    let tenant = pro_db::tenants::get(&state.db, &tenant.id).await.unwrap();
    assert_eq!(tenant.status, pro_core::TenantStatus::Active, "still running");

    // Staff management is an owner's, so the invite form is not drawn.
    let (status, staff) = get(&state, "/owner/staff", Some(&cookie)).await;
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

    let (status, _) = get(&state, "/owner/dashboard", Some(&customer)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "a customer cookie is no operator");

    let (status, _) = get(&state, "/dashboard", Some(&operator)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "and the reverse");
}

// ---------------------------------------------------------------------------
// Public pages, appearance and the host gate
// ---------------------------------------------------------------------------

/// `/` is deliberately absent from these lists. It is the marketing landing
/// page now, a hand-authored static document rather than something
/// `view` builds, so it has its own palette, its own stylesheet and one
/// script — none of which these assertions should hold for. What *should*
/// hold for it is in `the_landing_page_is_served_at_the_root` below.
#[tokio::test]
async fn every_public_page_renders() {
    let (state, _dir) = platform().await;
    for path in ["/pricing", "/signup", "/dashboard/signin", "/forgot", "/reset"] {
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

/// The landing page, and the things that are true of it instead.
///
/// It is the first thing anybody sees on the domain, and it is the one page
/// here that is allowed a script — so the two properties worth pinning are
/// that it actually is the root, and that its own stylesheet and script are
/// reachable. The `style=` check is the load-bearing one: the page's policy
/// deliberately omits `'unsafe-inline'`, so a stray inline style attribute
/// would be silently dropped by the browser and the stagger would go with it.
#[tokio::test]
async fn the_landing_page_is_served_at_the_root() {
    let (state, _dir) = platform().await;

    let (status, body) = get(&state, "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Everything"), "the landing headline is missing");
    assert!(body.contains(r#"href="/assets/landing.css""#), "no stylesheet link");
    assert!(body.contains(r#"href="/signup""#), "the landing page has to sell something");
    assert!(
        !body.contains("style=\""),
        "an inline style attribute: the landing CSP has no 'unsafe-inline' for styles, \
         so this would not apply — put it in landing.css"
    );

    // Both assets are served, and as themselves rather than as HTML.
    for (path, kind, needle) in [
        ("/assets/landing.css", "text/css", "--paper:"),
        ("/assets/landing.js", "text/javascript", "IntersectionObserver"),
    ] {
        let (status, headers, body) = send(&state, "GET", path, None, None).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let ct = headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(ct.starts_with(kind), "{path} served as {ct}");
        assert!(body.contains(needle), "{path} is not the file it should be");
    }

    // The price list stays where people who came to compare will look.
    let (status, _) = get(&state, "/pricing", None).await;
    assert_eq!(status, StatusCode::OK, "/pricing still serves the catalogue");
}

#[tokio::test]
async fn yearly_pricing_defaults_to_monthly_equivalents_with_annual_billing() {
    let (state, _dir) = platform().await;
    let (_, home) = get(&state, "/", None).await;
    assert!(home.contains(r#"id="bill-yearly" checked"#));
    assert!(!home.contains(r#"id="bill-monthly" checked"#));
    assert!(home.contains("$7 / month"));
    for (slug, monthly, annual) in [("starter", "7", "84"), ("creator", "11", "132"), ("business", "33", "396")] {
        assert!(home.contains(&format!(r#"{monthly}<span class="per">/month</span>"#)));
        assert!(home.contains(&format!("${annual} billed yearly")));
        assert!(home.contains(&format!("/signup?plan={slug}-yearly")));
    }

    let (_, yearly) = get(&state, "/pricing", None).await;
    for (slug, monthly, annual) in [("starter", "7", "84"), ("creator", "11", "132"), ("business", "33", "396")] {
        assert!(yearly.contains(&format!(r#"${monthly}<span> / month</span>"#)));
        assert!(yearly.contains(&format!("${annual} billed yearly")));
        let encoded = pro_web::view::urlencode(&format!("{slug}-yearly"));
        assert!(yearly.contains(&format!(r#"href="/signup?plan={encoded}""#)));
        assert!(!yearly.contains(&format!(r#"href="/signup?plan={slug}""#)));
    }
    let (_, monthly) = get(&state, "/pricing?billing=month", None).await;
    for (slug, price) in [("starter", "9"), ("creator", "15"), ("business", "49")] {
        assert!(monthly.contains(&format!(r#"${price}<span> / month</span>"#)));
        assert!(monthly.contains(&format!(r#"href="/signup?plan={slug}""#)));
        let encoded = pro_web::view::urlencode(&format!("{slug}-yearly"));
        assert!(!monthly.contains(&format!(r#"href="/signup?plan={encoded}""#)));
    }
}

/// Being a customer does not take the front page away from you.
///
/// The root used to redirect anybody with a session to `/dashboard`, so the
/// homepage was unreachable for the people most likely to send somebody a
/// link to it. It is served to everybody now, and the redirect moved to the
/// two doors that only make sense signed out — which is what keeps the
/// landing page's single "Start free trial" button correct for both visitors
/// and customers.
#[tokio::test]
async fn a_signed_in_customer_still_gets_the_landing_page() {
    let (state, _dir) = platform().await;
    let (_account, cookie) = as_customer(&state).await;

    let (status, body) = get(&state, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK, "the homepage redirected a signed-in customer away");
    assert!(body.contains("Everything"), "signed in, but not the landing page");

    // Signup and sign-in send them on to their sites instead.
    for path in ["/signup", "/dashboard/signin"] {
        let (status, headers, _) = send(&state, "GET", path, Some(&cookie), None).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{path} did not redirect a signed-in customer");
        assert_eq!(headers.get(header::LOCATION).unwrap(), "/dashboard", "{path}");
    }

    // Signed out, both still render their forms.
    for path in ["/signup", "/dashboard/signin"] {
        let (status, _) = get(&state, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path} broke for a signed-out visitor");
    }
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
        ("/owner/customers?page=2", "/owner/customers?page=2"),
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
        "http://evil.example:8080/owner/dashboard",
        // A host that merely *starts* with ours is a different host.
        "http://localhost.evil.example/owner/dashboard",
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

    for path in ["/pricing", "/signup", "/dashboard/signin", "/owner/signin", "/forgot"] {
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
        .uri("/owner/signin")
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

/// An operator changes their own password from the console. It needs the
/// current password, a matching repeat and the staff minimum; a change signs
/// the login out everywhere, and only the new password signs back in.
#[tokio::test]
async fn an_operator_changes_their_own_password() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Support).await;
    let old = "a-very-long-operator-password";
    let new = "a-brand-new-operator-password";

    // Reachable from the rail's name block, for every role.
    let (status, body) = get(&state, "/owner/account", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"href="/owner/account""#), "the rail links to the page");
    assert!(body.contains(r#"action="/owner/account/password""#));
    assert!(body.contains("ops@example.com"));

    let attempt = |current: &str, new_password: &str, confirm: &str| {
        format!("current={current}&new_password={new_password}&confirm_password={confirm}")
    };
    for (form, message) in [
        (attempt("wrong-password-entirely", new, new), "current password is not correct"),
        (attempt(old, new, "a-brand-new-typo-password"), "do not match"),
        (attempt(old, "too-short", "too-short"), "at least 12 characters"),
        (attempt(old, old, old), "different from the current one"),
    ] {
        let (status, headers, _) =
            send(&state, "POST", "/owner/account/password", Some(&cookie), Some(&form)).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let back = headers[header::LOCATION].to_str().unwrap().to_string();
        assert!(back.starts_with("/owner/account"), "refused back to the form: {back}");
        let (_, page) = get(&state, &back, Some(&cookie)).await;
        assert!(page.contains(message), "expected {message:?} after {form}");
    }
    assert!(pro_db::staff::authenticate(&state.db, "ops@example.com", old).await.is_ok());

    // Without a session nothing changes.
    send(&state, "POST", "/owner/account/password", None, Some(&attempt(old, new, new))).await;
    assert!(pro_db::staff::authenticate(&state.db, "ops@example.com", old).await.is_ok());

    let (status, headers, _) = send(
        &state,
        "POST",
        "/owner/account/password",
        Some(&cookie),
        Some(&attempt(old, new, new)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers[header::LOCATION].to_str().unwrap().starts_with("/owner/signin"));

    // Signed out everywhere, this session included.
    let (status, _) = get(&state, "/owner/account", Some(&cookie)).await;
    assert_ne!(status, StatusCode::OK, "the old session must not survive");
    assert!(pro_db::staff::authenticate(&state.db, "ops@example.com", old).await.is_err());
    assert!(pro_db::staff::authenticate(&state.db, "ops@example.com", new).await.is_ok());
}

/// A site answers on its primary domain only. Its built-in subdomain — and any
/// other hostname it owns — redirects there permanently, path and query
/// intact, so sign-in cookies and the members API live on one origin.
#[tokio::test]
async fn a_secondary_hostname_redirects_to_the_primary_domain() {
    let (state, _dir) = platform().await;
    let (account, _) = as_customer(&state).await;
    let tenant = seed_site(&state, &account.id, "drip").await;
    let custom = pro_db::domains::add(
        &state.db,
        &tenant.id,
        "drip-example.com",
        pro_core::DomainKind::Custom,
        false,
    )
    .await
    .unwrap();
    pro_db::domains::set_status(&state.db, &custom.id, pro_core::DomainStatus::Active, None)
        .await
        .unwrap();
    pro_db::domains::set_primary(&state.db, &tenant.id, &custom.id).await.unwrap();

    let hit = |method: &str, host: &str| {
        Request::builder()
            .method(method)
            .uri("/members/signup/?ref=nav")
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap()
    };

    let response = pro_web::entry(state.clone()).oneshot(hit("GET", "drip.localhost")).await.unwrap();
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    let location = response.headers()[header::LOCATION].to_str().unwrap();
    assert!(location.starts_with("http://drip-example.com"), "{location}");
    assert!(location.ends_with("/members/signup/?ref=nav"), "path and query kept: {location}");

    // A form POST stays a POST.
    let response = pro_web::entry(state.clone()).oneshot(hit("POST", "drip.localhost")).await.unwrap();
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);

    // And the other way round once the subdomain is primary again.
    let builtin = pro_db::domains::find_by_host(&state.db, "drip.localhost").await.unwrap().unwrap();
    pro_db::domains::set_primary(&state.db, &tenant.id, &builtin.id).await.unwrap();
    state.hosts.invalidate_all();
    let response = pro_web::entry(state.clone()).oneshot(hit("GET", "drip-example.com")).await.unwrap();
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert!(response.headers()[header::LOCATION].to_str().unwrap().starts_with("http://drip.localhost"));
}

/// HTTP/2 — what every browser negotiates over TLS — sends no `Host` header;
/// the hostname arrives as the URI's authority. The platform has to recognise
/// itself from that too, or a browser sees "Domain not connected" everywhere.
#[tokio::test]
async fn an_http2_request_with_only_an_authority_reaches_the_platform() {
    let (state, _dir) = platform().await;
    let request = Request::builder()
        .method("GET")
        .uri(format!("http://{HOST}/owner/signin"))
        .version(axum::http::Version::HTTP_2)
        .body(Body::empty())
        .unwrap();
    assert!(request.headers().get(header::HOST).is_none());
    let response = pro_web::entry(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("Operator sign in"));
}

/// Over HTTP/2 Chrome sends each cookie as its own `cookie` header once there
/// is more than one. A sign-in cookie that is not the first of them must still
/// be seen, or signing in just reloads the sign-in page.
#[tokio::test]
async fn a_session_cookie_in_a_second_http2_cookie_header_still_signs_in() {
    let (state, _dir) = platform().await;
    let operator = as_operator(&state, StaffRole::Owner).await;
    let request = Request::builder()
        .method("GET")
        .uri(format!("http://{HOST}/owner/dashboard"))
        .version(axum::http::Version::HTTP_2)
        .header(header::COOKIE, "pro_theme=dark")
        .header(header::COOKIE, operator)
        .body(Body::empty())
        .unwrap();
    let response = pro_web::entry(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "the second cookie header was ignored");
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
    for path in ["/owner/sites", &format!("/owner/sites/{}", tenant.id)] {
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

#[tokio::test]
async fn console_managed_mail_updates_existing_sites_and_releases_them() {
    use serde_json::json;
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Admin).await;
    let account = pro_db::accounts::create(&state.db, "tenant@example.test", "Tenant", "fixture-hash").await.unwrap();
    let tenant = pro_db::tenants::create(&state.db, &account.id, "mail-site", "Mail site", "starter", "2026-01-01", 31000, 31100).await.unwrap();
    pro_db::tenants::set_status(&state.db, &tenant.id, pro_core::TenantStatus::Active).await.unwrap();
    let dir = state.config.tenant_dir(&tenant.id);
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let body = pro_provision::config_file::build("https://mail-site.example.test", tenant.port, &state.config.plans[0], "2026-01-01", "https://billing", &Default::default(), &pro_provision::config_file::database_block(None));
    pro_provision::config_file::write(&dir, &state.config.tenants.env, &body).await.unwrap();

    let (status, _, _) = send(&state, "POST", "/owner/settings", Some(&cookie), Some("mail.tenantProvider=ses&mail.ses.region=us-east-1&mail.ses.user=console-ses-user&mail.ses.pass=console-ses-password&mail.fromAddress=verified%40example.test")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(state.live().mail().tenant_provider, "ses");
    let (_, html) = get(&state, "/owner/settings", Some(&cookie)).await;
    assert!(html.contains("Amazon SES"));
    assert!(!html.contains("console-ses-password"));
    assert!(!html.contains("console-ses-user"));
    let payloads: Vec<String> = sqlx::query_scalar("SELECT payload FROM jobs WHERE kind='sync_tenant_mail'").fetch_all(&state.db).await.unwrap();
    assert!(!payloads.is_empty());
    assert!(payloads.iter().all(|p| !p.contains("console-ses")));
    let worker = tokio::spawn(pro_web::worker::run_jobs(state.clone()));
    async fn await_provider(state: &pro_web::AppState, dir: &std::path::Path, expected: Option<&str>) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let config = pro_provision::config_file::read(dir, &state.config.tenants.env).await.unwrap();
                let actual = config.pointer("/hostSettings/managedMail/newsletter_transport").and_then(serde_json::Value::as_str);
                if actual == expected { break; }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }).await.unwrap();
    }
    await_provider(&state, &dir, Some("ses")).await;
    assert!(!state.supervisor.is_running(&tenant.id), "hibernating sites must stay asleep");
    let (status, _, _) = send(&state, "POST", "/owner/settings", Some(&cookie), Some("mail.tenantProvider=mailgun&mail.mailgun.apiKey=console-mailgun-key&mail.mailgun.domain=mg.example.test")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    await_provider(&state, &dir, Some("mailgun")).await;
    let (status, _, _) = send(&state, "POST", "/owner/settings", Some(&cookie), Some("mail.tenantProvider=tenant")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    await_provider(&state, &dir, None).await;
    let body = pro_provision::config_file::read(&dir, &state.config.tenants.env).await.unwrap();
    assert!(body["hostSettings"]["limits"].is_object());
    assert_eq!(body["url"], json!("https://mail-site.example.test"));
    worker.abort();
}

#[tokio::test]
async fn plan_email_allowances_are_editable_and_update_public_content() {
    let (state,_dir)=platform().await;
    let cookie=as_operator(&state,StaffRole::Owner).await;
    let (_,html)=get(&state,"/owner/plans",Some(&cookie)).await;
    for plan in state.live().plans() {assert!(html.contains(&format!(r#"name="emails.{}""#,plan.slug)));}
    let (status,_,_)=send(&state,"POST","/owner/plans",Some(&cookie),Some("emails.starter=7000&emails.starter-yearly=8000")).await;
    assert_eq!(status,StatusCode::SEE_OTHER);
    assert_eq!(state.live().plan("starter").unwrap().email_allowance(),7000);
    assert_eq!(state.live().plan("starter-yearly").unwrap().email_allowance(),8000);
    let (_,homepage)=get(&state,"/",None).await;
    assert!(homepage.contains("7,000 emails / month"));assert!(homepage.contains("8,000 emails / month"));
    assert!(!homepage.contains("no send quota"));assert!(!homepage.contains("{{email_"));
    assert!(homepage.contains("Email sending included"));
    let jobs:i64=sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind='sync_tenant_mail'").fetch_one(&state.db).await.unwrap();
    assert!(jobs>0);
    let _=send(&state,"POST","/owner/plans",Some(&cookie),Some("emails.starter=-1&emails.creator=9000")).await;
    assert_eq!(state.live().plan("starter").unwrap().email_allowance(),7000);
    assert_eq!(state.live().plan("creator").unwrap().email_allowance(),25000);
    if let Some(dir)=std::env::var_os("VUUUT_EMAIL_PREVIEW_DIR") {
        let dir=std::path::PathBuf::from(dir);std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("homepage.html"),homepage).unwrap();
        let (_,plans)=get(&state,"/owner/plans",Some(&cookie)).await;std::fs::write(dir.join("plans.html"),plans).unwrap();
    }
}

#[tokio::test]
async fn site_email_details_show_volume_and_support_individual_auto_stop_policy() {
    use serde_json::json;
    let (state,_dir)=platform().await;
    let cookie=as_operator(&state,StaffRole::Admin).await;
    let account=pro_db::accounts::create(&state.db,"volume@example.test","Volume owner","fixture").await.unwrap();
    let tenant=pro_db::tenants::create(&state.db,&account.id,"volume-site","Volume site","starter","2026-01-01",31000,31100).await.unwrap();
    pro_db::tenants::set_status(&state.db,&tenant.id,pro_core::TenantStatus::Active).await.unwrap();
    let report=json!({"sent":50,"limit":5000,"pending":2,"failed":3,"blocked":4,"newsletter_sent":40,"transactional_sent":10,"total_sent":250,"remaining":4948,"period_start":"2026-11-01T00:00:00Z","period_end":"2026-12-01T00:00:00Z","daily":[{"date":"2026-11-02","sent":50,"failed":3,"blocked":4}],"providers":[{"provider":"ses","sent":50}],"history_note":"Accepted submissions"});
    pro_db::email_usage::record(&state.db,&tenant.id,&report).await.unwrap();
    let path=format!("/owner/sites/{}",tenant.id);
    let (status,html)=get(&state,&path,Some(&cookie)).await;
    assert_eq!(status,StatusCode::OK);
    for label in ["Sent this period","Monthly allowance","Remaining","Newsletter sends","Transactional sends","Daily email activity","Sending providers","Save sending policy","5,000","4,948"] {assert!(html.contains(label),"missing {label}");}
    let policy_url=format!("{path}/email-policy");
    let _=send(&state,"POST",&policy_url,Some(&cookie),Some("policy=continue")).await;
    assert_eq!(pro_db::email_usage::policy(&state.db,&tenant.id).await.unwrap(),Some(false));
    let _=send(&state,"POST",&policy_url,Some(&cookie),Some("policy=stop")).await;
    assert_eq!(pro_db::email_usage::policy(&state.db,&tenant.id).await.unwrap(),Some(true));
    let _=send(&state,"POST",&policy_url,Some(&cookie),Some("policy=inherit")).await;
    assert_eq!(pro_db::email_usage::policy(&state.db,&tenant.id).await.unwrap(),None);
    assert!(state.live().mail().auto_stop_at_limit);
    let _=send(&state,"POST","/owner/settings/email",Some(&cookie),Some("__present.mail.autoStopAtLimit=1")).await;
    assert!(!state.live().mail().auto_stop_at_limit);
    if let Some(dir)=std::env::var_os("VUUUT_EMAIL_PREVIEW_DIR") {
        let dir=std::path::PathBuf::from(dir);std::fs::create_dir_all(&dir).unwrap();std::fs::write(dir.join("site.html"),html).unwrap();
    }
}


// ---------------------------------------------------------------------------
// Domains
// ---------------------------------------------------------------------------

/// A second customer with a site, for the tests about whose domains are whose.
async fn someone_else(state: &pro_web::AppState) -> (pro_core::Account, pro_core::Tenant) {
    let hash = pro_db::accounts::hash_password("another-long-password").unwrap();
    let other = pro_db::accounts::create(&state.db, "other@example.com", "Someone Else", &hash)
        .await
        .unwrap();
    let tenant = seed_site(state, &other.id, "theirs").await;
    (other, tenant)
}

#[tokio::test]
async fn a_customer_sees_only_their_own_domains() {
    let (state, _dir) = platform().await;
    let (account, cookie) = as_customer(&state).await;
    seed_site(&state, &account.id, "adas-journal").await;
    let (_, theirs) = someone_else(&state).await;
    let their_domain =
        pro_db::domains::primary_for_tenant(&state.db, &theirs.id).await.unwrap().unwrap();

    let (status, list) = get(&state, "/dashboard/domains", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(list.contains("adas-journal.localhost"), "their own built-in address");
    assert!(!list.contains("theirs.localhost"), "nobody else's");
    assert!(list.contains("Connect your own domain"), "the starter plan has room for one");

    // Somebody else's domain id is a 404, so ids cannot be probed.
    let (status, body) =
        get(&state, &format!("/dashboard/domains/{}", their_domain.id), Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!body.contains("theirs.localhost"));

    let (status, headers, _) = send(
        &state,
        "POST",
        &format!("/dashboard/domains/{}/dns/setup", their_domain.id),
        Some(&cookie),
        Some(""),
    ).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers[header::LOCATION].to_str().unwrap().contains("No%20such%20domain"));
}

#[tokio::test]
async fn a_domain_is_connected_managed_and_removed_from_its_own_page() {
    let (state, _dir) = platform().await;
    let (account, cookie) = as_customer(&state).await;
    let tenant = seed_site(&state, &account.id, "adas-journal").await;

    // Added from the Domains screen, which lands on the new domain's page.
    let (status, headers, _) = send(
        &state,
        "POST",
        "/dashboard/domains",
        Some(&cookie),
        Some(&format!("site={}&domain=Blog.Example.com", tenant.id)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = headers.get(header::LOCATION).unwrap().to_str().unwrap().to_string();
    assert!(location.starts_with("/dashboard/domains/"), "goes to the domain: {location}");

    let added = pro_db::domains::find_by_host(&state.db, "blog.example.com")
        .await
        .unwrap()
        .expect("stored, lowercased");
    let page = format!("/dashboard/domains/{}", added.id);
    let (status, body) = get(&state, &page, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_m3_shell(&body, "domain page");
    assert!(body.contains("Waiting for DNS"), "its state, in words");
    assert!(body.contains("CNAME"), "the record that routes it");
    assert!(body.contains(&added.verification_token), "and the ownership token");
    assert!(body.contains("_vuuut-pro-challenge.blog"));
    assert!(body.contains("Connect automatically"));
    assert!(body.contains(&format!(r#"action="{page}/dns/setup""#)));
    assert!(body.contains("Set up DNS manually"));
    assert!(body.contains("leave TTL at its default"));
    assert!(body.contains("Check now"));
    assert!(body.contains("Added"), "its history");
    assert!(!body.contains("Verification token"), "the raw record is the console's");
    // Its buttons come back to this page rather than the site's.
    assert!(body.contains("/check?back=domain"));

    // The plan's one custom domain is used, so the add form stops offering.
    let (_, list) = get(&state, "/dashboard/domains", Some(&cookie)).await;
    assert!(list.contains("blog.example.com"));
    assert!(list.contains("used every custom domain"));

    // Removed from its own page: back to the list, and gone.
    let (status, headers, _) = send(
        &state,
        "POST",
        &format!("/dashboard/sites/{}/domains/{}/remove?back=domain", tenant.id, added.id),
        Some(&cookie),
        Some(""),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = headers.get(header::LOCATION).unwrap().to_str().unwrap();
    assert!(location.starts_with("/dashboard/domains?"), "back to the list: {location}");
    assert!(pro_db::domains::find_by_host(&state.db, "blog.example.com").await.unwrap().is_none());
}

#[tokio::test]
async fn domain_connect_return_requires_a_fresh_token_and_still_checks_dns() {
    let (state, _dir) = platform().await;
    let (account, cookie) = as_customer(&state).await;
    let tenant = seed_site(&state, &account.id, "adas-journal").await;
    let domain = pro_db::domains::add(
        &state.db,
        &tenant.id,
        "blog.example.invalid",
        pro_core::DomainKind::Custom,
        false,
    )
    .await
    .unwrap();
    let page = format!("/dashboard/domains/{}", domain.id);
    let setup_page = format!("{page}/dns/setup");
    let (status, manual) = get(&state, &setup_page, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(manual.contains("Set up DNS manually"));
    assert!(manual.contains("find the DNS provider"));

    let (_, _, manual) = send(
        &state,
        "POST",
        &setup_page,
        Some(&cookie),
        Some(""),
    )
    .await;
    assert!(manual.contains("Set up DNS manually"));
    assert!(manual.contains("find the DNS provider"));

    let token = pro_db::tokens::issue(
        &state.db,
        pro_db::tokens::DOMAIN_CONNECT,
        "account",
        &account.id,
        serde_json::json!({ "domain_id": domain.id }),
    )
    .await
    .unwrap();
    let callback = format!("{page}/dns/finish/{token}");
    let (status, headers, _) = send(&state, "GET", &callback, Some(&cookie), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers[header::LOCATION]
        .to_str()
        .unwrap()
        .contains("DNS%20is%20being%20verified"));
    let checked = pro_db::domains::get(&state.db, &domain.id).await.unwrap();
    assert_ne!(checked.status, pro_core::DomainStatus::Active);
    assert!(checked.last_checked_at.is_some());

    let (status, headers, _) = send(&state, "GET", &callback, Some(&cookie), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers[header::LOCATION]
        .to_str()
        .unwrap()
        .contains("expired%20or%20already%20been%20used"));
}

#[tokio::test]
async fn the_back_parameter_is_not_an_open_redirect() {
    let (state, _dir) = platform().await;
    let (account, cookie) = as_customer(&state).await;
    let tenant = seed_site(&state, &account.id, "adas-journal").await;
    let builtin = pro_db::domains::primary_for_tenant(&state.db, &tenant.id).await.unwrap().unwrap();

    let (status, headers, _) = send(
        &state,
        "POST",
        &format!(
            "/dashboard/sites/{}/domains/{}/check?back=https://evil.example",
            tenant.id, builtin.id
        ),
        Some(&cookie),
        Some(""),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = headers.get(header::LOCATION).unwrap().to_str().unwrap();
    assert!(location.starts_with(&format!("/dashboard/sites/{}", tenant.id)), "{location}");
}

#[tokio::test]
async fn the_console_lists_every_domain_and_filters_them() {
    let (state, _dir) = platform().await;
    let cookie = as_operator(&state, StaffRole::Owner).await;
    let (account, _) = as_customer(&state).await;
    let mine = seed_site(&state, &account.id, "adas-journal").await;
    someone_else(&state).await;
    pro_db::domains::add(&state.db, &mine.id, "blog.example.com", pro_core::DomainKind::Custom, false)
        .await
        .unwrap();

    let (status, all) = get(&state, "/owner/domains", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    for host in ["adas-journal.localhost", "theirs.localhost", "blog.example.com"] {
        assert!(all.contains(host), "every domain on the platform: {host}");
    }
    assert!(all.contains("owner@example.com") && all.contains("other@example.com"), "and whose");

    let (_, pending) = get(&state, "/owner/domains?status=pending", Some(&cookie)).await;
    assert!(pending.contains("blog.example.com"));
    assert!(!pending.contains("theirs.localhost"), "built-in addresses are never pending");

    let (_, found) = get(&state, "/owner/domains?q=other%40example", Some(&cookie)).await;
    assert!(found.contains("theirs.localhost"), "searchable by owner");
    assert!(!found.contains("blog.example.com"));
}

#[tokio::test]
async fn console_domain_controls_follow_the_staff_role() {
    let (state, _dir) = platform().await;
    let hash = pro_db::accounts::hash_password("a-long-enough-password").unwrap();
    let account = pro_db::accounts::create(&state.db, "cust@example.com", "A Customer", &hash)
        .await
        .unwrap();
    let tenant = seed_site(&state, &account.id, "somebodys-site").await;
    let custom =
        pro_db::domains::add(&state.db, &tenant.id, "blog.example.com", pro_core::DomainKind::Custom, false)
            .await
            .unwrap();
    let page = format!("/owner/domains/{}", custom.id);

    let support = as_operator(&state, StaffRole::Support).await;
    let (status, body) = get(&state, &page, Some(&support)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Verification token"), "the raw record, for debugging");
    assert!(body.contains("cust@example.com"), "and its owner");
    assert!(body.contains("Check now"), "support may re-check DNS");
    assert!(!body.contains(r#"/remove""#), "but not remove a customer's domain");

    // And the route refuses regardless of what the page showed.
    let (status, _, _) =
        send(&state, "POST", &format!("{page}/remove"), Some(&support), Some("")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(pro_db::domains::find_by_host(&state.db, "blog.example.com").await.unwrap().is_some());
}

#[tokio::test]
async fn an_admin_removes_a_domain_from_the_console() {
    let (state, _dir) = platform().await;
    let hash = pro_db::accounts::hash_password("a-long-enough-password").unwrap();
    let account = pro_db::accounts::create(&state.db, "cust@example.com", "A Customer", &hash)
        .await
        .unwrap();
    let tenant = seed_site(&state, &account.id, "somebodys-site").await;
    let custom =
        pro_db::domains::add(&state.db, &tenant.id, "blog.example.com", pro_core::DomainKind::Custom, false)
            .await
            .unwrap();
    let admin = as_operator(&state, StaffRole::Admin).await;

    let (status, body) = get(&state, &format!("/owner/domains/{}", custom.id), Some(&admin)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!("/owner/domains/{}/remove", custom.id)));

    let (status, headers, _) = send(
        &state,
        "POST",
        &format!("/owner/domains/{}/remove", custom.id),
        Some(&admin),
        Some(""),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers.get(header::LOCATION).unwrap().to_str().unwrap().starts_with("/owner/domains?"));
    assert!(pro_db::domains::find_by_host(&state.db, "blog.example.com").await.unwrap().is_none());

    // Recorded against the operator, so the customer's history says who.
    let events = pro_db::audit::list(&state.db, Some(&tenant.id), None, 10, 0).await.unwrap();
    assert!(events.iter().any(|e| e.action == "domain.removed" && e.actor.starts_with("staff:")));
}

/// The console's Updates screen: only an owner reaches it, Update now hands
/// the root script a request (with the token), a second click while one runs
/// is refused, and a finished build is rolled out and recorded.
#[tokio::test]
async fn an_owner_updates_vuuut_from_the_console() {
    let (state, _dir) = platform().await;
    let updates = state.config.data_dir().join("updates");

    let admin = as_operator(&state, StaffRole::Admin).await;
    let (status, _, _) = send(&state, "GET", "/owner/updates", Some(&admin), None).await;
    assert_ne!(status, StatusCode::OK, "an administrator does not reach the updater");
    let (_, headers, _) = send(&state, "POST", "/owner/updates/run", Some(&admin), Some("")).await;
    assert!(!updates.join("request.json").exists(), "nor can an administrator start one: {headers:?}");

    pro_db::staff::create(&state.db, "boss@example.com", "Ada", "another-very-long-password", StaffRole::Owner).await.unwrap();
    let (_, headers, _) = send(&state, "POST", "/owner/signin", None, Some("email=boss@example.com&password=another-very-long-password")).await;
    let owner = session_from(&headers);

    let (status, page) = get(&state, "/owner/updates", Some(&owner)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Update now") && page.contains("Not recorded"), "{page}");
    assert!(page.contains("anisulkibria/rust-ghost"), "the default repository is shown");

    let (status, headers, _) = send(&state, "POST", "/owner/updates/run", Some(&owner), Some("")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers[header::LOCATION].to_str().unwrap().contains("Update%20started"), "{headers:?}");
    let request: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(updates.join("request.json")).unwrap()).unwrap();
    assert_eq!(request["repository"], "anisulkibria/rust-ghost");
    assert_eq!(request["branch"], "main");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(updates.join("request.json")).unwrap().permissions().mode() & 0o777, 0o600, "it can carry a token");
    let st = pro_web::updates::status(&state);
    assert_eq!(st.state, "requested");
    assert_eq!(st.requested_by, "boss@example.com");

    // A second click while that one runs changes nothing.
    std::fs::remove_file(updates.join("request.json")).unwrap();
    let (_, headers, _) = send(&state, "POST", "/owner/updates/run", Some(&owner), Some("")).await;
    assert!(headers[header::LOCATION].to_str().unwrap().contains("already%20running"), "{headers:?}");
    assert!(!updates.join("request.json").exists());
    let (_, page) = get(&state, "/owner/updates", Some(&owner)).await;
    assert!(page.contains(r#"http-equiv="refresh""#) && !page.contains("Update now"), "a running update refreshes the page and holds the button");

    // The script reports a finished build; the worker rolls it out.
    let mut built = st.clone();
    built.state = "built".into();
    built.commit = "0123456789abcdef0123456789abcdef01234567".into();
    std::fs::write(updates.join("status.json"), serde_json::to_string(&built).unwrap()).unwrap();
    pro_web::updates::tick(&state).await.unwrap();
    let done = pro_web::updates::status(&state);
    assert_eq!(done.state, "done", "{}", done.message);
    assert!(!done.finished_at.is_empty());
    assert_eq!(pro_web::updates::installed(&state).commit, built.commit);
    let (_, page) = get(&state, "/owner/updates", Some(&owner)).await;
    assert!(page.contains("0123456") && page.contains("Update now") && !page.contains(r#"http-equiv="refresh""#), "{page}");

    // A request the root service never picks up is reported, not left spinning.
    let mut stale = done.clone();
    stale.state = "requested".into();
    stale.updated_at = "2020-01-01T00:00:00Z".into();
    std::fs::write(updates.join("status.json"), serde_json::to_string(&stale).unwrap()).unwrap();
    std::fs::write(updates.join("request.json"), "{}").unwrap();
    pro_web::updates::tick(&state).await.unwrap();
    let failed = pro_web::updates::status(&state);
    assert_eq!(failed.state, "failed");
    assert!(failed.message.contains("vuuut-update.path"), "{}", failed.message);
    assert!(!updates.join("request.json").exists(), "the stale request is withdrawn");
}
