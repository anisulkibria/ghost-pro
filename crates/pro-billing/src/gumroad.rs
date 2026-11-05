//! Gumroad.
//!
//! The structural odd one out, and the differences are not cosmetic. Each of
//! them is a thing the other three providers do that Gumroad simply does not:
//!
//! * **No checkout API.** Stripe, Paddle and Lemon Squeezy are all asked to
//!   *create* a checkout and hand back a URL. Gumroad has no such call: a
//!   customer is sent to the product's own permalink, with the platform's
//!   metadata appended as query parameters. So nothing is reserved
//!   server-side, [`Provider::create_checkout`] performs no network request at
//!   all, and a plan's `priceIds.gumroad` is a product **permalink** rather
//!   than an id.
//!
//! * **Pings are form-encoded.** Everything else here speaks JSON. Gumroad
//!   posts `application/x-www-form-urlencoded`, with nesting expressed in the
//!   key — `url_params[account_id]`. [`Provider::parse_body`] is overridden to
//!   expand that back into the nested JSON the rest of the pipeline reads with
//!   pointers.
//!
//! * **Pings are not signed.** There is no signature header of any kind, so
//!   there is nothing to verify cryptographically. What authenticates a ping
//!   is a secret the operator invents and appends to the ping URL they paste
//!   into Gumroad (`/webhooks/gumroad?token=…`), compared here in constant
//!   time. That makes two things load-bearing that are merely prudent
//!   elsewhere: the token, and the event-id idempotency in
//!   `pro_db::audit::claim_webhook` — because a captured ping could otherwise
//!   be replayed for as long as the token lives.
//!
//! * **No cancellation endpoint, and no portal link.** Gumroad's public API
//!   exposes neither, so [`Provider::can_cancel`] is false and the dashboard
//!   says where to cancel instead of offering a button that cannot work.
//!
//! It is offered regardless because it is the shortest path from nothing to
//! charging: one Gumroad product per plan and a URL, with Gumroad as merchant
//! of record handling tax.
use pro_core::{ids, Plan, ProError, Result, Subscription};
use serde_json::{Map, Value};

use crate::{
    BillingEvent, Checkout, CheckoutRequest, Provider, SubscriptionState, WebhookHeaders,
};

const API: &str = "https://api.gumroad.com/v2";

pub struct Gumroad {
    access_token: String,
    subdomain: Option<String>,
    ping_token: Option<String>,
}

impl Gumroad {
    pub fn new(
        access_token: String,
        subdomain: Option<String>,
        ping_token: Option<String>,
    ) -> Self {
        Self { access_token, subdomain, ping_token }
    }

    /// Where a product lives. A seller subdomain is the usual form; the bare
    /// `gumroad.com/l/…` works too and is the fallback when none is set.
    fn product_url(&self, permalink: &str) -> String {
        match self.subdomain.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(sub) => format!("https://{sub}.gumroad.com/l/{permalink}"),
            None => format!("https://gumroad.com/l/{permalink}"),
        }
    }
}

/// Percent-encode one query value.
///
/// RFC 3986's unreserved set is left alone — `NON_ALPHANUMERIC` would encode
/// `-`, `_`, `.` and `~` as well, which turns an account id of `acc_1` into
/// `acc%5F1`. Legal, and Gumroad would decode it back, but it makes every
/// checkout URL unreadable in a log and in the customer's address bar for no
/// benefit.
fn q(value: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const QUERY: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    utf8_percent_encode(value, QUERY).to_string()
}

