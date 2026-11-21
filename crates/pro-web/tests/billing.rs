//! End-to-end tests for signed Polar webhooks and their effects on subscriptions and sites.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use pro_core::{Config, StaffRole, TenantStatus};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "pro.localhost:8080";
const SECRET: &str = "polar_whs_test";
const STARTER_PRODUCT: &str = "07521167-2b19-420a-9714-754d4b1d84ac";
const CREATOR_PRODUCT: &str = "0d023bce-1930-48c8-b6c9-f113c8619747";

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Platform {
    state: pro_web::AppState,
    #[allow(dead_code)]
    dir: tempdir::Dir,
}

/// A platform selling through Polar, with both plans priced.
async fn platform() -> Platform {
    build_platform(serde_json::json!({
        "provider": "polar",
        "dunningGraceDays": 14,
        "polar": {
            "accessToken": "polar_oat_test",
            "sandbox": true,
            "webhookSecret": SECRET,
        },
    }))
    .await
}

/// A platform told to take payment before a site opens, whose provider is
/// named but not configured — so no payment can actually be taken.
async fn platform_requiring_payment() -> Platform {
    build_platform(serde_json::json!({
        "provider": "stripe",
        "requirePaymentUpFront": true,
    }))
    .await
}

async fn build_platform(billing: serde_json::Value) -> Platform {
    let dir = tempdir::Dir::new();
    let mut config: Config = serde_json::from_value(serde_json::json!({
        "url": format!("http://{HOST}"),
        "subdomainBase": "localhost",
        "paths": { "ghostBinary": "/bin/sh", "dataDir": dir.path().to_str().unwrap() },
        "ssl": { "enabled": false },
        "mail": { "transport": "log" },
        "billing": billing,
    }))
    .unwrap();
    config.plans = pro_core::plans::default_plans();
    // The product IDs a real operator would set on /owner/plans. Put in the
    // file here so the fixture is one object rather than a settings write.
    config.plans[0].price_ids.insert("polar".into(), STARTER_PRODUCT.into());
    config.plans[1].price_ids.insert("polar".into(), CREATOR_PRODUCT.into());
    // Gumroad maps a plan to a product *permalink* rather than an id, and one
    // test posts a Gumroad ping to prove the form-encoded path works.
    config.plans[0].price_ids.insert("gumroad".into(), "starter-plan".into());
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
    Platform { state, dir }
}

impl Platform {
    /// An account with a site on the starter plan, and the tenant directory a
    /// plan change needs to be able to rewrite.
    async fn seed(&self) -> (pro_core::Account, pro_core::Tenant) {
        let state = &self.state;
        let hash = pro_db::accounts::hash_password("a-long-enough-password").unwrap();
        let account =
            pro_db::accounts::create(&state.db, "payer@example.com", "A Payer", &hash)
                .await
                .unwrap();
        pro_db::accounts::mark_verified(&state.db, &account.id).await.unwrap();

        let tenant = pro_db::tenants::create(
            &state.db,
            &account.id,
            "payers-site",
            "A Payer's Site",
            "starter",
            &pro_core::now_iso(),
            21_000,
            28_999,
        )
        .await
        .unwrap();
        pro_db::tenants::set_status(&state.db, &tenant.id, TenantStatus::Active)
            .await
            .unwrap();

        // `apply_plan` reads this file, edits `hostSettings` and writes it
        // back. Without it a plan change fails on a missing file rather than
        // on anything the test is about.
        let dir = state.config.tenant_dir(&tenant.id);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let body = serde_json::json!({
            "url": format!("http://payers-site.localhost"),
            "server": { "port": tenant.port },
            "hostSettings": {},
        });
        tokio::fs::write(
            dir.join(format!("config.{}.json", state.config.tenants.env)),
            serde_json::to_vec_pretty(&body).unwrap(),
        )
        .await
        .unwrap();

        (account, pro_db::tenants::get(&state.db, &tenant.id).await.unwrap())
    }

