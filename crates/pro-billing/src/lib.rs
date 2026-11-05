//! `pro-billing` — taking money for the platform, through Stripe, Paddle,
//! Lemon Squeezy or Gumroad.
//!
//! Only the platform's own subscriptions are here. A *tenant's* member
//! payments are that site's business and run through its own Ghost (today,
//! Stripe Connect — gated by the `paid_memberships` plan flag).
//!
//! ## The shape of the abstraction
//!
//! Four providers, one [`Provider`] trait, and the deliberate choice to keep
//! that trait tiny. It does four things:
//!
//! 1. start a checkout for a plan,
//! 2. verify a webhook's signature,
//! 3. translate a webhook into a [`BillingEvent`],
//! 4. produce a link where a customer manages their subscription.
//!
//! Everything else — entitlement, dunning, which site a subscription pays for
//! — is provider-neutral and lives in `pro-db` and `pro-web`. The providers
//! disagree about almost everything (Stripe has subscription objects with
//! period ends; Paddle sends transactions and subscriptions separately; Lemon
//! Squeezy wraps everything in JSON:API; Gumroad has no checkout API, posts
//! form-encoded bodies and signs nothing) so the translation has to happen at
//! the edge, once, and nothing downstream should be able to tell which
//! provider a customer is on.
//!
//! ## What a provider may not be able to do
//!
//! The trait is uniform; the providers are not. [`Provider::can_cancel`] is
//! the one capability a caller has to ask about, because Gumroad's API has no
//! cancellation endpoint — and a dashboard that renders a Cancel button for a
//! provider that cannot honour it is worse than one that explains where to
//! cancel instead.
//!
//! ## Why the statuses are normalised
//!
//! Each provider names subscription states differently, and some have states
//! the others do not. [`normalise_status`] maps all of them onto the six this
//! platform recognises. A status that arrives unmapped is a bug, and
//! `pro_db::subscriptions::upsert` refuses it rather than storing it — because
//! a status nothing recognises silently means "not entitled", and the
//! consequence is a paying customer's site going dark.
use pro_core::{Effective, ProError, Result};

pub mod gumroad;
pub mod lemonsqueezy;
pub mod paddle;
pub mod signature;
pub mod stripe;

/// What a webhook turned out to mean.
///
/// A small closed set, because the platform only reacts to a handful of
/// things. Everything else a provider sends — an invoice finalised, a payment
/// method attached — is acknowledged and ignored, which is why
/// [`BillingEvent::Ignored`] exists rather than an error.
#[derive(Debug, Clone, PartialEq)]
pub enum BillingEvent {
    /// A subscription was created or changed. Carries everything known about
    /// it; absent fields mean "unchanged".
    SubscriptionChanged(SubscriptionState),
    /// A payment failed. Starts the dunning clock.
    PaymentFailed { provider_subscription_id: String },
    /// A payment succeeded, which also clears dunning.
    PaymentSucceeded { provider_subscription_id: String },
    /// Understood, and nothing for the platform to do.
    Ignored { event_type: String },
}

/// A subscription as a provider describes it, in this platform's vocabulary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubscriptionState {
    pub provider_subscription_id: String,
    pub provider_customer_id: Option<String>,
    /// One of `pro_db::subscriptions::STATUSES`.
    pub status: String,
    /// The plan, recovered from the provider's price id via the catalogue.
    pub plan_slug: Option<String>,
    pub current_period_start: Option<String>,
    pub current_period_end: Option<String>,
    pub trial_ends_at: Option<String>,
    pub cancel_at_period_end: Option<bool>,
    pub canceled_at: Option<String>,
    /// Passed through checkout so a webhook can find the account that started
    /// it. All three providers support some form of custom metadata, and this
    /// is the only thing the platform puts in it.
    pub account_id: Option<String>,
    pub tenant_id: Option<String>,
}

/// Where a customer is sent to pay.
#[derive(Debug, Clone)]
pub struct Checkout {
    /// The URL to redirect to.
    pub url: String,
}

/// What a checkout needs to know.
pub struct CheckoutRequest<'a> {
    pub account_id: &'a str,
    pub account_email: &'a str,
    /// Set when the subscription is for a site that already exists.
    pub tenant_id: Option<&'a str>,
    pub plan: &'a pro_core::Plan,
    /// Where the provider returns the customer on success and on cancel.
    pub success_url: &'a str,
    pub cancel_url: &'a str,
    /// Trial days, if the plan grants them and the platform is not demanding
    /// payment up front.
    pub trial_days: i64,
}