/// Expand a form-encoded body into nested JSON.
///
/// Gumroad spells nesting in the key: `url_params[account_id]=acc_1` and
/// `variants[Tier]=Creator`. Flattening those to literal keys would mean every
/// read downstream had to know the bracket spelling, so one pass here turns
/// `a[b]` into `{"a": {"b": …}}` and everything after this point uses ordinary
/// JSON pointers.
///
/// Repeated keys keep the **first** value. Gumroad does not send repeats, and
/// last-wins would let a crafted body append `url_params[account_id]` a second
/// time to override the real one.
fn form_to_json(body: &[u8]) -> Value {
    let mut root = Map::new();
    for (key, value) in form_urlencoded::parse(body) {
        let key = key.into_owned();
        let value = Value::String(value.into_owned());

        match key.split_once('[') {
            // `outer[inner]` — and only one level, which is all Gumroad uses.
            Some((outer, rest)) if rest.ends_with(']') => {
                let inner = rest.trim_end_matches(']').to_string();
                let slot = root
                    .entry(outer.to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Some(map) = slot.as_object_mut() {
                    map.entry(inner).or_insert(value);
                }
            }
            _ => {
                root.entry(key).or_insert(value);
            }
        }
    }
    Value::Object(root)
}

/// Gumroad sends booleans as the strings `"true"`/`"false"`, and sometimes
/// omits a false flag entirely.
fn flag(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
        _ => false,
    }
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Gumroad's timestamps are ISO-8601 already; normalised to the one spelling
/// the database stores so a renewal date compares equal across providers.
fn date(value: Option<&Value>) -> Option<String> {
    let raw = text(value?)?;
    pro_core::parse_iso(&raw)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .or(Some(raw))
}

/// Which `resource_name` a ping is for.
///
/// Gumroad does not name the event in the body the way the others do: a sale
/// ping carries no `resource_name` at all, and the subscription resources each
/// carry their own fields. The shape is what identifies it.
fn resource_of(body: &Value) -> &'static str {
    if text(&body["resource_name"]).is_some() {
        // Later Gumroad deliveries do include it, and it is authoritative.
        return match body["resource_name"].as_str().unwrap_or_default().trim() {
            "sale" => "sale",
            "cancellation" => "cancellation",
            "subscription_updated" => "subscription_updated",
            "subscription_ended" => "subscription_ended",
            "subscription_restarted" => "subscription_restarted",
            "refund" => "refund",
            "dispute" => "dispute",
            "dispute_won" => "dispute_won",
            // A resource this version does not know. Deliberately *not*
            // inferred and *not* assumed to be a sale: Gumroad naming
            // something new must not be read as a purchase, which would
            // entitle a site off an event nobody has looked at.
            _ => "unknown",
        };
    }
    // Otherwise infer it, in the order that makes each shape unambiguous.
    if body.get("ended_at").is_some() || text(&body["ended_reason"]).is_some() {
        "subscription_ended"
    } else if body.get("restarted_at").is_some() {
        "subscription_restarted"
    } else if body.get("cancelled_at").is_some() || flag(&body["cancelled"]) {
        "cancellation"
    } else if text(&body["new_plan_tier"]).is_some() || text(&body["type"]).is_some() {
        "subscription_updated"
    } else if flag(&body["disputed"]) {
        "dispute"
    } else if flag(&body["refunded"]) {
        "refund"
    } else {
        "sale"
    }
}

/// The metadata the platform appended to the checkout URL, which Gumroad
/// returns under `url_params`.
///
/// A membership's renewal ping months later carries the same `url_params` as
/// the original sale, which is the whole reason the account is identified this
/// way rather than by email — an email can change, and two accounts can share
/// one.
fn param(body: &Value, name: &str) -> Option<String> {
    body.pointer(&format!("/url_params/{name}"))
        .and_then(text)
        // Gumroad also surfaces seller-defined custom fields, which an
        // operator may have set up instead.
        .or_else(|| body.pointer(&format!("/custom_fields/{name}")).and_then(text))
}