    /// Sign the seeded customer in through the real form, for the tests that
    /// go through the dashboard rather than a webhook.
    async fn sign_in(&self) -> String {
        let hash = pro_db::accounts::hash_password("a-long-enough-password").unwrap();
        let account =
            pro_db::accounts::create(&self.state.db, "buyer@example.com", "A Buyer", &hash)
                .await
                .unwrap();
        pro_db::accounts::mark_verified(&self.state.db, &account.id).await.unwrap();

        let (status, headers, _) = self
            .form("/dashboard/signin", "", "email=buyer@example.com&password=a-long-enough-password")
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "a good password signs in");
        headers
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .expect("a sign-in must set a session cookie")
            .to_string()
    }

    /// One form POST through the real router.
    async fn form(
        &self,
        path: &str,
        cookie: &str,
        body: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, HOST)
            .header("origin", format!("http://{HOST}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if !cookie.is_empty() {
            builder = builder.header(header::COOKIE, cookie);
        }
        let response = pro_web::entry(self.state.clone())
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, headers, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn post(&self, event: &Webhook) -> (StatusCode, String) {
        let body = serde_json::to_vec(&event.payload).unwrap();
        self.post_signed(&event.id, &body, &body, SECRET).await
    }

    async fn post_signed(
        &self, id: &str, body: &[u8], signed_body: &[u8], secret: &str,
    ) -> (StatusCode, String) {
        let timestamp = chrono::Utc::now().timestamp();
        self.post_raw(id, timestamp, body, &sign(id, timestamp, signed_body, secret)).await
    }

    async fn post_raw(
        &self, id: &str, timestamp: i64, body: &[u8], signature: &str,
    ) -> (StatusCode, String) {
        let request = Request::builder()
            .method("POST")
            .uri("/webhooks/polar")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .header("webhook-id", id)
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", signature)
            .body(Body::from(body.to_vec()))
            .unwrap();
        let response = pro_web::entry(self.state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn subscription(&self, provider_id: &str) -> Option<pro_core::Subscription> {
        pro_db::subscriptions::find_by_provider_id(&self.state.db, "polar", provider_id)
            .await
            .unwrap()
    }

    async fn tenant(&self, id: &str) -> pro_core::Tenant {
        pro_db::tenants::get(&self.state.db, id).await.unwrap()
    }
}

fn sign(id: &str, timestamp: i64, body: &[u8], secret: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{id}.{timestamp}.").as_bytes());
    mac.update(body);
    format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
}

struct Webhook {
    id: String,
    payload: serde_json::Value,
}

fn subscription_event(
    name: &str,
    webhook_id: &str,
    subscription_id: &str,
    product: &str,
    fields: serde_json::Value,
    checkout: serde_json::Value,
) -> Webhook {
    let mut data = serde_json::json!({
        "id": subscription_id,
        "status": "active",
        "customer_id": "55443",
        "customer": { "id": "55443", "external_id": checkout["account_id"] },
        "product_id": product,
        "cancel_at_period_end": false,
        "metadata": { "tenant_id": checkout["tenant_id"] },
        "modified_at": chrono::Utc::now().to_rfc3339(),
    });
    for (key, value) in fields.as_object().unwrap() {
        data[key] = value.clone();
    }
    Webhook {
        id: webhook_id.into(),
        payload: serde_json::json!({"type": name, "timestamp": pro_core::now_iso(), "data": data}),
    }
}

fn payment_event(webhook_id: &str, subscription_id: &str, status: &str) -> Webhook {
    subscription_event(
        "subscription.updated", webhook_id, subscription_id, STARTER_PRODUCT,
        serde_json::json!({ "status": status }), serde_json::json!({}),
    )
}

// ---------------------------------------------------------------------------
// Verification — the gate everything else sits behind
// ---------------------------------------------------------------------------

/// A forged event must not be able to grant a plan. This is the whole reason
/// the signature is checked before anything is parsed.
#[tokio::test]
async fn an_unsigned_or_wrongly_signed_event_is_refused() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let event = subscription_event(
        "subscription.created",
        "wh_1",
        "sub_1",
        CREATOR_PRODUCT,
        serde_json::json!({}),
        serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
    );
    let body = serde_json::to_vec(&event.payload).unwrap();

    // Signed with the wrong secret.
    let (status, _) = p.post_signed(&event.id, &body, &body, "not-the-secret").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Not signed at all.
    let (status, _) = p.post_raw(&event.id, chrono::Utc::now().timestamp(), &body, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A valid signature over *different* bytes, which is what a replay of one
    // event's signature onto another's body looks like.
    let (status, _) = p.post_signed(&event.id, &body, b"{}", SECRET).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Nothing was recorded, and the site did not move.
    assert!(p.subscription("sub_1").await.is_none());
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "starter");
}

/// The signature covers the exact bytes sent. A body that round-trips through
/// a parser comes out with different whitespace and key order, which is the
/// commonest way webhook verification is broken — so the check must be over
/// what arrived.
#[tokio::test]
async fn the_signature_is_over_the_raw_bytes() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let event = subscription_event(
        "subscription.created",
        "wh_raw",
        "sub_raw",
        STARTER_PRODUCT,
        serde_json::json!({}),
        serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
    );

    // Pretty-printed: same JSON, different bytes.
    let pretty = serde_json::to_vec_pretty(&event.payload).unwrap();
    let (status, _) = p.post_signed(&event.id, &pretty, &pretty, SECRET).await;
    assert_eq!(status, StatusCode::OK, "the bytes that were signed are the bytes that count");
    assert!(p.subscription("sub_raw").await.is_some());

    // And the compact spelling's signature does not verify the pretty one.
    let compact = serde_json::to_vec(&event.payload).unwrap();
    let (status, _) = p.post_signed(&event.id, &pretty, &compact, SECRET).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

/// A new subscription is stored against the account, and the site moves onto
/// the plan it now pays for.
#[tokio::test]
async fn a_new_subscription_entitles_the_site() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let renews = pro_core::days_from_now(30);

    let (status, body) = p
        .post(&subscription_event(
            "subscription.created",
            "wh_1",
            "sub_1",
            CREATOR_PRODUCT,
            serde_json::json!({ "current_period_end": renews }),
            serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "got {body}");

    let stored = p.subscription("sub_1").await.expect("the subscription was recorded");
    assert_eq!(stored.account_id, account.id);
    assert_eq!(stored.tenant_id.as_deref(), Some(tenant.id.as_str()));
    assert_eq!(stored.status, "active");
    assert_eq!(stored.plan_slug, "creator", "resolved from the product ID");
    assert_eq!(stored.current_period_end.as_deref(), Some(renews.as_str()));
    assert!(stored.is_entitled());

    // And the site itself, which is a separate claim from the subscription.
    let tenant = p.tenant(&tenant.id).await;
    assert_eq!(tenant.plan_slug, "creator", "the site moved onto the plan");
    assert_eq!(tenant.status, TenantStatus::Active);

    // The tenant's own config file carries the new plan's limits, which is
    // what the site reads on its next start.
    let written = tokio::fs::read_to_string(
        p.state
            .config
            .tenant_dir(&tenant.id)
            .join(format!("config.{}.json", p.state.config.tenants.env)),
    )
    .await
    .unwrap();
    assert!(written.contains("hostSettings"), "the limits were written");
    assert!(written.contains("limits"), "got {written}");
}

#[tokio::test]
async fn incomplete_checkout_cannot_open_a_site() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    pro_provision::suspend_site(
        &p.state.db, &p.state.config, &p.state.supervisor, &tenant,
        pro_core::suspension::AWAITING_PAYMENT,
    ).await.unwrap();

    for (id, status) in [("wh_pending", "incomplete"), ("wh_abandoned", "incomplete_expired")] {
        let result = p.post(&subscription_event(
            "subscription.created", id, "sub_pending", STARTER_PRODUCT,
            serde_json::json!({ "status": status }),
            serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
        )).await;
        assert_eq!(result.0, StatusCode::OK);
    }

    assert!(p.subscription("sub_pending").await.is_none());
    assert_eq!(p.tenant(&tenant.id).await.status, TenantStatus::Suspended);
}

