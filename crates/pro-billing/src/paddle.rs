//! Paddle Billing (v2).
//!
//! Paddle is a merchant of record, which is why an operator might choose it:
//! it handles sales tax and VAT itself. The API is JSON rather than
//! form-encoded, every object id is prefixed (`sub_`, `ctm_`, `pri_`), and the
//! webhook envelope is `{event_id, event_type, data: {...}}` — flatter than
//! Stripe's `data.object`.
//!
//! Classic Paddle (the pre-Billing product) is a different API with different
//! signing, and is not supported: its keys will fail here, which is why the
//! settings help text says so.
use pro_core::{Plan, ProError, Result, Subscription};
use serde_json::{json, Value};

use crate::{
    signature, BillingEvent, Checkout, CheckoutRequest, Provider, SubscriptionState,
    WebhookHeaders,
};

pub struct Paddle {
    api_key: String,
    webhook_secret: Option<String>,
    sandbox: bool,
}

impl Paddle {
    pub fn new(api_key: String, webhook_secret: Option<String>, sandbox: bool) -> Self {
        Self { api_key, webhook_secret, sandbox }
    }

    fn api(&self) -> &'static str {
        if self.sandbox {
            "https://sandbox-api.paddle.com"
        } else {
            "https://api.paddle.com"
        }
    }

    async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(ProError::internal)?;

        let mut builder = client
            .request(method, format!("{}{path}", self.api()))
            .bearer_auth(&self.api_key)
            .header("Paddle-Version", "1");
        if let Some(body) = body {
            builder = builder.json(&body);
        }

        let response = builder
            .send()
            .await
            .map_err(|e| ProError::Unavailable(format!("could not reach Paddle: {e}")))?;

        let status = response.status();
        let parsed: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(parsed);
        }
        // Paddle returns `{error: {detail, errors: [...]}}`; the detail is the
        // readable part and is what an operator needs.
        let message = parsed
            .pointer("/error/detail")
            .and_then(Value::as_str)
            .unwrap_or("Paddle rejected the request");
        Err(ProError::BadRequest(format!("Paddle: {message}")))
    }
}

/// Paddle sends RFC-3339 already; this only normalises it to the exact form
/// the control plane stores, so string comparisons on dates stay valid.
fn normalise_date(value: Option<&Value>) -> Option<String> {
    let raw = value?.as_str()?;
    pro_core::parse_iso(raw)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .or_else(|| Some(raw.to_string()))
}