#[async_trait::async_trait]
impl Provider for Gumroad {
    fn name(&self) -> &'static str {
        "gumroad"
    }

    /// Build the URL a customer is sent to. No network request: Gumroad has no
    /// checkout object to create.
    ///
    /// The trial days a plan grants are **not** passed on. Gumroad's free-trial
    /// period is a property of the product, set in its own dashboard, and
    /// there is no per-checkout override — so a trial is configured once,
    /// there, and `require_payment_up_front` is the only knob that still
    /// applies here.
    async fn create_checkout(&self, req: CheckoutRequest<'_>) -> Result<Checkout> {
        let permalink = req.plan.price_id("gumroad").ok_or_else(|| {
            ProError::Config(format!(
                "plan {:?} has no Gumroad product permalink — set \
                 plans[].priceIds.gumroad to the part after /l/ in the product's url",
                req.plan.slug
            ))
        })?;

        // `wanted=true` skips the product page and opens the checkout, which
        // is what a customer who already chose a plan expects.
        let mut url = format!(
            "{base}?wanted=true&email={email}&url_params%5Baccount_id%5D={account}\
             &url_params%5Bplan_slug%5D={plan}",
            base = self.product_url(permalink),
            email = q(req.account_email),
            account = q(req.account_id),
            plan = q(&req.plan.slug),
        );
        if let Some(tenant) = req.tenant_id {
            url.push_str(&format!("&url_params%5Btenant_id%5D={}", q(tenant)));
        }
        // Gumroad does not take a cancel URL, and its own success redirect is
        // a product setting, so `success_url` is appended for the operator who
        // has configured the product to pass it through. Harmless otherwise.
        url.push_str(&format!("&url_params%5Breturn_to%5D={}", q(req.success_url)));

        Ok(Checkout { url })
    }

    /// Gumroad posts form-encoded bodies, not JSON.
    fn parse_body(&self, body: &[u8]) -> Result<Value> {
        let parsed = form_to_json(body);
        if parsed.as_object().map(Map::is_empty).unwrap_or(true) {
            return Err(ProError::BadRequest(
                "Gumroad ping body was empty or not form-encoded".into(),
            ));
        }
        Ok(parsed)
    }

    /// There is no signature to check, so the shared token in the ping URL is
    /// the whole of the authentication.
    ///
    /// Refused rather than waved through when the token is missing on either
    /// side: an unverified billing event is how a stranger grants themselves a
    /// plan, and a Gumroad with no token configured can take no renewals at
    /// all — which is a visible failure, unlike silently trusting everything.
    fn verify_webhook(&self, _body: &[u8], headers: &WebhookHeaders) -> Result<()> {
        let expected = self.ping_token.as_deref().unwrap_or("");
        if expected.is_empty() {
            return Err(ProError::Config(
                "billing.gumroad.pingToken is unset, so Gumroad pings cannot be \
                 authenticated. Gumroad does not sign them, so this secret is the only \
                 thing that can."
                    .into(),
            ));
        }
        let presented = headers.shared_secret.as_deref().unwrap_or("");
        if ids::constant_time_eq(presented, expected) {
            Ok(())
        } else {
            Err(ProError::Forbidden(
                "Gumroad ping rejected: the token in the ping URL does not match \
                 billing.gumroad.pingToken"
                    .into(),
            ))
        }
    }

    /// Gumroad supplies no event id, so one is synthesised that is stable
    /// across its retries.
    ///
    /// Without this the pipeline's idempotency would be switched off for this
    /// provider — an empty id means "always fresh" — and a retried sale would
    /// be applied twice. `sale_id` is unique per charge, including per renewal
    /// of a membership, which is exactly the grain needed. Subscription
    /// resources carry no sale, so they are keyed on what they are plus when
    /// they happened.
    fn event_id(&self, body: &Value) -> String {
        if let Some(sale) = text(&body["sale_id"]).or_else(|| text(&body["order_number"])) {
            return sale;
        }
        let resource = resource_of(body);
        let subscription = text(&body["subscription_id"]).unwrap_or_default();
        let at = date(body.get("ended_at"))
            .or_else(|| date(body.get("cancelled_at")))
            .or_else(|| date(body.get("restarted_at")))
            .or_else(|| date(body.get("sale_timestamp")))
            .unwrap_or_default();
        if subscription.is_empty() && at.is_empty() {
            // Nothing stable to key on. Better an empty id — which the claim
            // treats as fresh and therefore applies — than a fabricated one
            // that could collide with an unrelated event and swallow it.
            return String::new();
        }
        format!("{resource}:{subscription}:{at}")
    }

    fn interpret(&self, body: &Value, catalogue: &[Plan]) -> Result<BillingEvent> {
        let resource = resource_of(body);
        let event_type = resource.to_string();

        // A membership's id. A one-off purchase has none, and this platform
        // sells subscriptions — so a sale without one is acknowledged and
        // ignored rather than recorded as an entitlement that never renews.
        let subscription_id = text(&body["subscription_id"]);

        match resource {
            "sale" => {
                let Some(id) = subscription_id else {
                    return Ok(BillingEvent::Ignored { event_type });
                };

                // A renewal is a fresh charge on a subscription that already
                // exists, and it is also what clears dunning. Gumroad marks
                // it rather than sending a different resource.
                let renewal = flag(&body["is_recurring_charge"]);

                // The product permalink identifies the plan. `short_product_id`
                // and `product_id` are Gumroad's internal ids and are accepted
                // too, so an operator who configured either still resolves.
                let plan_slug = ["product_permalink", "permalink", "short_product_id", "product_id"]
                    .iter()
                    .filter_map(|k| text(&body[*k]))
                    // A permalink may arrive as a full URL; the catalogue holds
                    // the bare slug.
                    .map(|v| v.rsplit('/').next().unwrap_or(&v).to_string())
                    .find_map(|v| crate::plan_for_price(catalogue, "gumroad", &v))
                    .map(|p| p.slug.clone())
                    // Else the slug the checkout URL carried, which survives
                    // a product being renamed.
                    .or_else(|| param(body, "plan_slug"));

                Ok(BillingEvent::SubscriptionChanged(SubscriptionState {
                    provider_subscription_id: id,
                    // Gumroad's buyer identifier. `purchaser_id` exists only
                    // for buyers with a Gumroad account, so the email is the
                    // fallback — it is what the receipt is addressed to.
                    provider_customer_id: text(&body["purchaser_id"])
                        .or_else(|| text(&body["email"])),
                    status: if flag(&body["refunded"]) || flag(&body["disputed"]) {
                        // Money taken back: not entitled, and the dunning
                        // clock is the wrong mechanism for a reversal.
                        "canceled".to_string()
                    } else {
                        "active".to_string()
                    },
                    plan_slug,
                    // The charge's own date starts the period it paid for.
                    current_period_start: date(body.get("sale_timestamp")),
                    // Gumroad does not state the next charge date on a ping.
                    // Left unset rather than computed from `recurrence`:
                    // `upsert` treats absent as unchanged, and a guessed
                    // renewal date drives dunning and suspension.
                    current_period_end: None,
                    trial_ends_at: None,
                    cancel_at_period_end: Some(false),
                    canceled_at: None,
                    account_id: param(body, "account_id"),
                    tenant_id: param(body, "tenant_id"),
                }))
                .map(|event| {
                    if renewal {
                        tracing::debug!("gumroad: renewal charge on subscription");
                    }
                    event
                })
            }

            // "Cancelled" at Gumroad means the membership will not renew; the
            // customer keeps access until the period they paid for runs out,
            // and a separate `subscription_ended` arrives when it does. So
            // this is Stripe's `cancel_at_period_end`, not an immediate stop —
            // reading it as immediate would cut a paying customer off early.
            "cancellation" => {
                let Some(id) = subscription_id else {
                    return Ok(BillingEvent::Ignored { event_type });
                };
                Ok(BillingEvent::SubscriptionChanged(SubscriptionState {
                    provider_subscription_id: id,
                    status: "active".to_string(),
                    cancel_at_period_end: Some(true),
                    canceled_at: date(body.get("cancelled_at")),
                    account_id: param(body, "account_id"),
                    tenant_id: param(body, "tenant_id"),
                    ..Default::default()
                }))
            }

            // The period has actually run out, or Gumroad gave up collecting.
            "subscription_ended" => {
                let Some(id) = subscription_id else {
                    return Ok(BillingEvent::Ignored { event_type });
                };
                let reason = text(&body["ended_reason"]).unwrap_or_default();
                Ok(BillingEvent::SubscriptionChanged(SubscriptionState {
                    provider_subscription_id: id,
                    status: match reason.as_str() {
                        // Collection failed to the end of Gumroad's own
                        // retries: expired, not merely cancelled.
                        "failed_payment" => "expired".to_string(),
                        _ => "canceled".to_string(),
                    },
                    canceled_at: date(body.get("ended_at")),
                    cancel_at_period_end: Some(false),
                    account_id: param(body, "account_id"),
                    tenant_id: param(body, "tenant_id"),
                    ..Default::default()
                }))
            }

            "subscription_restarted" => {
                let Some(id) = subscription_id else {
                    return Ok(BillingEvent::Ignored { event_type });
                };
                Ok(BillingEvent::SubscriptionChanged(SubscriptionState {
                    provider_subscription_id: id,
                    status: "active".to_string(),
                    cancel_at_period_end: Some(false),
                    canceled_at: None,
                    account_id: param(body, "account_id"),
                    tenant_id: param(body, "tenant_id"),
                    ..Default::default()
                }))
            }

            // An upgrade or downgrade between tiers of the same product. The
            // tier name is not a permalink, so the plan is resolved from the
            // tier only if an operator happened to name them alike; otherwise
            // the plan is left unchanged and the *next* charge's ping, which
            // does name a product, settles it.
            "subscription_updated" => {
                let Some(id) = subscription_id else {
                    return Ok(BillingEvent::Ignored { event_type });
                };
                let tier = text(&body["new_plan_tier"]).or_else(|| text(&body["tier"]));
                let plan_slug = tier
                    .as_deref()
                    .and_then(|t| crate::plan_for_price(catalogue, "gumroad", t))
                    .map(|p| p.slug.clone());
                Ok(BillingEvent::SubscriptionChanged(SubscriptionState {
                    provider_subscription_id: id,
                    status: "active".to_string(),
                    plan_slug,
                    account_id: param(body, "account_id"),
                    tenant_id: param(body, "tenant_id"),
                    ..Default::default()
                }))
            }

            // Money reversed. Treated as a payment failure rather than an
            // immediate cancellation, so the customer gets the platform's
            // dunning grace period to resolve it — the same as a declined
            // card at any other provider.
            "refund" | "dispute" => match subscription_id {
                Some(id) => Ok(BillingEvent::PaymentFailed { provider_subscription_id: id }),
                None => Ok(BillingEvent::Ignored { event_type }),
            },

            // The seller won the dispute, so the money is theirs after all.
            "dispute_won" => match subscription_id {
                Some(id) => Ok(BillingEvent::PaymentSucceeded { provider_subscription_id: id }),
                None => Ok(BillingEvent::Ignored { event_type }),
            },

            _ => Ok(BillingEvent::Ignored { event_type }),
        }
    }

    /// Gumroad's API exposes no customer portal link.
    ///
    /// A subscriber manages their membership from the link Gumroad emails with
    /// the receipt, which is signed with a token this platform never sees and
    /// cannot mint. `None` rather than a guessed URL that would 404.
    async fn management_url(&self, _subscription: &Subscription) -> Result<Option<String>> {
        Ok(None)
    }

    /// Gumroad has no cancellation endpoint, which [`Provider::can_cancel`]
    /// declares so the dashboard never offers the button.
    ///
    /// Reached only if something calls it anyway, and then it says what to do
    /// rather than failing opaquely.
    async fn cancel_subscription(&self, _subscription: &Subscription) -> Result<bool> {
        Err(ProError::Unavailable(
            "Gumroad subscriptions are cancelled from the membership link in your Gumroad \
             receipt — this platform cannot cancel them for you."
                .into(),
        ))
    }

    fn can_cancel(&self) -> bool {
        false
    }
}