#[tokio::test]
async fn an_older_snapshot_cannot_reactivate_a_revoked_subscription() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let checkout = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });
    let revoked = subscription_event(
        "subscription.updated", "wh_new", "sub_ordered", STARTER_PRODUCT,
        serde_json::json!({ "status": "canceled", "modified_at": "2026-11-21T01:00:01.012345Z" }),
        checkout.clone(),
    );
    let active = subscription_event(
        "subscription.created", "wh_old", "sub_ordered", STARTER_PRODUCT,
        serde_json::json!({ "status": "active", "modified_at": "2026-11-21T01:00:01.012344Z" }),
        checkout,
    );

    assert_eq!(p.post(&revoked).await.0, StatusCode::OK);
    let (status, response) = p.post(&active).await;
    assert_eq!(status, StatusCode::OK);
    assert!(response.contains("ignored older subscription update"), "{response}");
    assert_eq!(p.subscription("sub_ordered").await.unwrap().status, "canceled");
    assert_eq!(p.tenant(&tenant.id).await.status, TenantStatus::Suspended);
}

#[tokio::test]
async fn polar_signature_authenticates_delivery_id_and_timestamp() {
    let p = platform().await;
    let body = br#"{"type":"order.paid","data":{}}"#;
    let now = chrono::Utc::now().timestamp();
    let (status, _) = p.post_raw("other", now, body, &sign("original", now, body, SECRET)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let stale = now - pro_billing::signature::MAX_AGE_SECS - 1;
    let (status, _) = p.post_raw("original", stale, body, &sign("original", stale, body, SECRET)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A trial is entitled, and the dashboard needs the date it ends.
#[tokio::test]
async fn a_trial_is_entitled_and_dated() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let trial_ends = pro_core::days_from_now(14);

    let (status, _) = p
        .post(&subscription_event(
            "subscription.created",
            "wh_trial",
            "sub_trial",
            STARTER_PRODUCT,
            serde_json::json!({ "status": "trialing", "trial_end": trial_ends }),
            serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let stored = p.subscription("sub_trial").await.unwrap();
    assert_eq!(stored.status, "trialing");
    assert!(stored.is_trialing());
    assert!(stored.is_entitled(), "a trial runs the site");
    assert_eq!(stored.trial_ends_at.as_deref(), Some(trial_ends.as_str()));
}

// ---------------------------------------------------------------------------
// Idempotency — the defence against a retry granting a second subscription
// ---------------------------------------------------------------------------

/// Every provider retries. Acting twice on one event is how a customer gets
/// two subscriptions, or a plan change is applied to a site twice.
#[tokio::test]
async fn a_retried_event_is_applied_once() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let event = subscription_event(
        "subscription.created",
        "wh_dup",
        "sub_dup",
        CREATOR_PRODUCT,
        serde_json::json!({}),
        serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
    );

    let (first, body) = p.post(&event).await;
    assert_eq!(first, StatusCode::OK);
    assert!(body.contains("ok"), "got {body}");

    // The same delivery again, byte for byte.
    let (second, body) = p.post(&event).await;
    assert_eq!(second, StatusCode::OK, "a retry is acknowledged, not refused");
    assert!(body.contains("already_processed"), "got {body}");

    // One subscription, not two.
    let all = pro_db::subscriptions::list_for_account(&p.state.db, &account.id)
        .await
        .unwrap();
    assert_eq!(all.len(), 1, "a retry must not grant a second subscription");
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "creator");
}

/// Two genuinely different events must both be applied — idempotency keyed on
/// the event id, not on the subscription.
#[tokio::test]
async fn different_events_for_one_subscription_both_apply() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    let (status, _) = p
        .post(&subscription_event(
            "subscription.created",
            "wh_a",
            "sub_1",
            STARTER_PRODUCT,
            serde_json::json!({}),
            custom.clone(),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "starter");

    // An upgrade, a different webhook id.
    let (status, body) = p
        .post(&subscription_event(
            "subscription.updated",
            "wh_b",
            "sub_1",
            CREATOR_PRODUCT,
            serde_json::json!({}),
            custom,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("already_processed"), "got {body}");
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "creator", "the upgrade applied");
}

// ---------------------------------------------------------------------------
// Dunning, and the bug that made it one-way
// ---------------------------------------------------------------------------

/// A failed payment starts the clock rather than suspending at once: the
/// commonest cause is an expired card and the commonest fix takes a day.
#[tokio::test]
async fn a_failed_payment_starts_dunning_without_suspending() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription.created",
        "wh_1",
        "998877",
        STARTER_PRODUCT,
        serde_json::json!({}),
        custom,
    ))
    .await;

    let (status, _) = p.post(&payment_event("wh_2", "998877", "past_due")).await;
    assert_eq!(status, StatusCode::OK);

    let stored = p.subscription("998877").await.unwrap();
    assert_eq!(stored.status, "past_due");
    assert!(stored.past_due_since.is_some(), "the grace period is measured from here");
    assert!(stored.is_entitled(), "past_due is still entitled — that is what grace means");
    assert_eq!(
        p.tenant(&tenant.id).await.status,
        TenantStatus::Active,
        "the site stays up during the grace period"
    );
}