/// One payment provider.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// `stripe`, `paddle`, `lemonsqueezy`, `gumroad`.
    fn name(&self) -> &'static str;

    /// Begin a checkout.
    async fn create_checkout(&self, req: CheckoutRequest<'_>) -> Result<Checkout>;

    /// Verify the signature on a raw webhook body.
    ///
    /// Takes the **raw bytes**, not parsed JSON. Every one of these providers
    /// signs the exact body it sent, so re-serialising parsed JSON changes the
    /// bytes — different key order, different whitespace — and the signature
    /// stops matching. This is the single commonest way webhook verification
    /// is broken.
    fn verify_webhook(&self, body: &[u8], headers: &WebhookHeaders) -> Result<()>;

    /// Turn a verified raw body into the JSON the rest of the pipeline reads.
    ///
    /// Three of the four providers post JSON, so that is the default. Gumroad
    /// posts `application/x-www-form-urlencoded` and overrides this to expand
    /// its bracketed keys into nested JSON — which is why interpretation can
    /// stay a single pointer-reading step for every provider.
    ///
    /// Runs *after* verification, never before: a body that failed its check
    /// must not be parsed at all.
    fn parse_body(&self, body: &[u8]) -> Result<serde_json::Value> {
        serde_json::from_slice(body)
            .map_err(|e| ProError::BadRequest(format!("webhook body is not JSON: {e}")))
    }

    /// The provider's own id for this event, for idempotency. Empty when the
    /// provider does not supply one.
    fn event_id(&self, body: &serde_json::Value) -> String;

    /// What this event means for the platform.
    fn interpret(&self, body: &serde_json::Value, catalogue: &[pro_core::Plan])
        -> Result<BillingEvent>;

    /// A link where the customer can change card or cancel.
    ///
    /// `None` when the provider has no such thing reachable from here, in
    /// which case the dashboard shows the cancel controls it implements
    /// itself.
    async fn management_url(&self, subscription: &pro_core::Subscription) -> Result<Option<String>>;

    /// End this subscription.
    ///
    /// The platform always *asks* for end-of-period cancellation — a
    /// customer who paid for a month keeps it — and returns whether that is
    /// what happened: `true` means the subscription runs to
    /// `current_period_end` and the provider will say so in a webhook when
    /// it lands, `false` means it ended now. Providers with no scheduled
    /// cancellation end it immediately, which the caller words honestly
    /// rather than pretending there is a grace period.
    async fn cancel_subscription(&self, subscription: &pro_core::Subscription) -> Result<bool>;

    /// Whether [`Provider::cancel_subscription`] can actually do anything.
    ///
    /// True for every provider with a cancellation endpoint, which is all of
    /// them but Gumroad. The dashboard asks before drawing a Cancel button, so
    /// a customer is never offered an action the platform cannot carry out.
    fn can_cancel(&self) -> bool {
        true
    }
}

/// The headers a webhook verification may need, lifted out of the HTTP layer
/// so a provider can be tested without one.
#[derive(Debug, Clone, Default)]
pub struct WebhookHeaders {
    /// Stripe's `Stripe-Signature`.
    pub stripe_signature: Option<String>,
    /// Paddle's `Paddle-Signature`.
    pub paddle_signature: Option<String>,
    /// Lemon Squeezy's `X-Signature`.
    pub x_signature: Option<String>,
    /// A secret from the webhook URL's own query string, for a provider that
    /// signs nothing at all.
    ///
    /// Gumroad is the reason this exists: there is no header to verify, so the
    /// operator puts a token in the ping URL and that token *is* the
    /// authentication. It travels here rather than as a header because it is
    /// the URL the provider was configured with, not something it sends.
    pub shared_secret: Option<String>,
}

/// Build the provider the platform is currently selling through.
///
/// `None` for `billing.provider = "none"`, which is a platform that does not
/// charge — an internal deployment, or one where the operator provisions every
/// tenant by hand. Signup still works; it just never asks for a card.
pub fn active(settings: &Effective) -> Option<Box<dyn Provider>> {
    build(&settings.billing_provider(), settings)
}

