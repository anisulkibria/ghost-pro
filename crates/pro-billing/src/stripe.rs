//! Stripe.
//!
//! Checkout Sessions in subscription mode, webhooks verified with
//! `Stripe-Signature`, and the Billing Portal for self-service management.
//!
//! Reached over the REST API with `reqwest` rather than through a crate: the
//! platform touches four endpoints, and a form-encoded POST is less code than
//! keeping a large SDK's version in step with the API.
use pro_core::{Plan, ProError, Result, Subscription};
use serde_json::Value;

use crate::{
    signature, BillingEvent, Checkout, CheckoutRequest, Provider, SubscriptionState,
    WebhookHeaders,
};

const API: &str = "https://api.stripe.com/v1";

pub struct Stripe {
    secret_key: String,
    webhook_secret: Option<String>,
}

impl Stripe {
    pub fn new(secret_key: String, webhook_secret: Option<String>) -> Self {
        Self { secret_key, webhook_secret }
    }

    fn client(&self) -> Result<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(ProError::internal)
    }

    /// POST a form-encoded request and return the parsed body.
    ///
    /// Stripe reports its own errors in a JSON body with a 4xx, and those
    /// messages are written to be read — an unusable price id or a key in the
    /// wrong mode says exactly that. Surfacing the text is far more useful to
    /// an operator than "checkout failed".
    async fn post(&self, path: &str, form: &[(&str, String)]) -> Result<Value> {
        let response = self
            .client()?
            .post(format!("{API}{path}"))
            .basic_auth(&self.secret_key, Some(""))
            .form(form)
            .send()
            .await
            .map_err(|e| ProError::Unavailable(format!("could not reach Stripe: {e}")))?;

        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Stripe rejected the request");
        Err(ProError::BadRequest(format!("Stripe: {message}")))
    }
}