/// A successful retry updates the subscription and restores a suspended site.
#[tokio::test]
async fn a_recovered_payment_clears_dunning_and_restores_the_site() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription.created",
        "wh_1",
        "998877",
        STARTER_PRODUCT,
        serde_json::json!({}),
        custom,
    ))
    .await;
    p.post(&payment_event("wh_2", "998877", "past_due")).await;

    // The grace period ran out and the sweep suspended the site, which is the
    // state a customer is in when they finally pay.
    let t = p.tenant(&tenant.id).await;
    pro_provision::suspend_site(
        &p.state.db,
        &p.state.config,
        &p.state.supervisor,
        &t,
        "Payment outstanding",
    )
    .await
    .unwrap();
    assert_eq!(p.tenant(&tenant.id).await.status, TenantStatus::Suspended);

    // Now the payment comes good.
    let (status, body) = p
        .post(&payment_event("wh_3", "998877", "active"))
        .await;
    assert_eq!(status, StatusCode::OK, "got {body}");

    let stored = p.subscription("998877").await.unwrap();
    assert_eq!(stored.status, "active", "dunning is cleared");
    assert!(stored.is_entitled());
    assert_eq!(
        p.tenant(&tenant.id).await.status,
        TenantStatus::Active,
        "and the site is back — a customer who has just paid expects it now, \
         not on the next sweep"
    );
}

