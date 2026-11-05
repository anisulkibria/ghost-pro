//! End-to-end tests for the billing webhook pipeline, through Lemon Squeezy.
//!
//! This is the path that decides whether a paying customer's site stays up,
//! and it had no test at all. Every case here goes through the real HTTP
//! route: the body is signed the way Lemon Squeezy signs it, posted to
//! `/webhooks/lemonsqueezy`, and the assertions are about what ended up in
//! the database and what happened to the tenant — not about what `interpret`
//! returned.
//!
//! Lemon Squeezy specifically because it is the provider this platform sells
//! through. The provider-neutral half of the pipeline — claiming an event id,
//! resolving an account, applying a plan, suspending a site — is the same for
//! all four, so exercising it once here covers it for all of them.
//!
//! Three things make the real route worth testing rather than just the
//! translation:
//!
//! * the signature is verified over the **raw bytes**, so anything that
//!   re-serialises the body breaks it, and only an end-to-end test notices;
//! * the event id is claimed **before** acting, which is the whole of the
//!   defence against a retry granting a second subscription;
//! * applying a plan writes the tenant's config file and may restart it, so
//!   "the subscription moved" and "the site moved" are different claims.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use pro_core::{Config, StaffRole, TenantStatus};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "pro.localhost:8080";
const SECRET: &str = "ls_webhook_secret";
const STARTER_VARIANT: &str = "812345";
const CREATOR_VARIANT: &str = "812346";

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Platform {
    state: pro_web::AppState,
    #[allow(dead_code)]
    dir: tempdir::Dir,
}