/// Build a named provider, whether or not it is the active one.
///
/// Webhooks are accepted from every *configured* provider, not only the
/// active one: a subscription created at Stripe keeps sending events there
/// after new signups move to Paddle, and dropping those would mean missing
/// renewals and cancellations for every existing customer.
pub fn build(name: &str, settings: &Effective) -> Option<Box<dyn Provider>> {
    match name {
        "stripe" => {
            let cfg = settings.stripe();
            cfg.secret_key.map(|key| {
                Box::new(stripe::Stripe::new(key, cfg.webhook_secret)) as Box<dyn Provider>
            })
        }
        "paddle" => {
            let cfg = settings.paddle();
            cfg.api_key.map(|key| {
                Box::new(paddle::Paddle::new(key, cfg.webhook_secret, cfg.sandbox))
                    as Box<dyn Provider>
            })
        }
        "lemonsqueezy" => {
            let cfg = settings.lemonsqueezy();
            match (cfg.api_key, cfg.store_id) {
                (Some(key), Some(store)) => Some(Box::new(lemonsqueezy::LemonSqueezy::new(
                    key,
                    store,
                    cfg.webhook_secret,
                    cfg.test_mode,
                )) as Box<dyn Provider>),
                _ => None,
            }
        }
        // The ping token is required, not optional: Gumroad signs nothing, so
        // a Gumroad built without one would refuse every ping it received.
        // Refusing to build is the visible version of the same outcome.
        "gumroad" => {
            let cfg = settings.gumroad();
            match (cfg.access_token, cfg.ping_token) {
                (Some(token), Some(ping)) => {
                    Some(Box::new(gumroad::Gumroad::new(token, cfg.subdomain, Some(ping)))
                        as Box<dyn Provider>)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Map a provider's status onto the six this platform stores.
///
/// Returns `None` for anything unrecognised, and the caller treats that as a
/// bug worth logging rather than guessing — guessing "not entitled" takes a
/// paying customer's site offline, and guessing "entitled" gives away the
/// product.
pub fn normalise_status(provider: &str, raw: &str) -> Option<&'static str> {
    let s = raw.trim().to_ascii_lowercase();
    Some(match (provider, s.as_str()) {
        // ---- Stripe -------------------------------------------------------
        (_, "trialing") => "trialing",
        (_, "active") => "active",
        (_, "past_due") => "past_due",
        (_, "paused") => "paused",
        (_, "canceled") | (_, "cancelled") => "canceled",
        // Stripe's `incomplete` means the first payment has not completed.
        // Treated as past_due rather than active: nothing has been paid, but
        // the customer is mid-flow and the dunning clock is the right
        // mechanism.
        ("stripe", "incomplete") => "past_due",
        ("stripe", "incomplete_expired") => "expired",
        ("stripe", "unpaid") => "past_due",
        // Paddle needs no arms of its own: `trialing`, `active`, `past_due`,
        // `paused` and `canceled` are spelled exactly as above, which is why
        // those arms are keyed on `_` rather than per provider.
        // ---- Lemon Squeezy ------------------------------------------------
        ("lemonsqueezy", "on_trial") => "trialing",
        // Lemon Squeezy's `unpaid` is the end of its dunning, not the start.
        ("lemonsqueezy", "unpaid") => "expired",
        ("lemonsqueezy", "expired") => "expired",
        // Gumroad's own vocabulary never reaches here: its pings carry no
        // status field, so `gumroad.rs` decides the status from *which*
        // resource arrived and sets one of these six directly. The arms
        // below are what makes that output storable all the same.
        (_, "expired") => "expired",
        _ => return None,
    })
}

/// Find the plan a provider's price id belongs to.
///
/// The reverse of `Plan::price_id`. A webhook names a price, and the platform
/// has to get back to a plan to know which limits to write.
pub fn plan_for_price<'a>(
    catalogue: &'a [pro_core::Plan],
    provider: &str,
    price_id: &str,
) -> Option<&'a pro_core::Plan> {
    catalogue.iter().find(|p| p.price_id(provider) == Some(price_id))
}

/// Require a provider, with a message that says what to configure.
pub fn require(settings: &Effective) -> Result<Box<dyn Provider>> {
    active(settings).ok_or_else(|| {
        ProError::Config(format!(
            "billing.provider is {:?} but it is not fully configured",
            settings.billing_provider()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn settings(overrides: serde_json::Value) -> Effective {
        let mut cfg: pro_core::Config = serde_json::from_value(json!({
            "url": "https://pro.example.com",
            "subdomainBase": "sites.example.com",
        }))
        .unwrap();
        cfg.plans = pro_core::plans::default_plans();
        let map = overrides
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Effective::new(Arc::new(cfg), map)
    }

    #[test]
    fn no_provider_means_no_billing() {
        let s = settings(json!({"billing.provider": "none"}));
        assert!(active(&s).is_none());
        assert!(require(&s).is_err());
    }

    #[test]
    fn builds_each_provider_from_its_credentials() {
        let s = settings(json!({
            "billing.provider": "stripe",
            "billing.stripe.secretKey": "sk_test_1",
        }));
        assert_eq!(active(&s).unwrap().name(), "stripe");

        let p = settings(json!({
            "billing.provider": "paddle",
            "billing.paddle.apiKey": "pdl_1",
        }));
        assert_eq!(active(&p).unwrap().name(), "paddle");

        let l = settings(json!({
            "billing.provider": "lemonsqueezy",
            "billing.lemonsqueezy.apiKey": "ls_1",
            "billing.lemonsqueezy.storeId": "4242",
        }));
        assert_eq!(active(&l).unwrap().name(), "lemonsqueezy");
    }

    /// Lemon Squeezy needs a store as well as a key, and half a configuration
    /// must not produce a provider that fails at checkout.
    #[test]
    fn a_half_configured_provider_is_not_built() {
        let s = settings(json!({
            "billing.provider": "lemonsqueezy",
            "billing.lemonsqueezy.apiKey": "ls_1",
        }));
        assert!(active(&s).is_none());
    }

    /// The reason webhooks keep working through a provider migration: a
    /// non-active provider is still buildable while its keys are present.
    #[test]
    fn a_configured_but_inactive_provider_is_still_buildable() {
        let s = settings(json!({
            "billing.provider": "paddle",
            "billing.paddle.apiKey": "pdl_1",
            "billing.stripe.secretKey": "sk_test_1",
        }));
        assert_eq!(active(&s).unwrap().name(), "paddle");
        assert_eq!(build("stripe", &s).unwrap().name(), "stripe", "old subscriptions keep working");
        assert!(build("lemonsqueezy", &s).is_none());
    }

    #[test]
    fn normalises_the_shared_statuses() {
        for provider in ["stripe", "paddle", "lemonsqueezy"] {
            assert_eq!(normalise_status(provider, "active"), Some("active"));
            assert_eq!(normalise_status(provider, "past_due"), Some("past_due"));
            assert_eq!(normalise_status(provider, "canceled"), Some("canceled"));
            // Providers differ on the spelling of cancelled.
            assert_eq!(normalise_status(provider, "cancelled"), Some("canceled"));
            // Case and whitespace from a payload must not matter.
            assert_eq!(normalise_status(provider, " ACTIVE "), Some("active"));
        }
    }

    /// Each provider's own vocabulary for a trial.
    #[test]
    fn normalises_trials_per_provider() {
        assert_eq!(normalise_status("stripe", "trialing"), Some("trialing"));
        assert_eq!(normalise_status("paddle", "trialing"), Some("trialing"));
        assert_eq!(normalise_status("lemonsqueezy", "on_trial"), Some("trialing"));
    }

    /// `unpaid` means opposite things at Stripe and Lemon Squeezy: Stripe is
    /// still dunning, Lemon Squeezy has given up. Mapping both to one status
    /// would either suspend a customer early or never suspend them.
    #[test]
    fn unpaid_means_different_things_per_provider() {
        assert_eq!(normalise_status("stripe", "unpaid"), Some("past_due"));
        assert_eq!(normalise_status("lemonsqueezy", "unpaid"), Some("expired"));
    }

    /// Stripe's `incomplete` is mid-flow, not a failure.
    #[test]
    fn stripe_incomplete_enters_dunning() {
        assert_eq!(normalise_status("stripe", "incomplete"), Some("past_due"));
        assert_eq!(normalise_status("stripe", "incomplete_expired"), Some("expired"));
    }

    /// An unmapped status must be visible as such, not guessed at — guessing
    /// either takes a paying site offline or gives the product away.
    #[test]
    fn an_unknown_status_is_not_guessed() {
        assert_eq!(normalise_status("stripe", "quantum_superposition"), None);
        assert_eq!(normalise_status("paddle", ""), None);
    }

    /// Every status this module produces must be one the database accepts.
    #[test]
    fn normalised_statuses_are_all_storable() {
        let raws = [
            "trialing", "active", "past_due", "paused", "canceled", "cancelled", "expired",
            "incomplete", "incomplete_expired", "unpaid", "on_trial",
        ];
        for provider in ["stripe", "paddle", "lemonsqueezy"] {
            for raw in raws {
                if let Some(mapped) = normalise_status(provider, raw) {
                    assert!(
                        pro_db::subscriptions::STATUSES.contains(&mapped),
                        "{provider}/{raw} mapped to {mapped}, which the database refuses"
                    );
                }
            }
        }
    }

    /// A webhook names a price; the platform has to get back to a plan to
    /// know which limits to write.
    #[test]
    fn resolves_a_plan_from_a_price_id() {
        let mut plans = pro_core::plans::default_plans();
        plans[1].price_ids.insert("stripe".into(), "price_creator".into());
        plans[1].price_ids.insert("paddle".into(), "pri_creator".into());

        assert_eq!(
            plan_for_price(&plans, "stripe", "price_creator").map(|p| p.slug.as_str()),
            Some("creator")
        );
        assert_eq!(
            plan_for_price(&plans, "paddle", "pri_creator").map(|p| p.slug.as_str()),
            Some("creator")
        );
        // A price id is only meaningful at its own provider.
        assert!(plan_for_price(&plans, "paddle", "price_creator").is_none());
        assert!(plan_for_price(&plans, "stripe", "price_unknown").is_none());
    }
}