// ---------------------------------------------------------------------------
// Plan changes
// ---------------------------------------------------------------------------

/// A product change replaces the limits on the previously purchased plan.
#[tokio::test]
async fn a_plan_change_moves_the_site() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription.created",
        "wh_1",
        "sub_1",
        STARTER_PRODUCT,
        serde_json::json!({}),
        custom.clone(),
    ))
    .await;
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "starter");

    let (status, body) = p
        .post(&subscription_event(
            "subscription.updated",
            "wh_2",
            "sub_1",
            CREATOR_PRODUCT,
            serde_json::json!({}),
            custom,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("moved"), "the summary says what happened: {body}");

    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "creator");
    assert_eq!(p.subscription("sub_1").await.unwrap().plan_slug, "creator");
}

/// A downgrade is the same path, and must not be refused for taking the site
/// below what it currently uses — Ghost refuses *new* members rather than
/// deleting any, so the limits simply tighten.
#[tokio::test]
async fn a_downgrade_applies_too() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription.created",
        "wh_1",
        "sub_1",
        CREATOR_PRODUCT,
        serde_json::json!({}),
        custom.clone(),
    ))
    .await;
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "creator");

    p.post(&subscription_event(
        "subscription.updated",
        "wh_2",
        "sub_1",
        STARTER_PRODUCT,
        serde_json::json!({}),
        custom,
    ))
    .await;
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "starter");
}

// ---------------------------------------------------------------------------
// Ending a subscription
// ---------------------------------------------------------------------------

/// A scheduled cancellation keeps the paid subscription active.
#[tokio::test]
async fn a_cancellation_leaves_the_site_up_until_the_period_ends() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription.created",
        "wh_1",
        "sub_1",
        STARTER_PRODUCT,
        serde_json::json!({}),
        custom.clone(),
    ))
    .await;

    // Relative to now, because "the period is not over yet" is the whole
    // case — a literal date turns this into a test of the opposite once it
    // passes.
    let ends = pro_core::days_from_now(20);
    let (status, _) = p
        .post(&subscription_event(
            "subscription.canceled",
            "wh_2",
            "sub_1",
            STARTER_PRODUCT,
            serde_json::json!({
                "status": "active",
                "cancel_at_period_end": true,
                "current_period_end": ends,
            }),
            custom,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let stored = p.subscription("sub_1").await.unwrap();
    assert!(stored.cancel_at_period_end, "it will not renew");
    assert_eq!(stored.current_period_end.as_deref(), Some(ends.as_str()));
    assert!(stored.is_entitled(), "a cancelled-but-unexpired subscription still entitles");
    assert_eq!(
        p.tenant(&tenant.id).await.status,
        TenantStatus::Active,
        "but the site runs to the date the customer paid through"
    );
}

/// And when it actually expires, the site goes offline. Nothing is deleted —
/// that is what makes it recoverable by paying.
#[tokio::test]
async fn an_expired_subscription_suspends_the_site() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription.created",
        "wh_1",
        "sub_1",
        STARTER_PRODUCT,
        serde_json::json!({}),
        custom.clone(),
    ))
    .await;

    let (status, body) = p
        .post(&subscription_event(
            "subscription.revoked",
            "wh_2",
            "sub_1",
            STARTER_PRODUCT,
            serde_json::json!({ "status": "unpaid" }),
            custom,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("suspended"), "got {body}");

    let stored = p.subscription("sub_1").await.unwrap();
    assert_eq!(stored.status, "expired");
    assert!(!stored.is_entitled());

    let tenant = p.tenant(&tenant.id).await;
    assert_eq!(tenant.status, TenantStatus::Suspended);
    assert!(tenant.suspended_reason.is_some(), "and it says why");
}

