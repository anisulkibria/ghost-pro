//! Lemon Squeezy.
//!
//! Also a merchant of record, and the simplest of the three to set up — which
//! is why it is worth offering alongside Paddle.
//!
//! Two things make it structurally different from the other two:
//!
//! * **JSON:API envelopes.** Everything is
//!   `{data: {id, type, attributes: {...}}}`, so a subscription's fields live
//!   under `data.attributes` and its id is a *string of digits* at
//!   `data.id` — not inside the attributes.
//! * **No signed timestamp.** `X-Signature` is an HMAC of the body alone, so
//!   the signature grants no replay protection at all. The event-id
//!   idempotency in `pro_db::audit::claim_webhook` is the only thing standing
//!   between a captured delivery and it being replayed indefinitely. For this
//!   provider that table is load-bearing, not an optimisation.
//!
//! Ids are numeric strings throughout, which is why `pro-core`'s config loader
//! refuses to coerce digit-strings to numbers: a store id of `4242` must stay
//! `"4242"` or it stops matching what the API returns.
use pro_core::{Plan, ProError, Result, Subscription};
use serde_json::{json, Value};

use crate::{
    signature, BillingEvent, Checkout, CheckoutRequest, Provider, SubscriptionState,
    WebhookHeaders,
};

const API: &str = "https://api.lemonsqueezy.com/v1";

pub struct LemonSqueezy {
    api_key: String,
    store_id: String,
    webhook_secret: Option<String>,
    test_mode: bool,
}

impl LemonSqueezy {
    pub fn new(
        api_key: String,
        store_id: String,
        webhook_secret: Option<String>,
        test_mode: bool,
    ) -> Self {
        Self { api_key, store_id, webhook_secret, test_mode }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(ProError::internal)?;

        let mut builder = client
            .request(method, format!("{API}{path}"))
            .bearer_auth(&self.api_key)
            .header("Accept", "application/vnd.api+json");
        if let Some(body) = body {
            builder = builder
                .header("Content-Type", "application/vnd.api+json")
                .body(serde_json::to_vec(&body)?);
        }

        let response = builder
            .send()
            .await
            .map_err(|e| ProError::Unavailable(format!("could not reach Lemon Squeezy: {e}")))?;

        let status = response.status();
        let parsed: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(parsed);
        }
        // JSON:API errors are an array; the first detail is the readable part.
        let message = parsed
            .pointer("/errors/0/detail")
            .and_then(Value::as_str)
            .unwrap_or("Lemon Squeezy rejected the request");
        Err(ProError::BadRequest(format!("Lemon Squeezy: {message}")))
    }
}

fn normalise_date(value: Option<&Value>) -> Option<String> {
    let raw = value?.as_str()?;
    pro_core::parse_iso(raw)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .or_else(|| Some(raw.to_string()))
}