impl Gumroad {
    /// Register this platform's ping endpoint with Gumroad.
    ///
    /// Gumroad's per-product "Ping" setting only sends sales; the subscription
    /// resources have to be subscribed to through the API, one call each. Not
    /// run automatically — an operator pastes the URL into Gumroad, or calls
    /// this once — because it writes to their Gumroad account, and doing that
    /// as a side effect of saving a settings form would be a surprise.
    ///
    /// Returns the resources that were registered.
    pub async fn register_pings(&self, ping_url: &str) -> Result<Vec<&'static str>> {
        const RESOURCES: &[&str] = &[
            "sale",
            "refund",
            "dispute",
            "dispute_won",
            "cancellation",
            "subscription_updated",
            "subscription_ended",
            "subscription_restarted",
        ];

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(ProError::internal)?;

        let mut registered = Vec::new();
        for resource in RESOURCES {
            let response = client
                .put(format!("{API}/resource_subscriptions"))
                .form(&[
                    ("access_token", self.access_token.as_str()),
                    ("resource_name", resource),
                    ("post_url", ping_url),
                ])
                .send()
                .await
                .map_err(|e| ProError::Unavailable(format!("could not reach Gumroad: {e}")))?;

            let status = response.status();
            let parsed: Value = response.json().await.unwrap_or(Value::Null);
            if status.is_success() && flag(&parsed["success"]) {
                registered.push(*resource);
            } else {
                // One unsupported resource must not abort the rest: Gumroad
                // rejects resources a seller's account cannot use, and the
                // sale ping alone is already worth having.
                let message = text(&parsed["message"]).unwrap_or_else(|| status.to_string());
                tracing::warn!("gumroad: could not subscribe to {resource}: {message}");
            }
        }