/// Polar reports unpaid when it exhausts payment retries.
#[tokio::test]
async fn unpaid_is_terminal_at_this_provider() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription.created",
        "wh_1",
        "sub_1",
        STARTER_PRODUCT,
        serde_json::json!({}),
        custom.clone(),
    ))
    .await;
    p.post(&subscription_event(
        "subscription.updated",
        "wh_2",
        "sub_1",
        STARTER_PRODUCT,
        serde_json::json!({ "status": "unpaid" }),
        custom,
    ))
    .await;

    assert_eq!(p.subscription("sub_1").await.unwrap().status, "expired");
    assert_eq!(p.tenant(&tenant.id).await.status, TenantStatus::Suspended);
}

// ---------------------------------------------------------------------------
// Events that are not errors
// ---------------------------------------------------------------------------

/// A non-2xx makes the provider retry, and retrying something deliberately not
/// handled is a loop that never ends.
#[tokio::test]
async fn an_unhandled_event_is_acknowledged() {
    let p = platform().await;
    let event = Webhook {
        id: "wh_product".into(),
        payload: serde_json::json!({ "type": "product.created", "data": { "id": "product_1" } }),
    };
    let (status, body) = p.post(&event).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("ignored"), "got {body}");
}

/// An order event names an order, not a subscription. For a subscription
/// product the matching `subscription.created` arrives too, so this one has
/// nothing to do.
#[tokio::test]
async fn order_events_are_acknowledged_without_acting() {
    let p = platform().await;
    let (account, _) = p.seed().await;
    let event = Webhook {
        id: "wh_order".into(),
        payload: serde_json::json!({
            "type": "order.paid",
            "data": { "id": "ord_1", "status": "paid", "customer": { "external_id": account.id } }
        }),
    };
    let (status, body) = p.post(&event).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("ignored"), "got {body}");
    assert!(
        pro_db::subscriptions::list_for_account(&p.state.db, &account.id)
            .await
            .unwrap()
            .is_empty(),
        "an order alone entitles nothing"
    );
}

/// An event this platform cannot attribute is accepted and recorded, not
/// retried forever — and it has to be visible to an operator afterwards,
/// because somebody has paid and nothing has been granted.
#[tokio::test]
async fn an_unattributable_event_is_recorded_for_an_operator() {
    let p = platform().await;
    let event = subscription_event(
        "subscription.created",
        "wh_orphan",
        "sub_orphan",
        STARTER_PRODUCT,
        serde_json::json!({}),
        // No external account ID, and a customer ID this platform has never seen.
        serde_json::json!({}),
    );
    let (status, body) = p.post(&event).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "accepted, so Polar stops resending: {body}"
    );
    assert!(body.contains("accepted_with_error"), "got {body}");

    // And it shows up on the console's Activity screen.
    let failed = pro_db::audit::failed_webhooks(&p.state.db, 50).await.unwrap();
    assert_eq!(failed.len(), 1, "an operator has to be able to see this");
    let (provider, event_id, event_type, error) = &failed[0];
    assert_eq!(provider, "polar");
    assert_eq!(event_id, "wh_orphan");
    assert_eq!(event_type, "subscription.created");
    assert!(!error.is_empty(), "with a reason");
}