#[async_trait::async_trait]
impl Provider for Paddle {
    fn name(&self) -> &'static str {
        "paddle"
    }

    async fn create_checkout(&self, req: CheckoutRequest<'_>) -> Result<Checkout> {
        let price = req.plan.price_id("paddle").ok_or_else(|| {
            ProError::Config(format!(
                "plan {:?} has no Paddle price id — set plans[].priceIds.paddle",
                req.plan.slug
            ))
        })?;

        // A transaction with a checkout url, which is Paddle Billing's
        // server-side equivalent of a Stripe Checkout Session. `custom_data`
        // travels onto the subscription, so later events can be matched to an
        // account.
        let mut custom = json!({
            "account_id": req.account_id,
            "plan_slug": req.plan.slug,
        });
        if let Some(tenant) = req.tenant_id {
            custom["tenant_id"] = json!(tenant);
        }

        let body = json!({
            "items": [ { "price_id": price, "quantity": 1 } ],
            "customer": { "email": req.account_email },
            "custom_data": custom,
            "checkout": { "url": req.success_url },
            "collection_mode": "automatic",
        });

        let response = self.request(reqwest::Method::POST, "/transactions", Some(body)).await?;
        let url = response
            .pointer("/data/checkout/url")
            .and_then(Value::as_str)
            .ok_or_else(|| ProError::Unavailable("Paddle returned no checkout url".into()))?;
        Ok(Checkout { url: url.to_string() })
    }

    fn verify_webhook(&self, body: &[u8], headers: &WebhookHeaders) -> Result<()> {
        let header = headers
            .paddle_signature
            .as_deref()
            .ok_or_else(|| ProError::BadRequest("missing Paddle-Signature header".into()))?;
        signature::verify_paddle(body, header, self.webhook_secret.as_deref().unwrap_or(""))
    }

    fn event_id(&self, body: &Value) -> String {
        body["event_id"].as_str().unwrap_or_default().to_string()
    }

    fn interpret(&self, body: &Value, catalogue: &[Plan]) -> Result<BillingEvent> {
        let event_type = body["event_type"].as_str().unwrap_or_default().to_string();
        let data = &body["data"];

        match event_type.as_str() {
            "subscription.created" | "subscription.updated" | "subscription.activated"
            | "subscription.canceled" | "subscription.paused" | "subscription.resumed"
            | "subscription.trialing" => {
                let raw_status = data["status"].as_str().unwrap_or_default();
                // As with Stripe, the event type wins over a status field that
                // may lag behind it.
                let status = match event_type.as_str() {
                    "subscription.canceled" => "canceled",
                    "subscription.paused" => "paused",
                    _ => crate::normalise_status("paddle", raw_status).ok_or_else(|| {
                        ProError::Internal(format!(
                            "Paddle sent subscription status {raw_status:?}, which this \
                             version does not map"
                        ))
                    })?,
                };

                let price = data
                    .pointer("/items/0/price/id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let plan_slug = crate::plan_for_price(catalogue, "paddle", price)
                    .map(|p| p.slug.clone())
                    .or_else(|| data["custom_data"]["plan_slug"].as_str().map(str::to_string));

                // Paddle expresses "will not renew" as a scheduled change
                // rather than a boolean, so it has to be read out of that.
                let cancel_at_period_end = data
                    .pointer("/scheduled_change/action")
                    .and_then(Value::as_str)
                    .map(|action| action == "cancel");

                Ok(BillingEvent::SubscriptionChanged(SubscriptionState {
                    provider_subscription_id: data["id"].as_str().unwrap_or_default().to_string(),
                    provider_customer_id: data["customer_id"].as_str().map(str::to_string),
                    status: status.to_string(),
                    plan_slug,
                    current_period_start: normalise_date(
                        data.pointer("/current_billing_period/starts_at"),
                    ),
                    current_period_end: normalise_date(
                        data.pointer("/current_billing_period/ends_at"),
                    ),
                    trial_ends_at: normalise_date(data.pointer("/items/0/trial_dates/ends_at")),
                    cancel_at_period_end,
                    canceled_at: normalise_date(data.get("canceled_at")),
                    account_id: data["custom_data"]["account_id"].as_str().map(str::to_string),
                    tenant_id: data["custom_data"]["tenant_id"].as_str().map(str::to_string),
                }))
            }

            // Paddle reports payment outcomes on the transaction, which
            // carries the subscription it belongs to.
            "transaction.payment_failed" => {
                let Some(id) = data["subscription_id"].as_str() else {
                    return Ok(BillingEvent::Ignored { event_type });
                };
                Ok(BillingEvent::PaymentFailed { provider_subscription_id: id.to_string() })
            }
            "transaction.completed" | "transaction.paid" => {
                let Some(id) = data["subscription_id"].as_str() else {
                    // A one-off purchase, not a subscription renewal.
                    return Ok(BillingEvent::Ignored { event_type });
                };
                Ok(BillingEvent::PaymentSucceeded { provider_subscription_id: id.to_string() })
            }

            _ => Ok(BillingEvent::Ignored { event_type }),
        }
    }

    async fn management_url(&self, subscription: &Subscription) -> Result<Option<String>> {
        let Some(id) = &subscription.provider_subscription_id else {
            return Ok(None);
        };
        // Paddle exposes the update-payment-method transaction on the
        // subscription itself.
        let response = self
            .request(reqwest::Method::GET, &format!("/subscriptions/{id}"), None)
            .await?;
        Ok(response
            .pointer("/data/management_urls/update_payment_method")
            .and_then(Value::as_str)
            .or_else(|| response.pointer("/data/management_urls/cancel").and_then(Value::as_str))
            .map(str::to_string))
    }

    async fn cancel_subscription(&self, subscription: &Subscription) -> Result<bool> {
        let Some(id) = &subscription.provider_subscription_id else {
            return Err(ProError::BadRequest(
                "this subscription has no Paddle id".into(),
            ));
        };
        // Paddle Billing's scheduled change is the period-end form: the
        // subscription stays live until `next_billed_at`, then ends and says
        // so in a webhook. An immediate `POST …/cancel` exists too, and is
        // what the platform falls back to if the schedule is refused.
        let body = serde_json::json!({
            "scheduled_change": {"action": "cancel"}
        });
        match self
            .request(
                reqwest::Method::PATCH,
                &format!("/subscriptions/{id}"),
                Some(body),
            )
            .await
        {
            Ok(response) => {
                let scheduled = response
                    .pointer("/data/scheduled_change/action")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if scheduled == "cancel" {
                    return Ok(true);
                }
                Err(ProError::Internal(
                    "Paddle did not schedule the cancellation".into(),
                ))
            }
            Err(e) => {
                // A subscription already past its final period, or a
                // product configuration that cannot be scheduled, is told to
                // end now instead — a refusal to schedule is not a refusal
                // to cancel.
                self.request(
                    reqwest::Method::POST,
                    &format!("/subscriptions/{id}/cancel"),
                    None,
                )
                .await?;
                let _ = e;
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paddle() -> Paddle {
        Paddle::new("pdl_test".into(), Some("pdl_secret".into()), true)
    }

    fn catalogue() -> Vec<Plan> {
        let mut plans = pro_core::plans::default_plans();
        plans[0].price_ids.insert("paddle".into(), "pri_starter".into());
        plans[1].price_ids.insert("paddle".into(), "pri_creator".into());
        plans
    }

    #[test]
    fn uses_the_sandbox_when_asked() {
        assert_eq!(paddle().api(), "https://sandbox-api.paddle.com");
        assert_eq!(
            Paddle::new("k".into(), None, false).api(),
            "https://api.paddle.com"
        );
    }

    /// Paddle's envelope is flatter than Stripe's, and the id field is named
    /// differently — getting this wrong silently disables idempotency.
    #[test]
    fn reads_the_event_id_from_paddles_envelope() {
        assert_eq!(paddle().event_id(&json!({"event_id": "evt_abc"})), "evt_abc");
        // Stripe's field name must not accidentally work.
        assert_eq!(paddle().event_id(&json!({"id": "evt_abc"})), "");
    }

    #[test]
    fn interprets_a_subscription_update() {
        let event = json!({
            "event_id": "evt_1",
            "event_type": "subscription.updated",
            "data": {
                "id": "sub_01h",
                "customer_id": "ctm_01h",
                "status": "active",
                "current_billing_period": {
                    "starts_at": "2026-01-01T00:00:00Z",
                    "ends_at": "2026-02-01T00:00:00Z"
                },
                "items": [ { "price": { "id": "pri_creator" } } ],
                "custom_data": { "account_id": "acc_1", "tenant_id": "ten_1" }
            }
        });
        let BillingEvent::SubscriptionChanged(state) =
            paddle().interpret(&event, &catalogue()).unwrap()
        else {
            panic!("expected a subscription change")
        };

        assert_eq!(state.provider_subscription_id, "sub_01h");
        assert_eq!(state.provider_customer_id.as_deref(), Some("ctm_01h"));
        assert_eq!(state.status, "active");
        assert_eq!(state.plan_slug.as_deref(), Some("creator"));
        assert_eq!(state.account_id.as_deref(), Some("acc_1"));
        assert_eq!(state.tenant_id.as_deref(), Some("ten_1"));
        assert_eq!(state.current_period_end.as_deref(), Some("2026-02-01T00:00:00Z"));
    }

    /// Paddle expresses "will not renew" as a scheduled change, not a
    /// boolean. Missing this would mean the dashboard never shows that a
    /// customer has cancelled.
    #[test]
    fn reads_a_scheduled_cancellation() {
        let event = json!({
            "event_type": "subscription.updated",
            "data": {
                "id": "sub_1", "status": "active",
                "scheduled_change": { "action": "cancel", "effective_at": "2026-02-01T00:00:00Z" },
                "items": [], "custom_data": {}
            }
        });
        let BillingEvent::SubscriptionChanged(state) =
            paddle().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.cancel_at_period_end, Some(true));

        // A different scheduled change is not a cancellation.
        let paused = json!({
            "event_type": "subscription.updated",
            "data": {
                "id": "sub_1", "status": "active",
                "scheduled_change": { "action": "pause" },
                "items": [], "custom_data": {}
            }
        });
        let BillingEvent::SubscriptionChanged(state) =
            paddle().interpret(&paused, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.cancel_at_period_end, Some(false));
    }

    #[test]
    fn a_cancellation_event_wins_over_the_status_field() {
        let event = json!({
            "event_type": "subscription.canceled",
            "data": { "id": "sub_1", "status": "active", "items": [], "custom_data": {} }
        });
        let BillingEvent::SubscriptionChanged(state) =
            paddle().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "canceled");
    }

    #[test]
    fn interprets_a_trial() {
        let event = json!({
            "event_type": "subscription.trialing",
            "data": {
                "id": "sub_1", "status": "trialing",
                "items": [ {
                    "price": { "id": "pri_starter" },
                    "trial_dates": { "ends_at": "2026-01-15T00:00:00Z" }
                } ],
                "custom_data": {}
            }
        });
        let BillingEvent::SubscriptionChanged(state) =
            paddle().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "trialing");
        assert_eq!(state.trial_ends_at.as_deref(), Some("2026-01-15T00:00:00Z"));
    }

    /// Payment outcomes arrive on the transaction, which names its
    /// subscription.
    #[test]
    fn interprets_transaction_outcomes() {
        let failed = json!({
            "event_type": "transaction.payment_failed",
            "data": { "id": "txn_1", "subscription_id": "sub_1" }
        });
        assert_eq!(
            paddle().interpret(&failed, &catalogue()).unwrap(),
            BillingEvent::PaymentFailed { provider_subscription_id: "sub_1".into() }
        );

        let paid = json!({
            "event_type": "transaction.completed",
            "data": { "id": "txn_1", "subscription_id": "sub_1" }
        });
        assert_eq!(
            paddle().interpret(&paid, &catalogue()).unwrap(),
            BillingEvent::PaymentSucceeded { provider_subscription_id: "sub_1".into() }
        );
    }

    /// A one-off purchase has no subscription and is not a site's billing.
    #[test]
    fn ignores_a_transaction_without_a_subscription() {
        let event = json!({
            "event_type": "transaction.completed",
            "data": { "id": "txn_1" }
        });
        assert!(matches!(
            paddle().interpret(&event, &catalogue()).unwrap(),
            BillingEvent::Ignored { .. }
        ));
    }

    #[test]
    fn ignores_unrelated_events() {
        for kind in ["customer.created", "address.updated", "product.updated"] {
            let event = json!({"event_type": kind, "data": {}});
            assert!(matches!(
                paddle().interpret(&event, &catalogue()).unwrap(),
                BillingEvent::Ignored { .. }
            ));
        }
    }

    #[test]
    fn an_unmappable_status_is_an_error() {
        let event = json!({
            "event_type": "subscription.updated",
            "data": { "id": "sub_1", "status": "brand_new_state", "items": [], "custom_data": {} }
        });
        assert!(paddle().interpret(&event, &catalogue()).is_err());
    }

    #[test]
    fn normalises_dates_to_the_stored_form() {
        assert_eq!(
            normalise_date(Some(&json!("2026-01-01T00:00:00.000Z"))).as_deref(),
            Some("2026-01-01T00:00:00Z"),
            "millisecond precision is dropped so date strings compare correctly"
        );
        assert_eq!(normalise_date(Some(&Value::Null)), None);
        assert_eq!(normalise_date(None), None);
    }

    #[test]
    fn requires_the_signature_header() {
        assert!(paddle().verify_webhook(b"{}", &WebhookHeaders::default()).is_err());
    }

    #[test]
    fn verifies_a_real_signature() {
        let body = br#"{"event_id":"evt_1","event_type":"subscription.updated"}"#;
        let ts = chrono::Utc::now().timestamp();
        let signed = format!("{ts}:{}", String::from_utf8_lossy(body));
        let sig = signature::hmac_sha256_hex(b"pdl_secret", signed.as_bytes());
        let headers = WebhookHeaders {
            paddle_signature: Some(format!("ts={ts};h1={sig}")),
            ..Default::default()
        };
        assert!(paddle().verify_webhook(body, &headers).is_ok());
    }
}