/// A platform selling through Lemon Squeezy, with both plans priced.
async fn platform() -> Platform {
    let dir = tempdir::Dir::new();
    let mut config: Config = serde_json::from_value(serde_json::json!({
        "url": format!("http://{HOST}"),
        "subdomainBase": "localhost",
        "paths": { "ghostBinary": "/bin/sh", "dataDir": dir.path().to_str().unwrap() },
        "ssl": { "enabled": false },
        "mail": { "transport": "log" },
        "billing": {
            "provider": "lemonsqueezy",
            "dunningGraceDays": 14,
            "lemonsqueezy": {
                "apiKey": "ls_api_key",
                "storeId": "4242",
                "webhookSecret": SECRET,
            },
        },
    }))
    .unwrap();
    config.plans = pro_core::plans::default_plans();
    // The variant ids a real operator would set on /console/plans. Put in the
    // file here so the fixture is one object rather than a settings write.
    config.plans[0].price_ids.insert("lemonsqueezy".into(), STARTER_VARIANT.into());
    config.plans[1].price_ids.insert("lemonsqueezy".into(), CREATOR_VARIANT.into());
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

    /// Post a signed Lemon Squeezy event, as Lemon Squeezy would.
    async fn post(&self, event: &serde_json::Value) -> (StatusCode, String) {
        let body = serde_json::to_vec(event).unwrap();
        self.post_raw(&body, &sign(&body, SECRET)).await
    }

    /// Post with an arbitrary signature, for the verification cases.
    async fn post_raw(&self, body: &[u8], signature: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .method("POST")
            .uri("/webhooks/lemonsqueezy")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-signature", signature)
            .body(Body::from(body.to_vec()))
            .unwrap();
        let response = pro_web::entry(self.state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn subscription(&self, provider_id: &str) -> Option<pro_core::Subscription> {
        pro_db::subscriptions::find_by_provider_id(&self.state.db, "lemonsqueezy", provider_id)
            .await
            .unwrap()
    }

    async fn tenant(&self, id: &str) -> pro_core::Tenant {
        pro_db::tenants::get(&self.state.db, id).await.unwrap()
    }
}

/// Lemon Squeezy's `X-Signature`: a hex HMAC-SHA256 of the raw body.
fn sign(body: &[u8], secret: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// A subscription event in Lemon Squeezy's JSON:API shape.
fn subscription_event(
    name: &str,
    webhook_id: &str,
    subscription_id: &str,
    variant: &str,
    attrs: serde_json::Value,
    custom: serde_json::Value,
) -> serde_json::Value {
    let mut attributes = serde_json::json!({
        "status": "active",
        "customer_id": 55443,
        "variant_id": variant.parse::<i64>().unwrap(),
        "cancelled": false,
    });
    if let (Some(base), Some(extra)) = (attributes.as_object_mut(), attrs.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    serde_json::json!({
        "meta": { "event_name": name, "webhook_id": webhook_id, "custom_data": custom },
        "data": { "type": "subscriptions", "id": subscription_id, "attributes": attributes }
    })
}

/// A payment event, which names its subscription in the attributes.
fn payment_event(name: &str, webhook_id: &str, subscription_id: i64) -> serde_json::Value {
    serde_json::json!({
        "meta": { "event_name": name, "webhook_id": webhook_id, "custom_data": {} },
        "data": { "type": "subscription-invoices", "id": "inv_1",
                  "attributes": { "subscription_id": subscription_id } }
    })
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
        "subscription_created",
        "wh_1",
        "sub_1",
        CREATOR_VARIANT,
        serde_json::json!({}),
        serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
    );
    let body = serde_json::to_vec(&event).unwrap();

    // Signed with the wrong secret.
    let (status, _) = p.post_raw(&body, &sign(&body, "not-the-secret")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Not signed at all.
    let (status, _) = p.post_raw(&body, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A valid signature over *different* bytes, which is what a replay of one
    // event's signature onto another's body looks like.
    let (status, _) = p.post_raw(&body, &sign(b"{}", SECRET)).await;
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
        "subscription_created",
        "wh_raw",
        "sub_raw",
        STARTER_VARIANT,
        serde_json::json!({}),
        serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
    );

    // Pretty-printed: same JSON, different bytes.
    let pretty = serde_json::to_vec_pretty(&event).unwrap();
    let (status, _) = p.post_raw(&pretty, &sign(&pretty, SECRET)).await;
    assert_eq!(status, StatusCode::OK, "the bytes that were signed are the bytes that count");
    assert!(p.subscription("sub_raw").await.is_some());

    // And the compact spelling's signature does not verify the pretty one.
    let compact = serde_json::to_vec(&event).unwrap();
    let (status, _) = p.post_raw(&pretty, &sign(&compact, SECRET)).await;
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
            "subscription_created",
            "wh_1",
            "sub_1",
            CREATOR_VARIANT,
            serde_json::json!({ "renews_at": renews }),
            serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "got {body}");

    let stored = p.subscription("sub_1").await.expect("the subscription was recorded");
    assert_eq!(stored.account_id, account.id);
    assert_eq!(stored.tenant_id.as_deref(), Some(tenant.id.as_str()));
    assert_eq!(stored.status, "active");
    assert_eq!(stored.plan_slug, "creator", "resolved from the variant id");
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

/// A trial is entitled, and the dashboard needs the date it ends.
#[tokio::test]
async fn a_trial_is_entitled_and_dated() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let trial_ends = pro_core::days_from_now(14);

    let (status, _) = p
        .post(&subscription_event(
            "subscription_created",
            "wh_trial",
            "sub_trial",
            STARTER_VARIANT,
            serde_json::json!({ "status": "on_trial", "trial_ends_at": trial_ends }),
            serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let stored = p.subscription("sub_trial").await.unwrap();
    assert_eq!(stored.status, "trialing", "on_trial is this provider's word for it");
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
        "subscription_created",
        "wh_dup",
        "sub_dup",
        CREATOR_VARIANT,
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
            "subscription_created",
            "wh_a",
            "sub_1",
            STARTER_VARIANT,
            serde_json::json!({}),
            custom.clone(),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "starter");

    // An upgrade, a different webhook id.
    let (status, body) = p
        .post(&subscription_event(
            "subscription_updated",
            "wh_b",
            "sub_1",
            CREATOR_VARIANT,
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
        "subscription_created",
        "wh_1",
        "998877",
        STARTER_VARIANT,
        serde_json::json!({}),
        custom,
    ))
    .await;

    let (status, _) = p.post(&payment_event("subscription_payment_failed", "wh_2", 998877)).await;
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

/// The regression guard for the worst bug this integration had.
///
/// `subscription_payment_recovered` is Lemon Squeezy's word for a previously
/// failed payment finally going through. It was unhandled, so a customer who
/// paid stayed `past_due` and the housekeeping sweep suspended their site at
/// the end of the grace period — after they had paid.
#[tokio::test]
async fn a_recovered_payment_clears_dunning_and_restores_the_site() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription_created",
        "wh_1",
        "998877",
        STARTER_VARIANT,
        serde_json::json!({}),
        custom,
    ))
    .await;
    p.post(&payment_event("subscription_payment_failed", "wh_2", 998877)).await;

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
        .post(&payment_event("subscription_payment_recovered", "wh_3", 998877))
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

/// A refund is money going back out, so it enters dunning like a decline.
#[tokio::test]
async fn a_refunded_payment_enters_dunning() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    p.post(&subscription_event(
        "subscription_created",
        "wh_1",
        "998877",
        STARTER_VARIANT,
        serde_json::json!({}),
        serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
    ))
    .await;

    let (status, _) =
        p.post(&payment_event("subscription_payment_refunded", "wh_2", 998877)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(p.subscription("998877").await.unwrap().status, "past_due");
}

// ---------------------------------------------------------------------------
// Plan changes
// ---------------------------------------------------------------------------

/// The other bug: an upgrade arrives as `subscription_plan_changed`, and while
/// it was ignored the customer paid for Creator and kept Starter's limits.
#[tokio::test]
async fn a_plan_change_moves_the_site() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription_created",
        "wh_1",
        "sub_1",
        STARTER_VARIANT,
        serde_json::json!({}),
        custom.clone(),
    ))
    .await;
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "starter");

    let (status, body) = p
        .post(&subscription_event(
            "subscription_plan_changed",
            "wh_2",
            "sub_1",
            CREATOR_VARIANT,
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
        "subscription_created",
        "wh_1",
        "sub_1",
        CREATOR_VARIANT,
        serde_json::json!({}),
        custom.clone(),
    ))
    .await;
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "creator");

    p.post(&subscription_event(
        "subscription_updated",
        "wh_2",
        "sub_1",
        STARTER_VARIANT,
        serde_json::json!({}),
        custom,
    ))
    .await;
    assert_eq!(p.tenant(&tenant.id).await.plan_slug, "starter");
}