/// The console's replay button has to be able to finish the job once an
/// operator has fixed whatever made the event unattributable.
#[tokio::test]
async fn an_operator_can_replay_a_failed_event() {
    let p = platform().await;

    // Arrives before the account exists, which is how a webhook overtakes a
    // signup that is still in flight.
    let event = subscription_event(
        "subscription.created",
        "wh_replay",
        "sub_replay",
        CREATOR_PRODUCT,
        serde_json::json!({}),
        serde_json::json!({ "account_id": "acc_not_yet", "tenant_id": "ten_not_yet" }),
    );
    let (status, body) = p.post(&event).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("accepted_with_error"), "got {body}");
    assert!(p.subscription("sub_replay").await.is_none());

    // The operator finds the account and retries. The stored body is replayed
    // verbatim — the signature is not re-checked, because these are the same
    // bytes that passed it on arrival.
    let (account, tenant) = p.seed().await;
    let event = subscription_event(
        "subscription.created",
        "wh_replay2",
        "sub_replay",
        CREATOR_PRODUCT,
        serde_json::json!({}),
        serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
    );
    let (status, _) = p.post(&event).await;
    assert_eq!(status, StatusCode::OK);

    let outcome = pro_web::webhooks::replay_event(&p.state, "polar", "wh_replay2")
        .await
        .unwrap();
    assert!(!outcome.is_empty(), "the operator is told what the replay did");
    assert_eq!(p.subscription("sub_replay").await.unwrap().plan_slug, "creator");
}