#[async_trait::async_trait]
impl Provider for LemonSqueezy {
    fn name(&self) -> &'static str {
        "lemonsqueezy"
    }

    /// Create a checkout for a variant.
    ///
    /// `req.trial_days` is deliberately not sent. A Lemon Squeezy trial is a
    /// property of the *variant*, configured once in their dashboard, and the
    /// checkout API has no per-checkout override — so passing it would be
    /// silently dropped. What the plan's `trialDays` still controls here is
    /// everything on this platform's side: whether the dashboard says "trial
    /// ends", when the housekeeping sweep moves an expired trial into dunning,
    /// and what the pricing page advertises. Keep the two in step: a variant
    /// with a 14-day trial wants `trialDays: 14` on its plan.
    async fn create_checkout(&self, req: CheckoutRequest<'_>) -> Result<Checkout> {
        let variant = req.plan.price_id("lemonsqueezy").ok_or_else(|| {
            ProError::Config(format!(
                "plan {:?} has no Lemon Squeezy variant id — set \
                 plans[].priceIds.lemonsqueezy",
                req.plan.slug
            ))
        })?;

        let mut custom = json!({
            "account_id": req.account_id,
            "plan_slug": req.plan.slug,
        });
        if let Some(tenant) = req.tenant_id {
            custom["tenant_id"] = json!(tenant);
        }

        let body = json!({
            "data": {
                "type": "checkouts",
                "attributes": {
                    "checkout_data": {
                        "email": req.account_email,
                        // Comes back on every webhook for this subscription,
                        // which is how a renewal months later is matched to an
                        // account.
                        "custom": custom,
                    },
                    "checkout_options": { "embed": false },
                    "product_options": { "redirect_url": req.success_url },
                    "test_mode": self.test_mode,
                },
                "relationships": {
                    "store": { "data": { "type": "stores", "id": self.store_id } },
                    "variant": { "data": { "type": "variants", "id": variant } }
                }
            }
        });

        let response = self.request(reqwest::Method::POST, "/checkouts", Some(body)).await?;
        let url = response
            .pointer("/data/attributes/url")
            .and_then(Value::as_str)
            .ok_or_else(|| ProError::Unavailable("Lemon Squeezy returned no checkout url".into()))?;
        Ok(Checkout { url: url.to_string() })
    }

    fn verify_webhook(&self, body: &[u8], headers: &WebhookHeaders) -> Result<()> {
        let header = headers
            .x_signature
            .as_deref()
            .ok_or_else(|| ProError::BadRequest("missing X-Signature header".into()))?;
        signature::verify_lemonsqueezy(body, header, self.webhook_secret.as_deref().unwrap_or(""))
    }

    /// Lemon Squeezy puts the event name in a header and an id in
    /// `meta.webhook_id`.
    fn event_id(&self, body: &Value) -> String {
        body.pointer("/meta/webhook_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn interpret(&self, body: &Value, catalogue: &[Plan]) -> Result<BillingEvent> {
        // The event name travels in `meta.event_name` on the body as well as
        // in the `X-Event-Name` header; the body is used so interpretation
        // needs nothing but the payload.
        let event_type = body
            .pointer("/meta/event_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let data = &body["data"];
        let attrs = &data["attributes"];

        match event_type.as_str() {
            // Every event whose payload *is* a subscription object, so one arm
            // reads them all. `subscription_plan_changed` belongs here rather
            // than being ignored: it is how an upgrade or downgrade arrives,
            // and the new `variant_id` in these same attributes is what moves
            // the tenant onto the plan it now pays for.
            "subscription_created" | "subscription_updated" | "subscription_cancelled"
            | "subscription_resumed" | "subscription_expired" | "subscription_paused"
            | "subscription_unpaused" | "subscription_plan_changed" => {
                let raw_status = attrs["status"].as_str().unwrap_or_default();
                let mut status = match event_type.as_str() {
                    "subscription_expired" => "expired",
                    "subscription_paused" => "paused",
                    _ => crate::normalise_status("lemonsqueezy", raw_status).ok_or_else(|| {
                        ProError::Internal(format!(
                            "Lemon Squeezy sent subscription status {raw_status:?}, which \
                             this version does not map"
                        ))
                    })?,
                };

                // A cancellation whose period has not run out yet is still
                // entitled, and saying otherwise takes a paying customer's
                // site down early.
                //
                // Lemon Squeezy's `cancelled` means "will not renew": the
                // subscription keeps `ends_at` in the future and the customer
                // keeps access until then, which is the same bargain as
                // Stripe's `cancel_at_period_end`. But `canceled` is not in
                // `Subscription::is_entitled`, so reporting it here is what
                // made `apply_subscription` suspend the site the moment
                // somebody cancelled — in the middle of a month they had
                // already paid for. `subscription_expired` arrives when the
                // period actually ends, and *that* is what suspends.
                //
                // Only the status is softened; `cancel_at_period_end` below
                // still records that it will not renew, so the dashboard says
                // "Ends <date>" rather than pretending nothing happened.
                let ends_in_future = normalise_date(attrs.get("ends_at"))
                    .and_then(|at| pro_core::parse_iso(&at))
                    .map(|at| at > chrono::Utc::now())
                    .unwrap_or(false);
                if status == "canceled" && ends_in_future {
                    status = "active";
                }

                // The variant id is a number in the attributes, but ids are
                // strings everywhere else — so it is normalised before being
                // looked up in the catalogue.
                let variant = attrs["variant_id"]
                    .as_i64()
                    .map(|n| n.to_string())
                    .or_else(|| attrs["variant_id"].as_str().map(str::to_string))
                    .unwrap_or_default();
                let plan_slug = crate::plan_for_price(catalogue, "lemonsqueezy", &variant)
                    .map(|p| p.slug.clone())
                    .or_else(|| {
                        body.pointer("/meta/custom_data/plan_slug")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });

                // `cancelled` here means "will not renew", with the site live
                // until `ends_at` — which is Lemon Squeezy's equivalent of
                // Stripe's `cancel_at_period_end`, not an immediate stop.
                let cancelled_flag = attrs["cancelled"].as_bool();

                Ok(BillingEvent::SubscriptionChanged(SubscriptionState {
                    // The subscription id is on the envelope, not in the
                    // attributes, and it is a numeric string.
                    provider_subscription_id: data["id"]
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| data["id"].as_i64().map(|n| n.to_string()))
                        .unwrap_or_default(),
                    provider_customer_id: attrs["customer_id"]
                        .as_i64()
                        .map(|n| n.to_string())
                        .or_else(|| attrs["customer_id"].as_str().map(str::to_string)),
                    status: status.to_string(),
                    plan_slug,
                    // Lemon Squeezy reports only the *next* renewal, never the
                    // period start. Left unset rather than derived by
                    // subtracting an interval: `upsert` treats absent as
                    // unchanged, so the value established at checkout
                    // survives, and a guessed start would move the window the
                    // email allowance is counted in.
                    current_period_start: None,
                    // Whichever date the subscription actually runs to: a live
                    // one renews, a cancelled one ends. Both are the same
                    // thing to the dashboard, which prints "Renews" or "Ends"
                    // from `cancel_at_period_end`.
                    current_period_end: normalise_date(attrs.get("renews_at"))
                        .or_else(|| normalise_date(attrs.get("ends_at"))),
                    trial_ends_at: normalise_date(attrs.get("trial_ends_at")),
                    cancel_at_period_end: cancelled_flag,
                    // When it *stopped*, not when it was asked to stop. While
                    // `ends_at` is still ahead the subscription is running, so
                    // stamping this would be recording an end that has not
                    // happened — and `canceled_at` is what reporting counts as
                    // a lost customer.
                    canceled_at: if cancelled_flag == Some(true) && !ends_in_future {
                        normalise_date(attrs.get("ends_at"))
                    } else {
                        None
                    },
                    account_id: body
                        .pointer("/meta/custom_data/account_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    tenant_id: body
                        .pointer("/meta/custom_data/tenant_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }))
            }

            // Money did not arrive, or went back out. Both start the dunning
            // clock rather than cancelling outright, so the customer gets the
            // platform's grace period to fix a card — which is the commonest
            // cause and takes a day.
            "subscription_payment_failed" | "subscription_payment_refunded" => {
                let Some(id) = subscription_id_from_payment(attrs) else {
                    return Ok(BillingEvent::Ignored { event_type });
                };
                Ok(BillingEvent::PaymentFailed { provider_subscription_id: id })
            }

            // Money arrived. `recovered` is the one that matters most and the
            // easiest to miss: it is Lemon Squeezy's word for a *previously
            // failed* payment finally going through. Left unhandled, a
            // customer who pays stays `past_due` and the housekeeping sweep
            // suspends their site at the end of the grace period anyway —
            // after they have paid. It clears dunning and brings a suspended
            // site back, exactly as a first successful payment does.
            "subscription_payment_success" | "subscription_payment_recovered" => {
                let Some(id) = subscription_id_from_payment(attrs) else {
                    return Ok(BillingEvent::Ignored { event_type });
                };
                Ok(BillingEvent::PaymentSucceeded { provider_subscription_id: id })
            }

            // `order_created` and `order_refunded` describe an *order*, not a
            // subscription: for a subscription product the matching
            // `subscription_created` / `subscription_payment_refunded` arrives
            // too and carries the subscription id this platform keys on.
            // Acknowledged so Lemon Squeezy stops resending them.
            _ => Ok(BillingEvent::Ignored { event_type }),
        }
    }

    async fn management_url(&self, subscription: &Subscription) -> Result<Option<String>> {
        let Some(id) = &subscription.provider_subscription_id else {
            return Ok(None);
        };
        let response = self
            .request(reqwest::Method::GET, &format!("/subscriptions/{id}"), None)
            .await?;
        Ok(response
            .pointer("/data/attributes/urls/customer_portal")
            .and_then(Value::as_str)
            .or_else(|| {
                response
                    .pointer("/data/attributes/urls/update_payment_method")
                    .and_then(Value::as_str)
            })
            .map(str::to_string))
    }

    /// Cancel, and report honestly whether access continues.
    ///
    /// `DELETE /subscriptions/:id` is Lemon Squeezy's only cancellation call
    /// and it is *not* immediate: it flips the subscription to `cancelled` and
    /// sets `ends_at` to the end of the period already paid for, which is the
    /// same bargain as Stripe's `cancel_at_period_end`. The response carries
    /// that date, so it is read back rather than assumed — a customer who
    /// cancels mid-month should be told their site runs to the date they paid
    /// through, not that it has stopped.
    ///
    /// `false` only when Lemon Squeezy returns no `ends_at`, which means the
    /// subscription really is over now (a trial cancelled before its first
    /// charge, for instance). Reporting end-of-period in that case would
    /// promise a grace period that does not exist.
    async fn cancel_subscription(&self, subscription: &Subscription) -> Result<bool> {
        let Some(id) = &subscription.provider_subscription_id else {
            return Err(ProError::BadRequest(
                "this subscription has no Lemon Squeezy id".into(),
            ));
        };
        let response = self
            .request(reqwest::Method::DELETE, &format!("/subscriptions/{id}"), None)
            .await?;

        let ends_at = normalise_date(response.pointer("/data/attributes/ends_at"));
        Ok(match ends_at {
            // In the future: access continues until then.
            Some(when) => pro_core::parse_iso(&when)
                .map(|d| d > chrono::Utc::now())
                .unwrap_or(false),
            None => false,
        })
    }
}

/// A payment event names its subscription in the attributes, as a number.
fn subscription_id_from_payment(attrs: &Value) -> Option<String> {
    attrs["subscription_id"]
        .as_i64()
        .map(|n| n.to_string())
        .or_else(|| attrs["subscription_id"].as_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ls() -> LemonSqueezy {
        LemonSqueezy::new("ls_key".into(), "4242".into(), Some("ls_secret".into()), true)
    }

    fn catalogue() -> Vec<Plan> {
        let mut plans = pro_core::plans::default_plans();
        plans[0].price_ids.insert("lemonsqueezy".into(), "812345".into());
        plans[1].price_ids.insert("lemonsqueezy".into(), "812346".into());
        plans
    }

    #[test]
    fn reads_the_webhook_id_for_idempotency() {
        // The only replay defence this provider has, since its signature
        // carries no timestamp.
        assert_eq!(ls().event_id(&json!({"meta": {"webhook_id": "wh_1"}})), "wh_1");
        assert_eq!(ls().event_id(&json!({"meta": {}})), "");
    }

    #[test]
    fn interprets_a_subscription_update() {
        let event = json!({
            "meta": {
                "event_name": "subscription_updated",
                "webhook_id": "wh_1",
                "custom_data": { "account_id": "acc_1", "tenant_id": "ten_1" }
            },
            "data": {
                "type": "subscriptions",
                "id": "998877",
                "attributes": {
                    "status": "active",
                    "customer_id": 55443,
                    "variant_id": 812346,
                    "renews_at": "2026-02-01T00:00:00.000000Z",
                    "cancelled": false
                }
            }
        });
        let BillingEvent::SubscriptionChanged(state) =
            ls().interpret(&event, &catalogue()).unwrap()
        else {
            panic!("expected a subscription change")
        };

        // The id is on the envelope, not in the attributes.
        assert_eq!(state.provider_subscription_id, "998877");
        // Numeric ids become strings, so they match what is stored.
        assert_eq!(state.provider_customer_id.as_deref(), Some("55443"));
        assert_eq!(state.status, "active");
        assert_eq!(state.plan_slug.as_deref(), Some("creator"));
        assert_eq!(state.account_id.as_deref(), Some("acc_1"));
        assert_eq!(state.tenant_id.as_deref(), Some("ten_1"));
        assert_eq!(state.current_period_end.as_deref(), Some("2026-02-01T00:00:00Z"));
    }

    /// `on_trial` is this provider's word for a trial.
    #[test]
    fn interprets_a_trial() {
        let event = json!({
            "meta": { "event_name": "subscription_created", "custom_data": {} },
            "data": { "id": "1", "attributes": {
                "status": "on_trial",
                "variant_id": 812345,
                "trial_ends_at": "2026-01-15T00:00:00.000000Z"
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            ls().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "trialing");
        assert_eq!(state.trial_ends_at.as_deref(), Some("2026-01-15T00:00:00Z"));
    }

    /// `cancelled` means "will not renew", with the site live until `ends_at`
    /// — not an immediate stop. Reading it as immediate would cut a paying
    /// customer off early.
    ///
    /// The end date is relative to now, not a literal: "in the future" is the
    /// whole point of the case, and a hard-coded date silently becomes a test
    /// of the opposite behaviour once that date passes.
    #[test]
    fn a_cancellation_is_end_of_period_not_immediate() {
        let ends = pro_core::days_from_now(20);
        let event = json!({
            "meta": { "event_name": "subscription_cancelled", "custom_data": {} },
            "data": { "id": "1", "attributes": {
                "status": "cancelled",
                "variant_id": 812345,
                "cancelled": true,
                "ends_at": ends
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            ls().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        // Still entitled, because `ends_at` is in the future. Reporting
        // `canceled` here is what used to suspend the site the moment somebody
        // cancelled, in the middle of a month they had paid for —
        // `is_entitled` does not include `canceled`.
        assert_eq!(state.status, "active", "the period they paid for is not over");
        // But it will not renew, which is what the dashboard shows.
        assert_eq!(state.cancel_at_period_end, Some(true));
        assert_eq!(state.current_period_end.as_deref(), Some(ends.as_str()));
        // And nothing has ended yet, so nothing is stamped as ended.
        assert_eq!(state.canceled_at, None);
    }

    /// A cancellation whose period has already run out is genuinely over, and
    /// must not be softened into an entitlement.
    #[test]
    fn a_cancellation_in_the_past_is_not_entitled() {
        let event = json!({
            "meta": { "event_name": "subscription_cancelled", "custom_data": {} },
            "data": { "id": "1", "attributes": {
                "status": "cancelled",
                "variant_id": 812345,
                "cancelled": true,
                "ends_at": "2020-01-01T00:00:00.000000Z"
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            ls().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "canceled");
        assert_eq!(state.canceled_at.as_deref(), Some("2020-01-01T00:00:00Z"));
    }

    /// A cancellation with no end date at all is immediate — there is no
    /// period left to honour, so inventing one would give the product away.
    #[test]
    fn a_cancellation_with_no_end_date_is_immediate() {
        let event = json!({
            "meta": { "event_name": "subscription_cancelled", "custom_data": {} },
            "data": { "id": "1", "attributes": {
                "status": "cancelled",
                "variant_id": 812345,
                "cancelled": true
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            ls().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "canceled");
    }

    /// This provider's `unpaid` is the *end* of dunning, unlike Stripe's.
    #[test]
    fn unpaid_is_terminal_here() {
        let event = json!({
            "meta": { "event_name": "subscription_updated", "custom_data": {} },
            "data": { "id": "1", "attributes": { "status": "unpaid", "variant_id": 812345 }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            ls().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "expired");
    }

    /// The regression guard for the worst failure this integration can have.
    ///
    /// `subscription_payment_recovered` is a previously *failed* payment
    /// finally going through. Unhandled, it falls to `Ignored`, the
    /// subscription stays `past_due`, and the housekeeping sweep suspends a
    /// site whose owner has already paid.
    #[test]
    fn a_recovered_payment_clears_dunning() {
        let event = json!({
            "meta": { "event_name": "subscription_payment_recovered" },
            "data": { "id": "inv_7", "attributes": { "subscription_id": 998877 } }
        });
        assert_eq!(
            ls().interpret(&event, &catalogue()).unwrap(),
            BillingEvent::PaymentSucceeded {
                provider_subscription_id: "998877".into()
            },
            "a recovered payment must be treated as money arriving"
        );
    }

    /// A refund is money going back out, so it enters dunning like a decline
    /// rather than ending the subscription on the spot.
    #[test]
    fn a_refunded_payment_enters_dunning() {
        let event = json!({
            "meta": { "event_name": "subscription_payment_refunded" },
            "data": { "id": "inv_8", "attributes": { "subscription_id": 998877 } }
        });
        assert_eq!(
            ls().interpret(&event, &catalogue()).unwrap(),
            BillingEvent::PaymentFailed {
                provider_subscription_id: "998877".into()
            }
        );
    }

    /// An upgrade arrives as `subscription_plan_changed`, carrying the new
    /// variant. Ignored, the customer pays for Creator and keeps Starter's
    /// limits.
    #[test]
    fn a_plan_change_moves_the_tenant() {
        let event = json!({
            "meta": {
                "event_name": "subscription_plan_changed",
                "custom_data": { "account_id": "acc_1", "tenant_id": "ten_1" }
            },
            "data": { "id": "998877", "attributes": {
                "status": "active",
                "variant_id": 812346,
                "renews_at": "2026-03-01T00:00:00.000000Z",
                "cancelled": false
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            ls().interpret(&event, &catalogue()).unwrap()
        else {
            panic!("a plan change is a subscription change")
        };
        assert_eq!(state.plan_slug.as_deref(), Some("creator"), "the new variant's plan");
        assert_eq!(state.status, "active");
        assert_eq!(state.tenant_id.as_deref(), Some("ten_1"));
    }

    /// The period start is never guessed. Lemon Squeezy reports only the next
    /// renewal, and deriving a start from it would move the window the email
    /// allowance is counted in.
    #[test]
    fn the_period_start_is_left_to_the_checkout() {
        let event = json!({
            "meta": { "event_name": "subscription_updated", "custom_data": {} },
            "data": { "id": "1", "attributes": {
                "status": "active",
                "variant_id": 812345,
                "renews_at": "2026-02-01T00:00:00.000000Z"
            }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            ls().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.current_period_start, None, "absent means unchanged");
        assert_eq!(state.current_period_end.as_deref(), Some("2026-02-01T00:00:00Z"));
    }

    #[test]
    fn interprets_payment_outcomes() {
        let failed = json!({
            "meta": { "event_name": "subscription_payment_failed" },
            "data": { "id": "p1", "attributes": { "subscription_id": 998877 } }
        });
        assert_eq!(
            ls().interpret(&failed, &catalogue()).unwrap(),
            BillingEvent::PaymentFailed { provider_subscription_id: "998877".into() }
        );

        let paid = json!({
            "meta": { "event_name": "subscription_payment_success" },
            "data": { "id": "p1", "attributes": { "subscription_id": 998877 } }
        });
        assert_eq!(
            ls().interpret(&paid, &catalogue()).unwrap(),
            BillingEvent::PaymentSucceeded { provider_subscription_id: "998877".into() }
        );
    }

    /// A variant id the catalogue does not know falls back to the custom data
    /// rather than losing the plan.
    #[test]
    fn falls_back_to_custom_data_for_an_unknown_variant() {
        let event = json!({
            "meta": {
                "event_name": "subscription_updated",
                "custom_data": { "plan_slug": "starter" }
            },
            "data": { "id": "1", "attributes": { "status": "active", "variant_id": 999999 }}
        });
        let BillingEvent::SubscriptionChanged(state) =
            ls().interpret(&event, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.plan_slug.as_deref(), Some("starter"));
    }

    /// A variant id sent as a string must resolve the same as a numeric one.
    #[test]
    fn variant_ids_resolve_whether_numeric_or_string() {
        for variant in [json!(812345), json!("812345")] {
            let event = json!({
                "meta": { "event_name": "subscription_updated", "custom_data": {} },
                "data": { "id": "1", "attributes": { "status": "active", "variant_id": variant }}
            });
            let BillingEvent::SubscriptionChanged(state) =
                ls().interpret(&event, &catalogue()).unwrap()
            else {
                panic!()
            };
            assert_eq!(state.plan_slug.as_deref(), Some("starter"));
        }
    }

    #[test]
    fn ignores_unrelated_events() {
        for kind in ["order_created", "license_key_created", "subscription_plan_changed_elsewhere"]
        {
            let event = json!({"meta": {"event_name": kind}, "data": {"attributes": {}}});
            assert!(matches!(
                ls().interpret(&event, &catalogue()).unwrap(),
                BillingEvent::Ignored { .. }
            ));
        }
    }

    #[test]
    fn an_unmappable_status_is_an_error() {
        let event = json!({
            "meta": { "event_name": "subscription_updated", "custom_data": {} },
            "data": { "id": "1", "attributes": { "status": "brand_new", "variant_id": 812345 }}
        });
        assert!(ls().interpret(&event, &catalogue()).is_err());
    }

    #[test]
    fn requires_the_signature_header() {
        assert!(ls().verify_webhook(b"{}", &WebhookHeaders::default()).is_err());
    }

    #[test]
    fn verifies_a_real_signature() {
        let body = br#"{"meta":{"event_name":"subscription_updated"}}"#;
        let sig = signature::hmac_sha256_hex(b"ls_secret", body);
        let headers = WebhookHeaders { x_signature: Some(sig), ..Default::default() };
        assert!(ls().verify_webhook(body, &headers).is_ok());
    }

    #[test]
    fn normalises_microsecond_dates() {
        assert_eq!(
            normalise_date(Some(&json!("2026-02-01T00:00:00.000000Z"))).as_deref(),
            Some("2026-02-01T00:00:00Z")
        );
    }
}
