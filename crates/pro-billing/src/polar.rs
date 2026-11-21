//! Polar hosted checkout, customer sessions, and subscription webhooks.
use pro_core::{Plan, ProError, Result, Subscription};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

use crate::{
    signature, BillingEvent, Checkout, CheckoutRequest, Provider, SubscriptionState, WebhookHeaders,
};

pub struct Polar {
    access_token: String,
    webhook_secret: String,
    api: &'static str,
}

impl Polar {
    pub fn new(access_token: String, webhook_secret: String, sandbox: bool) -> Self {
        Self {
            access_token,
            webhook_secret,
            api: if sandbox {
                "https://sandbox-api.polar.sh/v1"
            } else {
                "https://api.polar.sh/v1"
            },
        }
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Value,
    ) -> Result<T> {
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(ProError::internal)?
            .request(method, format!("{}{path}", self.api))
            .bearer_auth(&self.access_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProError::Unavailable(format!("Could not reach Polar: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            tracing::warn!(%status, "Polar rejected a billing request");
            return Err(ProError::Unavailable(format!(
                "Polar rejected the billing request ({status})."
            )));
        }

        response.json().await.map_err(|e| {
            ProError::Unavailable(format!(
                "Polar returned an unexpected billing response: {e}"
            ))
        })
    }
}

fn product_id(plan: &Plan) -> Result<&str> {
    plan.price_id("polar").ok_or_else(|| {
        ProError::Config(format!(
            "plan {:?} has no Polar product ID — set plans[].priceIds.polar",
            plan.slug
        ))
    })
}

fn subscription_path(subscription: &Subscription) -> Result<String> {
    let id = subscription
        .provider_subscription_id
        .as_deref()
        .ok_or_else(|| ProError::BadRequest("This subscription has no Polar ID.".into()))?;
    const PATH_ID: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC.remove(b'-');
    let id = percent_encoding::utf8_percent_encode(id, PATH_ID);
    Ok(format!("/subscriptions/{id}"))
}

#[derive(Deserialize)]
struct CheckoutSession {
    url: String,
}

#[derive(Deserialize)]
struct CustomerSession {
    customer_portal_url: String,
}

#[derive(Deserialize)]
struct Cancellation {
    cancel_at_period_end: bool,
}

#[async_trait::async_trait]
impl Provider for Polar {
    fn name(&self) -> &'static str {
        "polar"
    }

    async fn create_checkout(&self, req: CheckoutRequest<'_>) -> Result<Checkout> {
        let mut body = json!({
            "products": [product_id(req.plan)?],
            "external_customer_id": req.account_id,
            "customer_email": req.account_email,
            "success_url": req.success_url,
            "return_url": req.cancel_url,
            "allow_trial": req.trial_days > 0,
        });
        if let Some(tenant_id) = req.tenant_id {
            body["metadata"] = json!({ "tenant_id": tenant_id });
        }
        if req.trial_days > 0 {
            body["trial_interval"] = json!("day");
            body["trial_interval_count"] = json!(req.trial_days);
        }

        let checkout: CheckoutSession = self.request(Method::POST, "/checkouts/", body).await?;
        Ok(Checkout { url: checkout.url })
    }

    fn verify_webhook(&self, body: &[u8], headers: &WebhookHeaders) -> Result<()> {
        signature::verify_polar(body, headers, &self.webhook_secret)
    }

    fn event_id(&self, _body: &Value, headers: &WebhookHeaders) -> String {
        headers.webhook_id.clone().unwrap_or_default()
    }

    fn interpret(&self, body: &Value, catalogue: &[Plan]) -> Result<BillingEvent> {
        let event_type = body["type"]
            .as_str()
            .ok_or_else(|| ProError::BadRequest("Polar webhook has no event type".into()))?;
        let ignored = || BillingEvent::Ignored {
            event_type: event_type.into(),
        };
        if !matches!(
            event_type,
            "subscription.created"
                | "subscription.updated"
                | "subscription.active"
                | "subscription.canceled"
                | "subscription.uncanceled"
                | "subscription.revoked"
                | "subscription.past_due"
        ) {
            return Ok(ignored());
        }

        let data = &body["data"];
        let raw_status = data["status"].as_str().unwrap_or_default();
        // An incomplete first payment never opens a site or starts dunning.
        if matches!(raw_status, "incomplete" | "incomplete_expired") {
            return Ok(ignored());
        }
        let status = crate::normalise_status("polar", raw_status).ok_or_else(|| {
            ProError::BadRequest(format!(
                "Unrecognised Polar subscription status {raw_status:?}"
            ))
        })?;

        let id = data["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ProError::BadRequest("Polar webhook has no subscription ID".into()))?;
        let plan_slug = data["product_id"]
            .as_str()
            .and_then(|id| crate::plan_for_price(catalogue, "polar", id))
            .map(|p| p.slug.clone());
        let read_date = |field: &str| {
            data[field]
                .as_str()
                .and_then(pro_core::parse_iso)
                .map(|date| date.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        };
        let provider_version = data["modified_at"]
            .as_str()
            .or_else(|| data["created_at"].as_str())
            .or_else(|| body["timestamp"].as_str())
            .and_then(pro_core::parse_iso)
            .map(|date| date.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string());

        Ok(BillingEvent::SubscriptionChanged(SubscriptionState {
            provider_subscription_id: id.into(),
            provider_customer_id: data["customer_id"].as_str().map(str::to_owned),
            status: status.into(),
            plan_slug,
            current_period_start: read_date("current_period_start"),
            current_period_end: read_date("current_period_end"),
            trial_ends_at: read_date("trial_end"),
            cancel_at_period_end: data["cancel_at_period_end"].as_bool(),
            canceled_at: read_date("canceled_at"),
            account_id: data
                .pointer("/customer/external_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tenant_id: data
                .pointer("/metadata/tenant_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            provider_version,
        }))
    }

    async fn management_url(&self, subscription: &Subscription) -> Result<Option<String>> {
        let body = match subscription.provider_customer_id.as_deref() {
            Some(id) => json!({ "customer_id": id }),
            None => json!({ "external_customer_id": subscription.account_id }),
        };
        let session: CustomerSession = self
            .request(Method::POST, "/customer-sessions/", body)
            .await?;
        Ok(Some(session.customer_portal_url))
    }

    async fn change_plan(&self, subscription: &Subscription, plan: &Plan) -> Result<()> {
        let _: Value = self
            .request(
                Method::PATCH,
                &subscription_path(subscription)?,
                json!({
                    "product_id": product_id(plan)?,
                }),
            )
            .await?;
        Ok(())
    }

    async fn cancel_subscription(&self, subscription: &Subscription) -> Result<bool> {
        let response: Cancellation = self
            .request(
                Method::PATCH,
                &subscription_path(subscription)?,
                json!({
                    "cancel_at_period_end": true,
                }),
            )
            .await?;
        if !response.cancel_at_period_end {
            return Err(ProError::Unavailable(
                "Polar did not confirm scheduled cancellation.".into(),
            ));
        }
        Ok(true)
    }
}