/// A provider this platform has never been configured for must not be able to
/// post events at all.
#[tokio::test]
async fn an_unconfigured_provider_is_refused() {
    let p = platform().await;
    let request = Request::builder()
        .method("POST")
        .uri("/webhooks/stripe")
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = pro_web::entry(p.state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Gumroad posts form-encoded bodies and signs nothing, so its ping takes a
/// different route through the same pipeline: the token in the URL is the
/// authentication, and `parse_body` is what reads the body at all.
#[tokio::test]
async fn a_gumroad_ping_needs_its_token() {
    let p = platform().await;
    // Configure Gumroad alongside Polar — webhooks are accepted from
    // every configured provider, not only the active one.
    p.state
        .settings
        .save(
            &p.state.db,
            &[
                ("billing.gumroad.accessToken".into(), Some(serde_json::json!("gr_token"))),
                ("billing.gumroad.pingToken".into(), Some(serde_json::json!("ping_secret"))),
            ],
            StaffRole::Owner,
            "test",
        )
        .await
        .unwrap();

    let (account, tenant) = p.seed().await;
    let body = format!(
        "sale_id=s_1&subscription_id=gsub_1&product_permalink=starter-plan\
         &url_params%5Baccount_id%5D={}&url_params%5Btenant_id%5D={}\
         &sale_timestamp=2026-01-01T10:00:00Z",
        account.id, tenant.id
    );

    let post = |query: &str, body: String| {
        let state = p.state.clone();
        let uri = format!("/webhooks/gumroad{query}");
        async move {
            let request = Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::HOST, HOST)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap();
            let response = pro_web::entry(state).oneshot(request).await.unwrap();
            response.status()
        }
    };

    // No token: refused, because nothing else can authenticate it.
    assert_eq!(post("", body.clone()).await, StatusCode::FORBIDDEN);
    // Wrong token: refused.
    assert_eq!(post("?token=nope", body.clone()).await, StatusCode::FORBIDDEN);
    assert!(
        pro_db::subscriptions::find_by_provider_id(&p.state.db, "gumroad", "gsub_1")
            .await
            .unwrap()
            .is_none(),
        "nothing was granted"
    );

    // The right token, and the form-encoded body is understood.
    assert_eq!(post("?token=ping_secret", body).await, StatusCode::OK);
    let stored = pro_db::subscriptions::find_by_provider_id(&p.state.db, "gumroad", "gsub_1")
        .await
        .unwrap()
        .expect("the ping was applied");
    assert_eq!(stored.account_id, account.id);
    assert_eq!(stored.status, "active");
    assert_eq!(stored.plan_slug, "starter", "resolved from the product permalink");
}

// ---------------------------------------------------------------------------
// Payment before the site opens
// ---------------------------------------------------------------------------

/// `billing.requirePaymentUpFront` promises the site does not open until it is
/// paid for. It used to promise only that: the flag was read in three places
/// to decide whether to grant a *trial*, and site creation never consulted it
/// at all, so a customer got a working site and a bill that never came.
///
/// The site is reserved and suspended rather than deferred — the address has
/// to be claimed when it is chosen — so what this asserts is that it is
/// *closed*, and closed for a reason the payment pipeline recognises.
#[tokio::test]
async fn a_new_site_is_closed_until_the_payment_lands() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;

    // The state site creation leaves behind on a payment-first platform.
    pro_provision::suspend_site(
        &p.state.db,
        &p.state.config,
        &p.state.supervisor,
        &tenant,
        pro_core::suspension::AWAITING_PAYMENT,
    )
    .await
    .unwrap();
    let closed = p.tenant(&tenant.id).await;
    assert_eq!(closed.status, TenantStatus::Suspended);
    assert!(closed.is_suspended_for_billing(), "the payment pipeline must recognise it");

    // The active subscription opens the site once checkout completes.
    let (status, body) = p
        .post(&subscription_event(
            "subscription.created",
            "wh_1",
            "778899",
            STARTER_PRODUCT,
            serde_json::json!({}),
            serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "got {body}");

    let stored = p.subscription("778899").await.unwrap();
    assert!(stored.is_entitled(), "the subscription entitles the site");

    let open = p.tenant(&tenant.id).await;
    assert_eq!(
        open.status,
        TenantStatus::Active,
        "the payment landed, so the site opens"
    );
    assert_eq!(open.suspended_reason, None, "and the reason is cleared with it");
}

/// The other half of the same rule, and the one with teeth: a site an operator
/// shut must not reopen because a card was charged. Before the suspension
/// reason was consulted, any payment on any subscription reopened whatever
/// suspended site it pointed at — so a customer suspended for hosting phishing
/// pages could bring their site back by paying.
#[tokio::test]
async fn a_payment_does_not_reopen_a_site_an_operator_shut() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription.created",
        "wh_1",
        "665544",
        STARTER_PRODUCT,
        serde_json::json!({}),
        custom.clone(),
    ))
    .await;

    // An operator's own reason, not one of billing's.
    let t = p.tenant(&tenant.id).await;
    pro_provision::suspend_site(
        &p.state.db,
        &p.state.config,
        &p.state.supervisor,
        &t,
        "Abuse — phishing pages",
    )
    .await
    .unwrap();

    // Every way a payment can arrive.
    p.post(&payment_event("wh_2", "665544", "active")).await;
    p.post(&payment_event("wh_3", "665544", "active")).await;
    p.post(&subscription_event(
        "subscription.updated",
        "wh_4",
        "665544",
        STARTER_PRODUCT,
        serde_json::json!({ "status": "active" }),
        custom,
    ))
    .await;

    let still = p.tenant(&tenant.id).await;
    assert_eq!(
        still.status,
        TenantStatus::Suspended,
        "an operator's suspension is not something a customer can pay their way out of"
    );
    assert_eq!(still.suspended_reason.as_deref(), Some("Abuse — phishing pages"));
}

/// A platform told to take payment first, that cannot take a payment, must
/// refuse to build the site rather than build one nobody can ever pay for.
///
/// The ordering is the assertion: the provider is resolved before the tenant
/// row exists, so a misconfigured platform leaves nothing behind.
#[tokio::test]
async fn payment_first_without_a_usable_provider_builds_nothing() {
    let p = platform_requiring_payment().await;
    let cookie = p.sign_in().await;

    let (status, _, _) = p
        .form(
            "/dashboard/new",
            &cookie,
            "title=Adas+Journal&slug=adas-journal&plan=starter\
             &owner_name=Ada&owner_email=ada@example.com\
             &owner_password=a-long-enough-password",
        )
        .await;
    // An operator's mistake, not the customer's, so it is an error page rather
    // than the form again with something they could usefully change.
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "refused");

    assert!(
        pro_db::tenants::find_by_slug(&p.state.db, "adas-journal")
            .await
            .unwrap()
            .is_none(),
        "nothing may be built when the payment it waits for cannot be taken"
    );
}

/// A tiny temporary directory, so these tests leave nothing behind.
mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("pro-billing-test-{unique}"));
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