        if registered.is_empty() {
            return Err(ProError::BadRequest(
                "Gumroad accepted none of the ping registrations — check \
                 billing.gumroad.accessToken"
                    .into(),
            ));
        }
        Ok(registered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gr() -> Gumroad {
        Gumroad::new("gr_token".into(), Some("acme".into()), Some("ping_secret".into()))
    }

    fn catalogue() -> Vec<Plan> {
        let mut plans = pro_core::plans::default_plans();
        plans[0].price_ids.insert("gumroad".into(), "starter-plan".into());
        plans[1].price_ids.insert("gumroad".into(), "creator-plan".into());
        plans
    }

    fn headers(token: Option<&str>) -> WebhookHeaders {
        WebhookHeaders {
            shared_secret: token.map(str::to_string),
            ..Default::default()
        }
    }

    // ---- checkout ---------------------------------------------------------

    /// There is no API call to make, so the whole of checkout is the URL.
    #[tokio::test]
    async fn builds_a_product_url_carrying_the_metadata() {
        let plans = catalogue();
        let checkout = gr()
            .create_checkout(CheckoutRequest {
                account_id: "acc_1",
                account_email: "someone@example.com",
                tenant_id: Some("ten_1"),
                plan: &plans[1],
                success_url: "https://pro.example.com/dashboard/billing",
                cancel_url: "https://pro.example.com/dashboard/billing",
                trial_days: 14,
            })
            .await
            .unwrap();

        assert!(
            checkout.url.starts_with("https://acme.gumroad.com/l/creator-plan?"),
            "got {}",
            checkout.url
        );
        assert!(checkout.url.contains("wanted=true"), "opens the checkout, not the product page");
        // The metadata a renewal months later is matched by.
        assert!(checkout.url.contains("url_params%5Baccount_id%5D=acc_1"));
        assert!(checkout.url.contains("url_params%5Btenant_id%5D=ten_1"));
        assert!(checkout.url.contains("url_params%5Bplan_slug%5D=creator"));
        // `@` is encoded because it is reserved in a query; the dots are not,
        // because they are unreserved and encoding them would only make the
        // URL harder to read.
        assert!(checkout.url.contains("email=someone%40example.com"), "got {}", checkout.url);
    }

    /// With no seller subdomain the bare gumroad.com form still works.
    #[tokio::test]
    async fn falls_back_to_the_bare_domain() {
        let plans = catalogue();
        let gr = Gumroad::new("t".into(), None, Some("s".into()));
        let checkout = gr
            .create_checkout(CheckoutRequest {
                account_id: "a",
                account_email: "a@b.c",
                tenant_id: None,
                plan: &plans[0],
                success_url: "https://x/y",
                cancel_url: "https://x/y",
                trial_days: 0,
            })
            .await
            .unwrap();
        assert!(checkout.url.starts_with("https://gumroad.com/l/starter-plan?"));
        assert!(!checkout.url.contains("tenant_id"), "no site yet, so no tenant param");
    }

    /// A plan with no Gumroad product cannot be sold, and the message has to
    /// say what to set — a permalink, not an id.
    #[tokio::test]
    async fn a_plan_with_no_permalink_is_a_configuration_error() {
        let plans = pro_core::plans::default_plans();
        let err = gr()
            .create_checkout(CheckoutRequest {
                account_id: "a",
                account_email: "a@b.c",
                tenant_id: None,
                plan: &plans[0],
                success_url: "https://x/y",
                cancel_url: "https://x/y",
                trial_days: 0,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("priceIds.gumroad"), "got {err}");
    }

    // ---- verification -----------------------------------------------------

    /// The token in the ping URL is the entire authentication, so the cases
    /// around it are the security-relevant ones.
    #[test]
    fn a_ping_is_authenticated_by_its_token() {
        assert!(gr().verify_webhook(b"", &headers(Some("ping_secret"))).is_ok());

        let wrong = gr().verify_webhook(b"", &headers(Some("not_it"))).unwrap_err();
        assert_eq!(wrong.status(), 403);

        let absent = gr().verify_webhook(b"", &headers(None)).unwrap_err();
        assert_eq!(absent.status(), 403, "no token is not a pass");

        // A near-miss must not pass: the comparison is constant-time, not a
        // prefix match.
        assert!(gr().verify_webhook(b"", &headers(Some("ping_secre"))).is_err());
        assert!(gr().verify_webhook(b"", &headers(Some("ping_secretx"))).is_err());
    }

    /// A platform that has not set a token must refuse every ping rather than
    /// accept all of them. This is the fail-closed case.
    #[test]
    fn with_no_token_configured_nothing_is_accepted() {
        let gr = Gumroad::new("t".into(), None, None);
        assert!(gr.verify_webhook(b"", &headers(None)).is_err());
        assert!(gr.verify_webhook(b"", &headers(Some(""))).is_err());
        // Not even by presenting an empty token, which an empty expectation
        // would otherwise match.
        let empty = Gumroad::new("t".into(), None, Some(String::new()));
        assert!(empty.verify_webhook(b"", &headers(Some(""))).is_err());
    }

    // ---- body parsing -----------------------------------------------------

    #[test]
    fn expands_form_encoding_into_nested_json() {
        let body = b"sale_id=s1&url_params%5Baccount_id%5D=acc_1&url_params%5Btenant_id%5D=ten_1&email=a%40b.c";
        let parsed = gr().parse_body(body).unwrap();
        assert_eq!(parsed["sale_id"], json!("s1"));
        assert_eq!(parsed["email"], json!("a@b.c"));
        // The bracketed keys became a nested object, so the rest of the
        // pipeline reads them with ordinary pointers.
        assert_eq!(parsed.pointer("/url_params/account_id").unwrap(), &json!("acc_1"));
        assert_eq!(parsed.pointer("/url_params/tenant_id").unwrap(), &json!("ten_1"));
    }

    /// A repeated key must not let a crafted body override the real value.
    #[test]
    fn the_first_value_of_a_repeated_key_wins() {
        let body = b"url_params%5Baccount_id%5D=real&url_params%5Baccount_id%5D=attacker&sale_id=a&sale_id=b";
        let parsed = gr().parse_body(body).unwrap();
        assert_eq!(parsed.pointer("/url_params/account_id").unwrap(), &json!("real"));
        assert_eq!(parsed["sale_id"], json!("a"));
    }

    #[test]
    fn an_empty_body_is_rejected() {
        assert!(gr().parse_body(b"").is_err());
    }

    // ---- idempotency ------------------------------------------------------

    /// An empty event id switches idempotency off, so a sale must always
    /// produce one — a retried ping would otherwise be applied twice.
    #[test]
    fn a_sale_is_keyed_on_its_charge() {
        assert_eq!(gr().event_id(&json!({"sale_id": "s_99"})), "s_99");
        // Each renewal is its own charge, which is the grain that lets a
        // renewal through while a retry of the same charge is caught.
        assert_eq!(
            gr().event_id(&json!({"sale_id": "s_100", "is_recurring_charge": "true"})),
            "s_100"
        );
        assert_eq!(gr().event_id(&json!({"order_number": "12345"})), "12345");
    }

    #[test]
    fn subscription_events_are_keyed_on_what_and_when() {
        let id = gr().event_id(&json!({
            "resource_name": "subscription_ended",
            "subscription_id": "sub_1",
            "ended_at": "2026-03-01T00:00:00Z"
        }));
        assert_eq!(id, "subscription_ended:sub_1:2026-03-01T00:00:00Z");
    }

    /// Nothing stable to key on: an empty id is handled as "fresh", which
    /// applies the event. A fabricated id could collide with a real one and
    /// swallow it, which is strictly worse.
    #[test]
    fn an_unkeyable_ping_gets_no_id_rather_than_a_made_up_one() {
        assert_eq!(gr().event_id(&json!({})), "");
    }

    // ---- interpretation ---------------------------------------------------

    fn sale() -> Value {
        json!({
            "sale_id": "s_1",
            "subscription_id": "sub_1",
            "product_permalink": "creator-plan",
            "email": "buyer@example.com",
            "purchaser_id": "p_9",
            "sale_timestamp": "2026-01-01T10:00:00Z",
            "recurrence": "monthly",
            "url_params": { "account_id": "acc_1", "tenant_id": "ten_1", "plan_slug": "creator" }
        })
    }

    #[test]
    fn interprets_a_membership_sale() {
        let BillingEvent::SubscriptionChanged(state) =
            gr().interpret(&sale(), &catalogue()).unwrap()
        else {
            panic!("expected a subscription change")
        };
        assert_eq!(state.provider_subscription_id, "sub_1");
        assert_eq!(state.provider_customer_id.as_deref(), Some("p_9"));
        assert_eq!(state.status, "active");
        assert_eq!(state.plan_slug.as_deref(), Some("creator"));
        assert_eq!(state.account_id.as_deref(), Some("acc_1"));
        assert_eq!(state.tenant_id.as_deref(), Some("ten_1"));
        assert_eq!(state.current_period_start.as_deref(), Some("2026-01-01T10:00:00Z"));
        // Gumroad never states the next charge date, and guessing it would
        // drive dunning off a fiction.
        assert_eq!(state.current_period_end, None);
    }

    /// A one-off purchase is not an entitlement that renews, so it is
    /// acknowledged and ignored rather than stored as a subscription.
    #[test]
    fn a_non_membership_sale_is_ignored() {
        let mut body = sale();
        body.as_object_mut().unwrap().remove("subscription_id");
        assert!(matches!(
            gr().interpret(&body, &catalogue()).unwrap(),
            BillingEvent::Ignored { .. }
        ));
    }

    /// The plan has to resolve from the permalink even when it arrives as a
    /// full URL, which Gumroad sometimes sends.
    #[test]
    fn resolves_a_plan_from_a_permalink_url() {
        let mut body = sale();
        body["product_permalink"] = json!("https://acme.gumroad.com/l/starter-plan");
        body["url_params"]["plan_slug"] = json!("wrong-on-purpose");
        let BillingEvent::SubscriptionChanged(state) =
            gr().interpret(&body, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            state.plan_slug.as_deref(),
            Some("starter"),
            "the catalogue wins over the url param"
        );
    }

    /// A product renamed at Gumroad stops matching the catalogue; the slug the
    /// checkout carried is what keeps the subscription attributable.
    #[test]
    fn falls_back_to_the_slug_the_checkout_carried() {
        let mut body = sale();
        body["product_permalink"] = json!("renamed-since");
        let BillingEvent::SubscriptionChanged(state) =
            gr().interpret(&body, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.plan_slug.as_deref(), Some("creator"));
    }

    /// Gumroad's "cancelled" is end-of-period, like Stripe's
    /// `cancel_at_period_end`. Reading it as immediate would take a paying
    /// customer's site down early.
    #[test]
    fn a_cancellation_is_end_of_period_not_immediate() {
        let body = json!({
            "resource_name": "cancellation",
            "subscription_id": "sub_1",
            "cancelled": "true",
            "cancelled_at": "2026-02-01T00:00:00Z",
            "url_params": { "account_id": "acc_1" }
        });
        let BillingEvent::SubscriptionChanged(state) =
            gr().interpret(&body, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "active", "still entitled until the period ends");
        assert_eq!(state.cancel_at_period_end, Some(true));
        assert_eq!(state.canceled_at.as_deref(), Some("2026-02-01T00:00:00Z"));
    }

    /// And when the period does end, entitlement stops.
    #[test]
    fn an_ended_subscription_stops_entitlement() {
        let ended = json!({
            "resource_name": "subscription_ended",
            "subscription_id": "sub_1",
            "ended_at": "2026-03-01T00:00:00Z",
            "ended_reason": "cancelled"
        });
        let BillingEvent::SubscriptionChanged(state) =
            gr().interpret(&ended, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "canceled");

        // Gumroad giving up on collection is `expired`, which is a different
        // thing from a customer choosing to leave.
        let failed = json!({
            "resource_name": "subscription_ended",
            "subscription_id": "sub_1",
            "ended_at": "2026-03-01T00:00:00Z",
            "ended_reason": "failed_payment"
        });
        let BillingEvent::SubscriptionChanged(state) =
            gr().interpret(&failed, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "expired");
    }

    #[test]
    fn a_restart_restores_entitlement() {
        let body = json!({
            "resource_name": "subscription_restarted",
            "subscription_id": "sub_1",
            "restarted_at": "2026-04-01T00:00:00Z"
        });
        let BillingEvent::SubscriptionChanged(state) =
            gr().interpret(&body, &catalogue()).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.status, "active");
        assert_eq!(state.cancel_at_period_end, Some(false));
    }

    /// A refund or a dispute enters dunning rather than cancelling outright,
    /// so the customer gets the same grace period as a declined card.
    #[test]
    fn money_reversed_enters_dunning() {
        for resource in ["refund", "dispute"] {
            let body = json!({"resource_name": resource, "subscription_id": "sub_1"});
            assert_eq!(
                gr().interpret(&body, &catalogue()).unwrap(),
                BillingEvent::PaymentFailed { provider_subscription_id: "sub_1".into() },
                "{resource}"
            );
        }
        let won = json!({"resource_name": "dispute_won", "subscription_id": "sub_1"});
        assert_eq!(
            gr().interpret(&won, &catalogue()).unwrap(),
            BillingEvent::PaymentSucceeded { provider_subscription_id: "sub_1".into() }
        );
    }

    /// A sale ping that already says it was refunded is a reversal, not an
    /// entitlement — and it reaches the platform two ways.
    #[test]
    fn a_refunded_sale_is_not_entitled() {
        // Unnamed, so the shape identifies it: a reversal, which enters
        // dunning like any other failed payment.
        let mut inferred = sale();
        inferred["refunded"] = json!("true");
        assert_eq!(
            gr().interpret(&inferred, &catalogue()).unwrap(),
            BillingEvent::PaymentFailed { provider_subscription_id: "sub_1".into() }
        );

        // Named as a sale, so it *is* a sale — but one whose money has gone
        // back, which must not leave the site entitled.
        let mut named = sale();
        named["resource_name"] = json!("sale");
        named["refunded"] = json!("true");
        let BillingEvent::SubscriptionChanged(state) =
            gr().interpret(&named, &catalogue()).unwrap()
        else {
            panic!("a named sale stays a sale")
        };
        assert_eq!(state.status, "canceled");
    }

    /// Gumroad's older deliveries carry no `resource_name`, so the shape has
    /// to identify the event.
    #[test]
    fn infers_the_resource_when_it_is_not_named() {
        assert_eq!(resource_of(&json!({"sale_id": "s", "subscription_id": "x"})), "sale");
        assert_eq!(resource_of(&json!({"cancelled": "true"})), "cancellation");
        assert_eq!(resource_of(&json!({"ended_at": "2026-01-01T00:00:00Z"})), "subscription_ended");
        assert_eq!(
            resource_of(&json!({"restarted_at": "2026-01-01T00:00:00Z"})),
            "subscription_restarted"
        );
        // A named resource always wins over inference.
        assert_eq!(
            resource_of(&json!({"resource_name": "cancellation", "sale_id": "s"})),
            "cancellation"
        );
        // And a name this version does not know is not quietly read as a
        // purchase — which is what would entitle a site off an unexamined
        // event.
        assert_eq!(
            resource_of(&json!({"resource_name": "something_new", "sale_id": "s"})),
            "unknown"
        );
    }

    /// Anything else is acknowledged, so Gumroad stops resending it.
    #[test]
    fn unknown_resources_are_ignored_not_errors() {
        let body = json!({"resource_name": "something_new", "subscription_id": "sub_1"});
        assert!(matches!(
            gr().interpret(&body, &catalogue()).unwrap(),
            BillingEvent::Ignored { .. }
        ));
    }

    /// The dashboard must not offer a cancel button this provider cannot
    /// honour, and the call itself must explain rather than fail opaquely.
    #[tokio::test]
    async fn cancellation_is_declared_unsupported() {
        assert!(!gr().can_cancel());
        let subscription = Subscription {
            id: "s1".into(),
            account_id: "acc_1".into(),
            tenant_id: Some("ten_1".into()),
            plan_slug: "creator".into(),
            provider: "gumroad".into(),
            provider_subscription_id: Some("sub_1".into()),
            provider_customer_id: None,
            status: "active".into(),
            current_period_start: None,
            current_period_end: None,
            trial_ends_at: None,
            cancel_at_period_end: false,
            canceled_at: None,
            past_due_since: None,
            created_at: pro_core::now_iso(),
            updated_at: pro_core::now_iso(),
        };
        assert!(gr().management_url(&subscription).await.unwrap().is_none());
        let err = gr().cancel_subscription(&subscription).await.unwrap_err();
        assert!(err.to_string().contains("Gumroad receipt"), "got {err}");
    }
}