/// Seconds since the epoch, as Stripe reports dates, to our ISO-8601.
fn from_unix(value: Option<&Value>) -> Option<String> {
    let secs = value?.as_i64()?;
    chrono::DateTime::from_timestamp(secs, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

#[async_trait::async_trait]
impl Provider for Stripe {
    fn name(&self) -> &'static str {
        "stripe"
    }

    async fn create_checkout(&self, req: CheckoutRequest<'_>) -> Result<Checkout> {
        let price = req.plan.price_id("stripe").ok_or_else(|| {
            ProError::Config(format!(
                "plan {:?} has no Stripe price id — set plans[].priceIds.stripe",
                req.plan.slug
            ))
        })?;

        let mut form = vec![
            ("mode", "subscription".to_string()),
            ("line_items[0][price]", price.to_string()),
            ("line_items[0][quantity]", "1".to_string()),
            ("success_url", req.success_url.to_string()),
            ("cancel_url", req.cancel_url.to_string()),
            ("customer_email", req.account_email.to_string()),
            // On the session, so `checkout.session.completed` carries it.
            ("metadata[account_id]", req.account_id.to_string()),
            ("metadata[plan_slug]", req.plan.slug.clone()),
            // And on the subscription, because every later
            // `customer.subscription.*` event reads its metadata and not the
            // session's. Setting only one of the two is why a renewal months
            // later cannot be matched to an account.
            ("subscription_data[metadata][account_id]", req.account_id.to_string()),
            ("subscription_data[metadata][plan_slug]", req.plan.slug.clone()),
        ];
        if let Some(tenant) = req.tenant_id {
            form.push(("metadata[tenant_id]", tenant.to_string()));
            form.push(("subscription_data[metadata][tenant_id]", tenant.to_string()));
        }
        if req.trial_days > 0 {
            form.push(("subscription_data[trial_period_days]", req.trial_days.to_string()));
        }

        let body = self.post("/checkout/sessions", &form).await?;
        let url = body["url"]
            .as_str()
            .ok_or_else(|| ProError::Unavailable("Stripe returned no checkout url".into()))?;
        Ok(Checkout { url: url.to_string() })
    }

    fn verify_webhook(&self, body: &[u8], headers: &WebhookHeaders) -> Result<()> {
        let header = headers
            .stripe_signature
            .as_deref()
            .ok_or_else(|| ProError::BadRequest("missing Stripe-Signature header".into()))?;
        signature::verify_stripe(body, header, self.webhook_secret.as_deref().unwrap_or(""))
    }

    fn event_id(&self, body: &Value) -> String {
        body["id"].as_str().unwrap_or_default().to_string()
    }

    fn interpret(&self, body: &Value, catalogue: &[Plan]) -> Result<BillingEvent> {
        let event_type = body["type"].as_str().unwrap_or_default().to_string();
        let object = &body["data"]["object"];

        match event_type.as_str() {
            // The session itself carries no subscription status, so the
            // subscription event that follows is what establishes state. This
            // is handled only to link the customer to the account early, which
            // matters if the subscription event arrives first — webhooks are
            // not ordered.
            "checkout.session.completed" => {
                let Some(subscription_id) = object["subscription"].as_str() else {
                    // A one-off payment, not a subscription.
                    return Ok(BillingEvent::Ignored { event_type });
                };
                Ok(BillingEvent::SubscriptionChanged(SubscriptionState {
                    provider_subscription_id: subscription_id.to_string(),
                    provider_customer_id: object["customer"].as_str().map(str::to_string),
                    // Checkout completing means it is live; the following
                    // subscription event refines this.
                    status: "active".into(),
                    plan_slug: object["metadata"]["plan_slug"].as_str().map(str::to_string),
                    account_id: object["metadata"]["account_id"].as_str().map(str::to_string),
                    tenant_id: object["metadata"]["tenant_id"].as_str().map(str::to_string),
                    ..Default::default()
                }))
            }

            "customer.subscription.created"
            | "customer.subscription.updated"
            | "customer.subscription.deleted"
            | "customer.subscription.paused"
            | "customer.subscription.resumed" => {
                let raw_status = object["status"].as_str().unwrap_or_default();
                // A deletion event can still carry `status: active`, so the
                // event type has to win here.
                let status = if event_type == "customer.subscription.deleted" {
                    "canceled"
                } else {
                    crate::normalise_status("stripe", raw_status).ok_or_else(|| {
                        ProError::Internal(format!(
                            "Stripe sent subscription status {raw_status:?}, which this \
                             version does not map"
                        ))
                    })?
                };

                // The plan is recovered from the price rather than trusted
                // from metadata: metadata is what *we* wrote at checkout, and
                // a plan changed later at Stripe would leave it stale.
                let price = object
                    .pointer("/items/data/0/price/id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let plan_slug = crate::plan_for_price(catalogue, "stripe", price)
                    .map(|p| p.slug.clone())
                    .or_else(|| object["metadata"]["plan_slug"].as_str().map(str::to_string));

                Ok(BillingEvent::SubscriptionChanged(SubscriptionState {
                    provider_subscription_id: object["id"].as_str().unwrap_or_default().to_string(),
                    provider_customer_id: object["customer"].as_str().map(str::to_string),
                    status: status.to_string(),
                    plan_slug,
                    current_period_start: from_unix(object.get("current_period_start")),
                    current_period_end: from_unix(object.get("current_period_end")),
                    trial_ends_at: from_unix(object.get("trial_end")),
                    cancel_at_period_end: object["cancel_at_period_end"].as_bool(),
                    canceled_at: from_unix(object.get("canceled_at")),
                    account_id: object["metadata"]["account_id"].as_str().map(str::to_string),
                    tenant_id: object["metadata"]["tenant_id"].as_str().map(str::to_string),
                }))
            }

            "invoice.payment_failed" => {
                let Some(id) = object["subscription"].as_str() else {
                    return Ok(BillingEvent::Ignored { event_type });
                };
                Ok(BillingEvent::PaymentFailed { provider_subscription_id: id.to_string() })
            }
            "invoice.payment_succeeded" | "invoice.paid" => {
                let Some(id) = object["subscription"].as_str() else {
                    return Ok(BillingEvent::Ignored { event_type });
                };
                Ok(BillingEvent::PaymentSucceeded { provider_subscription_id: id.to_string() })
            }

            _ => Ok(BillingEvent::Ignored { event_type }),
        }
    }

    async fn management_url(&self, subscription: &Subscription) -> Result<Option<String>> {
        let Some(customer) = &subscription.provider_customer_id else {
            return Ok(None);
        };
        // A portal session is short-lived and single-use, so it is created on
        // demand rather than stored.
        let body = self
            .post(
                "/billing_portal/sessions",
                &[("customer", customer.clone())],
            )
            .await?;
        Ok(body["url"].as_str().map(str::to_string))
    }

    async fn cancel_subscription(&self, subscription: &Subscription) -> Result<bool> {
        let Some(id) = &subscription.provider_subscription_id else {
            return Err(ProError::BadRequest(
                "this subscription has no Stripe id".into(),
            ));
        };
        // `cancel_at_period_end` rather than a DELETE: the customer has paid
        // for the rest of the period, and Stripe both honours that and says
        // so in the response. Stripe sends no webhook for the *scheduling* —
        // only `customer.subscription.deleted` when the period actually ends
        // — so the platform notes the pending end itself.
        let body = self
            .post(
                &format!("/subscriptions/{id}"),
                &[("cancel_at_period_end", "true".to_string())],
            )
            .await?;
        Ok(body["cancel_at_period_end"].as_bool().unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stripe() -> Stripe {
        Stripe::new("sk_test_x".into(), Some("whsec_x".into()))
    }

    fn catalogue() -> Vec<Plan> {
        let mut plans = pro_core::plans::default_plans();
        plans[0].price_ids.insert("stripe".into(), "price_starter".into());
        plans[1].price_ids.insert("stripe".into(), "price_creator".into());
        plans
    }

    #[test]
    fn reads_the_event_id_for_idempotency() {
        assert_eq!(stripe().event_id(&json!({"id": "evt_123"})), "evt_123");
        assert_eq!(stripe().event_id(&json!({})), "");
    }

    #[test]
    fn interprets_a_subscription_update() {
        let event = json!({
            "id": "evt_1",
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": "sub_1",
                "customer": "cus_1",
                "status": "active",
                "current_period_start": 1_767_225_600i64,
                "current_period_end": 1_769_904_000i64,
                "cancel_at_period_end": false,
                "items": { "data": [ { "price": { "id": "price_creator" } } ] },
                "metadata": { "account_id": "acc_1", "tenant_id": "ten_1" }
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            stripe().interpret(&event, &catalogue()).unwrap()
        else {
            panic!("expected a subscription change")
        };

        assert_eq!(state.provider_subscription_id, "sub_1");
        assert_eq!(state.provider_customer_id.as_deref(), Some("cus_1"));
        assert_eq!(state.status, "active");
        assert_eq!(state.plan_slug.as_deref(), Some("creator"));
        assert_eq!(state.account_id.as_deref(), Some("acc_1"));
        assert_eq!(state.tenant_id.as_deref(), Some("ten_1"));
        assert_eq!(state.current_period_start.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(state.cancel_at_period_end, Some(false));
    }

    /// The plan comes from the price, not from metadata — a plan changed at
    /// Stripe after checkout would leave our metadata stale.
    #[test]
    fn the_price_decides_the_plan_not_the_metadata() {
        let event = json!({
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": "sub_1", "status": "active",
                "items": { "data": [ { "price": { "id": "price_creator" } } ] },
                "metadata": { "plan_slug": "starter" }
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            stripe().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.plan_slug.as_deref(), Some("creator"), "the live price wins");
    }

    /// A price the catalogue does not know falls back to metadata rather than
    /// losing the plan entirely.
    #[test]
    fn falls_back_to_metadata_for_an_unknown_price() {
        let event = json!({
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": "sub_1", "status": "active",
                "items": { "data": [ { "price": { "id": "price_retired" } } ] },
                "metadata": { "plan_slug": "starter" }
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            stripe().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.plan_slug.as_deref(), Some("starter"));
    }

    /// A deletion event can still carry `status: active`, so the event type
    /// has to win — otherwise a cancelled subscription reads as live.
    #[test]
    fn a_deletion_is_a_cancellation_whatever_the_status_says() {
        let event = json!({
            "type": "customer.subscription.deleted",
            "data": { "object": {
                "id": "sub_1", "status": "active",
                "items": { "data": [] }, "metadata": {}
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            stripe().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "canceled");
    }

    #[test]
    fn interprets_a_trialing_subscription() {
        let event = json!({
            "type": "customer.subscription.created",
            "data": { "object": {
                "id": "sub_1", "status": "trialing",
                "trial_end": 1_767_225_600i64,
                "items": { "data": [ { "price": { "id": "price_starter" } } ] },
                "metadata": {}
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            stripe().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "trialing");
        assert_eq!(state.trial_ends_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn interprets_payment_outcomes() {
        let failed = json!({
            "type": "invoice.payment_failed",
            "data": { "object": { "subscription": "sub_1" } }
        });
        assert_eq!(
            stripe().interpret(&failed, &catalogue()).unwrap(),
            BillingEvent::PaymentFailed { provider_subscription_id: "sub_1".into() }
        );

        let paid = json!({
            "type": "invoice.payment_succeeded",
            "data": { "object": { "subscription": "sub_1" } }
        });
        assert_eq!(
            stripe().interpret(&paid, &catalogue()).unwrap(),
            BillingEvent::PaymentSucceeded { provider_subscription_id: "sub_1".into() }
        );
    }

    /// An invoice with no subscription is a one-off payment, not a billing
    /// failure for a site.
    #[test]
    fn ignores_an_invoice_without_a_subscription() {
        let event = json!({
            "type": "invoice.payment_failed",
            "data": { "object": {} }
        });
        assert!(matches!(
            stripe().interpret(&event, &catalogue()).unwrap(),
            BillingEvent::Ignored { .. }
        ));
    }

    /// Most of what Stripe sends is not the platform's business, and has to
    /// be acknowledged rather than erroring — an error means endless retries.
    #[test]
    fn ignores_unrelated_events() {
        for kind in ["payment_method.attached", "invoice.finalized", "charge.succeeded"] {
            let event = json!({"type": kind, "data": {"object": {}}});
            assert!(
                matches!(
                    stripe().interpret(&event, &catalogue()).unwrap(),
                    BillingEvent::Ignored { .. }
                ),
                "{kind} should be ignored"
            );
        }
    }

    /// An unmapped status is a bug to surface, not a state to guess at.
    #[test]
    fn an_unmappable_status_is_an_error() {
        let event = json!({
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": "sub_1", "status": "something_new",
                "items": { "data": [] }, "metadata": {}
            }}
        });
        assert!(stripe().interpret(&event, &catalogue()).is_err());
    }

    #[test]
    fn converts_stripe_timestamps() {
        assert_eq!(
            from_unix(Some(&json!(1_767_225_600i64))).as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(from_unix(Some(&Value::Null)), None);
        assert_eq!(from_unix(None), None);
    }

    /// Verification must refuse when the header is absent, rather than
    /// skipping the check.
    #[test]
    fn requires_the_signature_header() {
        let err = stripe().verify_webhook(b"{}", &WebhookHeaders::default()).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn verifies_a_real_signature() {
        let body = br#"{"id":"evt_1","type":"invoice.paid"}"#;
        let ts = chrono::Utc::now().timestamp();
        let signed = format!("{ts}.{}", String::from_utf8_lossy(body));
        let sig = signature::hmac_sha256_hex(b"whsec_x", signed.as_bytes());
        let headers = WebhookHeaders {
            stripe_signature: Some(format!("t={ts},v1={sig}")),
            ..Default::default()
        };
        assert!(stripe().verify_webhook(body, &headers).is_ok());
    }
}