// ---------------------------------------------------------------------------
// Ending a subscription
// ---------------------------------------------------------------------------

/// Lemon Squeezy's `cancelled` means "will not renew", with the site live
/// until `ends_at`. Reading it as immediate would cut a paying customer off
/// early — they have paid for the rest of the period.
#[tokio::test]
async fn a_cancellation_leaves_the_site_up_until_the_period_ends() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription_created",
        "wh_1",
        "sub_1",
        STARTER_VARIANT,
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
            "subscription_cancelled",
            "wh_2",
            "sub_1",
            STARTER_VARIANT,
            serde_json::json!({
                "status": "cancelled",
                "cancelled": true,
                "ends_at": ends,
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
        "subscription_created",
        "wh_1",
        "sub_1",
        STARTER_VARIANT,
        serde_json::json!({}),
        custom.clone(),
    ))
    .await;

    let (status, body) = p
        .post(&subscription_event(
            "subscription_expired",
            "wh_2",
            "sub_1",
            STARTER_VARIANT,
            serde_json::json!({ "status": "expired" }),
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

/// `unpaid` means opposite things at Stripe and Lemon Squeezy: Stripe is still
/// dunning, Lemon Squeezy has given up. Mapping both to one status would
/// either suspend early or never suspend at all.
#[tokio::test]
async fn unpaid_is_terminal_at_this_provider() {
    let p = platform().await;
    let (account, tenant) = p.seed().await;
    let custom = serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id });

    p.post(&subscription_event(
        "subscription_created",
        "wh_1",
        "sub_1",
        STARTER_VARIANT,
        serde_json::json!({}),
        custom.clone(),
    ))
    .await;
    p.post(&subscription_event(
        "subscription_updated",
        "wh_2",
        "sub_1",
        STARTER_VARIANT,
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
    let event = serde_json::json!({
        "meta": { "event_name": "license_key_created", "webhook_id": "wh_lic" },
        "data": { "id": "lic_1", "attributes": {} }
    });
    let (status, body) = p.post(&event).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("ignored"), "got {body}");
}

/// An order event names an order, not a subscription. For a subscription
/// product the matching `subscription_created` arrives too, so this one has
/// nothing to do.
#[tokio::test]
async fn order_events_are_acknowledged_without_acting() {
    let p = platform().await;
    let (account, _) = p.seed().await;
    let event = serde_json::json!({
        "meta": { "event_name": "order_created", "webhook_id": "wh_order",
                  "custom_data": { "account_id": account.id } },
        "data": { "id": "ord_1", "attributes": { "status": "paid" } }
    });
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
        "subscription_created",
        "wh_orphan",
        "sub_orphan",
        STARTER_VARIANT,
        serde_json::json!({}),
        // No account_id, and a customer id this platform has never seen.
        serde_json::json!({}),
    );
    let (status, body) = p.post(&event).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "accepted, so Lemon Squeezy stops resending: {body}"
    );
    assert!(body.contains("accepted_with_error"), "got {body}");

    // And it shows up on the console's Activity screen.
    let failed = pro_db::audit::failed_webhooks(&p.state.db, 50).await.unwrap();
    assert_eq!(failed.len(), 1, "an operator has to be able to see this");
    let (provider, event_id, event_type, error) = &failed[0];
    assert_eq!(provider, "lemonsqueezy");
    assert_eq!(event_id, "wh_orphan");
    assert_eq!(event_type, "subscription_created");
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
        "subscription_created",
        "wh_replay",
        "sub_replay",
        CREATOR_VARIANT,
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
        "subscription_created",
        "wh_replay2",
        "sub_replay",
        CREATOR_VARIANT,
        serde_json::json!({}),
        serde_json::json!({ "account_id": account.id, "tenant_id": tenant.id }),
    );
    let (status, _) = p.post(&event).await;
    assert_eq!(status, StatusCode::OK);

    let outcome = pro_web::webhooks::replay_event(&p.state, "lemonsqueezy", "wh_replay2")
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
    // Configure Gumroad alongside Lemon Squeezy — webhooks are accepted from
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
