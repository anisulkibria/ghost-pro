//! Runtime platform settings: what the operator can change from the admin
//! console without restarting or editing a file.
//!
//! The precedence is deliberately the opposite of what a twelve-factor app
//! does, and it is worth being explicit about why:
//!
//! ```text
//! console (stored in the control-plane database)   ← wins
//! PRO_* environment variables
//! pro.json
//! built-in defaults
//! ```
//!
//! Config is the *seed*; the console is the *source of truth*. If the file won
//! instead, an operator would switch payment provider in the UI, see it save,
//! and watch it have no effect — which is a worse failure than the surprise of
//! a file being overridden. The admin screens show which layer each value came
//! from, so the surprise is visible rather than silent, and clearing a setting
//! in the console falls back to the file again.
//!
//! Every editable setting is declared once in [`REGISTRY`]. The admin console
//! renders its form from that list, validation runs from it, and secrets are
//! marked in it so they are never read back out to a browser. A new setting is
//! one entry here plus one accessor on [`Effective`] — not a new form field, a
//! new validator and a new column.
use serde_json::Value;
use std::collections::BTreeMap;

use crate::config::{
    Config, GumroadConfig, LemonSqueezyConfig, MailConfig, PaddleConfig, StripeConfig,
};
use crate::models::StaffRole;
use crate::error::{ProError, Result};

/// How a setting is edited, which is also how it is validated.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Text,
    /// Write-only. Stored, never served back: the console shows whether a
    /// value is set, not what it is. An API key that can be read back out of
    /// an admin page is an API key one XSS away from being stolen.
    Secret,
    Bool,
    /// An integer, with inclusive bounds the form enforces too.
    Int { min: i64, max: i64 },
    /// One of a fixed set. `(value, label)` pairs.
    Select(&'static [(&'static str, &'static str)]),
    /// Stored as a structured value and edited on a screen of its own rather
    /// than on the settings form.
    ///
    /// It is in the registry regardless, because that is what carries the
    /// key's minimum role — [`authorize`] refuses a key it has never heard of,
    /// so a setting outside the registry could not be saved at all. The
    /// console simply skips these when rendering the form; the screen that
    /// owns one builds its own value and saves it through the same path.
    ///
    /// The one of these today is `billing.priceIds`, which is a map of plan
    /// slug to that plan's id at each provider — a shape no single form field
    /// can hold, and the one piece of billing configuration that used to
    /// require editing `pro.json` and restarting.
    Managed,
}

/// One setting an operator may change.
pub struct SettingDef {
    /// Dotted path, matching the config field it overrides:
    /// `billing.provider`, `mail.host`. The dots are not navigation — the key
    /// is opaque and stored whole.
    pub key: &'static str,
    /// Which card the console groups this setting under. The group's name is
    /// the section heading above that card.
    pub group: &'static str,
    pub label: &'static str,
    /// Shown under the field. This is where the consequence of a setting
    /// belongs, since the operator reading it is deciding whether to change
    /// something that affects every customer.
    pub help: &'static str,
    pub kind: Kind,
    /// The lowest staff role that may *change* this.
    ///
    /// Anything that decides how the platform takes money — which provider is
    /// live, and the credentials behind it — is [`StaffRole::Owner`], so a
    /// support or administrator login cannot redirect revenue. The console
    /// renders a setting above the viewer's role as read-only rather than
    /// hiding it, because a setting that vanishes looks like a bug while one
    /// that is visibly locked explains itself.
    pub min_role: StaffRole,
    /// The value when the console has not overridden it, read from config.
    pub fallback: fn(&Config) -> Value,
}

/// The settings the admin console exposes.
///
/// Ordered as the console displays them.
pub static REGISTRY: &[SettingDef] = &[
    SettingDef {
        key: "billing.emailLimits", group: "Billing", label: "Monthly email allowances",
        help: "Newsletter and transactional email allowances, edited in Plans.",
        kind: Kind::Managed, min_role: StaffRole::Owner,
        fallback: |_| Value::Object(Default::default()),
    },
    SettingDef {
        key: "mail.autoStopAtLimit", group: "Mail", label: "Stop sending at the email allowance",
        help: "Stop tenant newsletter and transactional sends at their monthly plan allowance. Counts continue when this is off. Individual sites can override this in site details.",
        kind: Kind::Bool, min_role: StaffRole::Admin,
        fallback: |c| Value::Bool(c.mail.auto_stop_at_limit),
    },
    // ---- Billing ---------------------------------------------------------
    SettingDef {
        key: "billing.provider",
        group: "Billing",
        label: "Payment provider",
        help: "Which provider new checkouts go to. Existing subscriptions keep billing \
               wherever they were created, and webhooks from every configured provider \
               stay active — so switching here is safe mid-flight and does not \
               interrupt anyone already paying.",
        kind: Kind::Select(&[
            ("none", "No billing (provision by hand)"),
            ("stripe", "Stripe"),
            ("paddle", "Paddle"),
            ("lemonsqueezy", "Lemon Squeezy"),
            ("gumroad", "Gumroad"),
        ]),
        min_role: StaffRole::Owner,
        fallback: |c| Value::String(c.billing.provider.clone()),
    },
    SettingDef {
        key: "billing.requirePaymentUpFront",
        group: "Billing",
        label: "Require payment before the site opens",
        help: "Off gives the trial first and asks for a card when it ends, which is how \
               Ghost Pro does it. On sends the customer straight to checkout: the site is \
               reserved but stays closed to the public, and opens when the payment lands. \
               Needs a payment provider — a platform that does not charge ignores this.",
        kind: Kind::Bool,
        min_role: StaffRole::Owner,
        fallback: |c| Value::Bool(c.billing.require_payment_up_front),
    },
    SettingDef {
        key: "billing.dunningGraceDays",
        group: "Billing",
        label: "Grace period after a failed payment (days)",
        help: "How long a site stays fully online after a payment fails, before it is \
               suspended. The admin stays reachable even after suspension, so an owner \
               can always pay and come back.",
        kind: Kind::Int { min: 0, max: 180 },
        min_role: StaffRole::Owner,
        fallback: |c| Value::from(c.billing.dunning_grace_days),
    },
    // ---- Stripe ----------------------------------------------------------
    SettingDef {
        key: "billing.stripe.secretKey",
        group: "Stripe",
        label: "Secret key",
        help: "sk_live_… or sk_test_… — used server-side to create checkouts.",
        kind: Kind::Secret,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.stripe.secret_key),
    },
    SettingDef {
        key: "billing.stripe.publishableKey",
        group: "Stripe",
        label: "Publishable key",
        help: "pk_live_… — safe to send to the browser.",
        kind: Kind::Text,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.stripe.publishable_key),
    },
    SettingDef {
        key: "billing.stripe.webhookSecret",
        group: "Stripe",
        label: "Webhook signing secret",
        help: "whsec_… from the endpoint you pointed at /webhooks/stripe. Without it \
               webhooks are rejected rather than trusted — an unverified billing event \
               is how a stranger grants themselves a plan.",
        kind: Kind::Secret,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.stripe.webhook_secret),
    },
    // ---- Paddle ----------------------------------------------------------
    SettingDef {
        key: "billing.paddle.apiKey",
        group: "Paddle",
        label: "API key",
        help: "A Paddle Billing (v2) API key. Classic Paddle keys will not work.",
        kind: Kind::Secret,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.paddle.api_key),
    },
    SettingDef {
        key: "billing.paddle.clientToken",
        group: "Paddle",
        label: "Client-side token",
        help: "Used by Paddle.js to open the overlay checkout in the browser.",
        kind: Kind::Text,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.paddle.client_token),
    },
    SettingDef {
        key: "billing.paddle.webhookSecret",
        group: "Paddle",
        label: "Webhook secret",
        help: "The secret from your Paddle notification destination, used to verify the \
               Paddle-Signature header.",
        kind: Kind::Secret,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.paddle.webhook_secret),
    },
    SettingDef {
        key: "billing.paddle.sandbox",
        group: "Paddle",
        label: "Use Paddle sandbox",
        help: "Talk to sandbox-api.paddle.com instead of the live API.",
        kind: Kind::Bool,
        min_role: StaffRole::Owner,
        fallback: |c| Value::Bool(c.billing.paddle.sandbox),
    },
    // ---- Lemon Squeezy ---------------------------------------------------
    SettingDef {
        key: "billing.lemonsqueezy.apiKey",
        group: "Lemon Squeezy",
        label: "API key",
        help: "Created under Settings → API in your Lemon Squeezy dashboard.",
        kind: Kind::Secret,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.lemonsqueezy.api_key),
    },
    SettingDef {
        key: "billing.lemonsqueezy.storeId",
        group: "Lemon Squeezy",
        label: "Store ID",
        help: "The numeric id of the store checkouts are created in.",
        kind: Kind::Text,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.lemonsqueezy.store_id),
    },
    SettingDef {
        key: "billing.lemonsqueezy.webhookSecret",
        group: "Lemon Squeezy",
        label: "Webhook signing secret",
        help: "The signing secret of the webhook you pointed at /webhooks/lemonsqueezy, \
               used to verify X-Signature. Send the subscription_* events (all twelve); \
               order, license, customer, affiliate and dispute events are not used.",
        kind: Kind::Secret,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.lemonsqueezy.webhook_secret),
    },
    SettingDef {
        key: "billing.lemonsqueezy.testMode",
        group: "Lemon Squeezy",
        label: "Create checkouts in test mode",
        help: "Test-mode checkouts take no real money.",
        kind: Kind::Bool,
        min_role: StaffRole::Owner,
        fallback: |c| Value::Bool(c.billing.lemonsqueezy.test_mode),
    },
    // ---- which product each plan is, at each provider --------------------
    //
    // Edited on /console/plans, not on the settings form: it is a map, and
    // which fields it needs depends on the plan catalogue and on which
    // providers are configured. Registered here for its role check, and
    // because `billing.provider` cannot be switched to a provider that can
    // sell nothing — the check for that reads this.
    SettingDef {
        key: "billing.priceIds",
        group: "Billing",
        label: "Plan product ids",
        help: "Which product or variant each plan is, at each provider. Set these on the \
               Plans screen.",
        kind: Kind::Managed,
        min_role: StaffRole::Owner,
        // The config file's own `plans[].priceIds` are the fallback, so a
        // platform configured entirely in `pro.json` keeps working untouched
        // and the console only ever adds to it.
        fallback: |c| {
            Value::Object(
                c.plans
                    .iter()
                    .filter(|p| !p.price_ids.is_empty())
                    .map(|p| {
                        (
                            p.slug.clone(),
                            Value::Object(
                                p.price_ids
                                    .iter()
                                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                                    .collect(),
                            ),
                        )
                    })
                    .collect(),
            )
        },
    },
    // ---- Gumroad ---------------------------------------------------------
    //
    // Gumroad has no checkout API and signs nothing, so these three carry
    // more weight than the other providers' credentials: the permalink is
    // what a customer is sent to, and the ping token is the *only* thing
    // authenticating a billing event. See `config::GumroadConfig`.
    SettingDef {
        key: "billing.gumroad.accessToken",
        group: "Gumroad",
        label: "Access token",
        help: "An application access token from Settings → Advanced → Applications in \
               Gumroad. Used to read a sale back, which is how an unsigned ping is \
               confirmed to be real.",
        kind: Kind::Secret,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.gumroad.access_token),
    },
    SettingDef {
        key: "billing.gumroad.subdomain",
        group: "Gumroad",
        label: "Seller subdomain",
        help: "The part before .gumroad.com in your own store's address, so a checkout \
               link becomes https://<subdomain>.gumroad.com/l/<permalink>. Leave empty \
               to use gumroad.com/l/<permalink>, which also works.",
        kind: Kind::Text,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.gumroad.subdomain),
    },
    SettingDef {
        key: "billing.gumroad.pingToken",
        group: "Gumroad",
        label: "Ping token",
        help: "A secret you invent, then append to the ping URL you paste into Gumroad: \
               /webhooks/gumroad?token=… . Gumroad does not sign its pings, so this is \
               what tells a real one from a forgery. Pings without it are refused.",
        kind: Kind::Secret,
        min_role: StaffRole::Owner,
        fallback: |c| opt(&c.billing.gumroad.ping_token),
    },
    // ---- Signup ----------------------------------------------------------
    SettingDef {
        key: "signup.enabled",
        group: "Signup",
        label: "Allow new signups",
        help: "Off closes public registration. Existing customers are unaffected, and an \
               operator can still create accounts from this console.",
        kind: Kind::Bool,
        min_role: StaffRole::Admin,
        fallback: |_| Value::Bool(true),
    },
    SettingDef {
        key: "signup.requireEmailVerification",
        group: "Signup",
        label: "Verify email before the site is created",
        help: "On means a new customer must click a link before provisioning starts. \
               This is the main brake on throwaway signups.",
        kind: Kind::Bool,
        min_role: StaffRole::Admin,
        fallback: |_| Value::Bool(true),
    },
    SettingDef {
        key: "signup.defaultPlan",
        group: "Signup",
        label: "Default plan slug",
        help: "Which plan a new signup lands on when none is chosen. Leave blank to use \
               the cheapest purchasable plan.",
        kind: Kind::Text,
        min_role: StaffRole::Admin,
        fallback: |_| Value::String(String::new()),
    },
    // ---- Platform --------------------------------------------------------
    SettingDef {
        key: "platform.name",
        group: "Platform",
        label: "Platform name",
        help: "Shown in the dashboard and in the emails the platform sends.",
        kind: Kind::Text,
        min_role: StaffRole::Admin,
        fallback: |_| Value::String("Vuuut".into()),
    },
    SettingDef {
        key: "platform.supportEmail",
        group: "Platform",
        label: "Support email",
        help: "Where customers are told to write when something is wrong.",
        kind: Kind::Text,
        min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.reply_to),
    },
    SettingDef {
        key: "tenants.idleTimeoutSecs",
        group: "Platform",
        label: "Hibernate idle sites after (seconds)",
        help: "A site with no traffic for this long is stopped, and started again on its \
               next request — usually inside a second. This is what lets one machine hold \
               far more sites than it has memory to run at once. 0 keeps every site \
               running.",
        kind: Kind::Int { min: 0, max: 86_400 },
        min_role: StaffRole::Admin,
        fallback: |c| Value::from(c.tenants.idle_timeout_secs as i64),
    },
    SettingDef {
        key: "tenants.maxRunning",
        group: "Platform",
        label: "Most sites running at once",
        help: "When a request arrives for a hibernating site and this many are already \
               running, the least recently used one is stopped to make room.",
        kind: Kind::Int { min: 1, max: 10_000 },
        min_role: StaffRole::Admin,
        fallback: |c| Value::from(c.tenants.max_running as i64),
    },
    SettingDef {
        key: "mail.tenantProvider", group: "Mail", label: "Tenant email provider",
        help: "Amazon SES or Mailgun applies to all existing and new sites and locks tenant delivery settings. Saving queues configuration updates and restarts running sites.", kind: Kind::Select(&[("tenant", "Tenant configures their own"), ("ses", "Platform Amazon SES"), ("mailgun", "Platform Mailgun")]), min_role: StaffRole::Admin,
        fallback: |c| Value::String(c.mail.tenant_provider.clone()),
    },
    SettingDef {
        key: "mail.ses.region", group: "Amazon SES", label: "AWS region",
        help: "For example us-east-1. Used to derive the standard endpoint when no explicit SMTP endpoint is set.", kind: Kind::Text, min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.ses.region),
    },
    SettingDef {
        key: "mail.ses.endpoint", group: "Amazon SES", label: "SES SMTP endpoint",
        help: "Paste the SMTP Endpoint from your AWS CSV, including Mail Manager endpoints. Leave blank to use the region's standard SES endpoint. STARTTLS on port 587.", kind: Kind::Text, min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.ses.endpoint),
    },
    SettingDef {
        key: "mail.ses.user", group: "Amazon SES", label: "SES SMTP username",
        help: "Use the Username column from your AWS SMTP credentials CSV, not its Base64 column.", kind: Kind::Secret, min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.ses.user),
    },
    SettingDef {
        key: "mail.ses.pass", group: "Amazon SES", label: "SES SMTP password",
        help: "Use the Password column from your AWS SMTP credentials CSV, not its Base64 column or an AWS API secret access key.", kind: Kind::Secret, min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.ses.pass),
    },
    SettingDef {
        key: "mail.mailgun.apiKey", group: "Mailgun", label: "Sending API key",
        help: "", kind: Kind::Secret, min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.mailgun.api_key),
    },
    SettingDef {
        key: "mail.mailgun.domain", group: "Mailgun", label: "Sending domain",
        help: "", kind: Kind::Text, min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.mailgun.domain),
    },
    SettingDef {
        key: "mail.mailgun.baseUrl", group: "Mailgun", label: "Mailgun region",
        help: "", kind: Kind::Select(&[("https://api.mailgun.net/v3", "US"), ("https://api.eu.mailgun.net/v3", "EU")]), min_role: StaffRole::Admin,
        fallback: |c| Value::String(c.mail.mailgun.base_url.clone().unwrap_or_else(|| "https://api.mailgun.net/v3".into())),
    },
    SettingDef {
        key: "mail.mailgun.webhookSigningKey", group: "Mailgun", label: "Webhook signing key",
        help: "", kind: Kind::Secret, min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.mailgun.webhook_signing_key),
    },
    SettingDef {
        key: "mail.ses.maxSendRate", group: "Amazon SES", label: "Newsletter messages per second",
        help: "Per newsletter send; default 1. Allocate your SES account quota across concurrently sending sites and leave capacity for transactional mail.",
        kind: Kind::Int { min: 1, max: 10_000 }, min_role: StaffRole::Admin,
        fallback: |c| Value::from(c.mail.ses.max_send_rate.unwrap_or(1)),
    },
    // ---- Mail ------------------------------------------------------------
    SettingDef {
        key: "mail.transport",
        group: "Mail",
        label: "Transport",
        help: "“Log” writes messages to the platform log instead of sending them, which \
               is what development should use.",
        kind: Kind::Select(&[("smtp", "SMTP"), ("ses", "Amazon SES (SMTP)"), ("log", "Log only")]),
        min_role: StaffRole::Admin,
        fallback: |c| Value::String(c.mail.transport.clone()),
    },
    SettingDef {
        key: "mail.host",
        group: "Mail",
        label: "SMTP host",
        help: "",
        kind: Kind::Text,
        min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.host),
    },
    SettingDef {
        key: "mail.port",
        group: "Mail",
        label: "SMTP port",
        help: "587 for STARTTLS, 465 for implicit TLS.",
        kind: Kind::Int { min: 1, max: 65_535 },
        min_role: StaffRole::Admin,
        fallback: |c| Value::from(c.mail.port.unwrap_or(587) as i64),
    },
    SettingDef {
        key: "mail.user",
        group: "Mail",
        label: "SMTP username",
        help: "",
        kind: Kind::Text,
        min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.user),
    },
    SettingDef {
        key: "mail.pass",
        group: "Mail",
        label: "SMTP password",
        help: "",
        kind: Kind::Secret,
        min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.pass),
    },
    SettingDef {
        key: "mail.fromAddress",
        group: "Mail",
        label: "From address",
        help: "The address platform email is sent from. Must be one your SMTP provider \
               will accept. Leave blank to send as the SMTP username above, which is \
               what most providers expect — set it only if you authenticate as one \
               address and send as another.",
        kind: Kind::Text,
        min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.from_address),
    },
    SettingDef {
        key: "mail.fromName",
        group: "Mail",
        label: "From name",
        help: "",
        kind: Kind::Text,
        min_role: StaffRole::Admin,
        fallback: |c| opt(&c.mail.from_name),
    },
];

fn opt(v: &Option<String>) -> Value {
    match v {
        Some(s) => Value::String(s.clone()),
        None => Value::String(String::new()),
    }
}

pub fn def(key: &str) -> Option<&'static SettingDef> {
    REGISTRY.iter().find(|d| d.key == key)
}

/// The groups in registry order, for rendering the console.
pub fn groups() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for d in REGISTRY {
        if !out.contains(&d.group) {
            out.push(d.group);
        }
    }
    out
}

/// Whether `role` may change `key`.
pub fn can_edit(key: &str, role: StaffRole) -> bool {
    def(key).map(|d| role >= d.min_role).unwrap_or(false)
}

/// Refuse a change `role` is not entitled to make.
///
/// Called on the save path for every incoming key, so a form submission that
/// carries a field the actor is not allowed to set is rejected rather than
/// quietly applied. Rendering the field as read-only is not the control —
/// this is.
pub fn authorize(key: &str, role: StaffRole) -> Result<()> {
    let d = def(key).ok_or_else(|| ProError::BadRequest(format!("unknown setting: {key}")))?;
    if role >= d.min_role {
        Ok(())
    } else {
        Err(ProError::Forbidden(format!(
            "Changing “{}” requires the {} role.",
            d.label,
            d.min_role.label()
        )))
    }
}

/// The settings a given role may edit, for rendering the console.
pub fn editable_by(role: StaffRole) -> Vec<&'static SettingDef> {
    REGISTRY.iter().filter(|d| role >= d.min_role).collect()
}

/// Check and normalise one incoming value against its declaration.
///
/// Returns `None` when the value means "unset me", which is how the console
/// clears an override and falls back to config. An empty string is that signal
/// for text and secret fields — the alternative, storing `""`, would be an
/// override that masks the config value with a blank.
pub fn validate(key: &str, raw: &str) -> Result<Option<Value>> {
    let d = def(key).ok_or_else(|| ProError::BadRequest(format!("unknown setting: {key}")))?;
    let t = raw.trim();
    Ok(match &d.kind {
        // Not editable as a form field, so there is no scalar to validate.
        // Refused rather than ignored: a `billing.priceIds=` arriving on the
        // settings form means something has gone wrong, and silently
        // accepting it would clear every plan's product id.
        Kind::Managed => {
            return Err(ProError::BadRequest(format!(
                "“{}” is not set from this form",
                d.label
            )))
        }
        Kind::Text | Kind::Secret => {
            if t.is_empty() {
                None
            } else {
                Some(Value::String(t.to_string()))
            }
        }
        // An HTML checkbox sends nothing at all when it is off, so the absent
        // case is handled by the caller; here, anything truthy is true.
        Kind::Bool => Some(Value::Bool(matches!(
            t.to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        ))),
        Kind::Int { min, max } => {
            if t.is_empty() {
                return Ok(None);
            }
            let n: i64 = t
                .parse()
                .map_err(|_| ProError::BadRequest(format!("{} must be a whole number", d.label)))?;
            if n < *min || n > *max {
                return Err(ProError::BadRequest(format!(
                    "{} must be between {min} and {max}",
                    d.label
                )));
            }
            Some(Value::from(n))
        }
        Kind::Select(options) => {
            if t.is_empty() {
                return Ok(None);
            }
            if !options.iter().any(|(v, _)| *v == t) {
                return Err(ProError::BadRequest(format!("{} has no option “{t}”", d.label)));
            }
            Some(Value::String(t.to_string()))
        }
    })
}

/// The live configuration: the file layer with the console's overrides applied.
///
/// Everything in the platform reads settings through this rather than off
/// [`Config`] directly, which is what makes a change in the console take
/// effect without a restart. Cheap to clone; rebuilt when a setting is saved.
#[derive(Debug, Clone)]
pub struct Effective {
    config: std::sync::Arc<Config>,
    overrides: BTreeMap<String, Value>,
}

impl Effective {
    pub fn new(config: std::sync::Arc<Config>, overrides: BTreeMap<String, Value>) -> Self {
        Self { config, overrides }
    }

    /// The underlying file config, for the parts that are not operator-editable
    /// (paths, port ranges, the plan catalogue).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Whether this key is set in the console, as opposed to coming from the
    /// file. The admin screens show this so an overridden value is visibly
    /// overridden.
    pub fn is_overridden(&self, key: &str) -> bool {
        self.overrides.contains_key(key)
    }

    /// The console's overrides, without the config layer underneath.
    ///
    /// Exposed so a caller can project what a pending form submission *would*
    /// produce, and check it before writing — the platform refuses a provider
    /// switch whose credentials are missing, and that question can only be
    /// asked of the merged result.
    pub fn overrides(&self) -> &BTreeMap<String, Value> {
        &self.overrides
    }

    fn get(&self, key: &str) -> Value {
        if let Some(v) = self.overrides.get(key) {
            return v.clone();
        }
        match def(key) {
            Some(d) => (d.fallback)(&self.config),
            None => Value::Null,
        }
    }

    fn string(&self, key: &str) -> String {
        match self.get(key) {
            Value::String(s) => s,
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }

    /// A text setting, with blank treated as absent — which is what every
    /// credential check wants.
    fn opt_string(&self, key: &str) -> Option<String> {
        let s = self.string(key);
        if s.trim().is_empty() {
            None
        } else {
            Some(s)
        }
    }

    fn bool(&self, key: &str) -> bool {
        match self.get(key) {
            Value::Bool(b) => b,
            Value::String(s) => matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"),
            _ => false,
        }
    }

    fn int(&self, key: &str) -> i64 {
        match self.get(key) {
            Value::Number(n) => n.as_i64().unwrap_or(0),
            Value::String(s) => s.trim().parse().unwrap_or(0),
            _ => 0,
        }
    }

    /// The value to show in an admin form. Secrets never come back out, so
    /// this returns the empty string for them — the console reports whether
    /// one is set through [`Self::secret_is_set`] instead.
    pub fn form_value(&self, key: &str) -> String {
        match def(key).map(|d| &d.kind) {
            Some(Kind::Secret) => String::new(),
            _ => self.string(key),
        }
    }

    pub fn secret_is_set(&self, key: &str) -> bool {
        self.opt_string(key).is_some()
    }

    // ---- the plan catalogue ----------------------------------------------

    /// The plan catalogue, with any price ids the console has set applied on
    /// top of the config file's.
    ///
    /// Everything that reads a plan goes through here rather than through
    /// `config().plans`, because a price id is the one piece of a plan that an
    /// operator has to be able to change without a restart: it is the id of a
    /// product at a payment provider, and until it is set that plan cannot be
    /// sold at all. The rest of a plan — its limits, its price, its name — is
    /// a deployment decision and stays in `pro.json`.
    ///
    /// A merge, not a replacement: an override names one provider for one
    /// plan, and every other id the config file declares is left alone. So an
    /// operator adding a Lemon Squeezy variant does not silently drop the
    /// Stripe price that existing customers are still billed on.
    pub fn plans(&self) -> Vec<crate::plans::Plan> {
        let overrides = self.overrides.get("billing.priceIds").and_then(Value::as_object);
        self.config
            .plans
            .iter()
            .map(|plan| {
                let mut normalized = plan.clone();
                normalized.max_emails_per_period = Some(self.overrides.get("billing.emailLimits")
                    .and_then(|v| v.get(&plan.slug)).and_then(Value::as_i64).unwrap_or_else(|| plan.email_allowance()));
                let plan = &normalized;
                let Some(for_plan) = overrides
                    .and_then(|o| o.get(&plan.slug))
                    .and_then(Value::as_object)
                else {
                    return plan.clone();
                };
                let mut plan = plan.clone();
                for (provider, id) in for_plan {
                    match id.as_str().map(str::trim) {
                        // A blank clears it, so a mistyped id can be removed
                        // rather than only replaced.
                        Some("") | None => {
                            plan.price_ids.remove(provider);
                        }
                        Some(id) => {
                            plan.price_ids.insert(provider.clone(), id.to_string());
                        }
                    }
                }
                plan
            })
            .collect()
    }

    /// One plan by slug, from the overridden catalogue.
    pub fn plan(&self, slug: &str) -> Option<crate::plans::Plan> {
        self.plans().into_iter().find(|p| p.slug == slug)
    }

    /// Plans a new customer may buy, cheapest first — the overridden
    /// catalogue's answer to `Config::purchasable_plans`.
    pub fn purchasable_plans(&self) -> Vec<crate::plans::Plan> {
        let provider = self.billing_provider();
        let mut plans: Vec<crate::plans::Plan> = if provider == "none" {
            // Nothing is being charged for, so every visible plan is on offer
            // — a price id would be meaningless.
            self.plans().into_iter().filter(|p| !p.hidden).collect()
        } else {
            self.plans().into_iter().filter(|p| p.is_purchasable(&provider)).collect()
        };
        plans.sort_by_key(|p| (p.sort_order, p.price_cents));
        plans
    }

    /// The price ids the console has set, as a map, for the screen that edits
    /// them. Plan slug → provider → id.
    pub fn price_id_overrides(&self) -> std::collections::BTreeMap<String, BTreeMap<String, String>> {
        self.plans()
            .into_iter()
            .map(|p| (p.slug, p.price_ids.into_iter().collect()))
            .collect()
    }

    // ---- billing ---------------------------------------------------------

    /// Which provider new checkouts go to.
    pub fn billing_provider(&self) -> String {
        let p = self.string("billing.provider");
        if p.is_empty() {
            "none".into()
        } else {
            p
        }
    }

    pub fn require_payment_up_front(&self) -> bool {
        self.bool("billing.requirePaymentUpFront")
    }

    pub fn dunning_grace_days(&self) -> i64 {
        self.int("billing.dunningGraceDays")
    }

    pub fn stripe(&self) -> StripeConfig {
        StripeConfig {
            secret_key: self.opt_string("billing.stripe.secretKey"),
            publishable_key: self.opt_string("billing.stripe.publishableKey"),
            webhook_secret: self.opt_string("billing.stripe.webhookSecret"),
        }
    }

    pub fn paddle(&self) -> PaddleConfig {
        PaddleConfig {
            api_key: self.opt_string("billing.paddle.apiKey"),
            client_token: self.opt_string("billing.paddle.clientToken"),
            webhook_secret: self.opt_string("billing.paddle.webhookSecret"),
            sandbox: self.bool("billing.paddle.sandbox"),
        }
    }

    pub fn lemonsqueezy(&self) -> LemonSqueezyConfig {
        LemonSqueezyConfig {
            api_key: self.opt_string("billing.lemonsqueezy.apiKey"),
            store_id: self.opt_string("billing.lemonsqueezy.storeId"),
            webhook_secret: self.opt_string("billing.lemonsqueezy.webhookSecret"),
            test_mode: self.bool("billing.lemonsqueezy.testMode"),
        }
    }

    pub fn gumroad(&self) -> GumroadConfig {
        GumroadConfig {
            access_token: self.opt_string("billing.gumroad.accessToken"),
            subdomain: self.opt_string("billing.gumroad.subdomain"),
            ping_token: self.opt_string("billing.gumroad.pingToken"),
        }
    }

    /// Whether a provider has enough configured to be *selectable*.
    ///
    /// The console uses this to refuse a switch to a provider whose keys are
    /// missing, which is the mistake that would otherwise break every new
    /// checkout the moment it was saved.
    pub fn provider_is_configured(&self, provider: &str) -> bool {
        match provider {
            "none" => true,
            "stripe" => self.stripe().secret_key.is_some(),
            "paddle" => self.paddle().api_key.is_some(),
            "lemonsqueezy" => {
                let ls = self.lemonsqueezy();
                ls.api_key.is_some() && ls.store_id.is_some()
            }
            // The ping token counts as a credential here, not a nicety:
            // without it every ping is refused, so a Gumroad with a token
            // missing cannot take a single renewal.
            "gumroad" => {
                let g = self.gumroad();
                g.access_token.is_some() && g.ping_token.is_some()
            }
            _ => false,
        }
    }

    /// The secret a provider's webhooks are verified against.
    ///
    /// Without it every delivery is rejected rather than trusted, which fails
    /// exactly like an unreachable platform: the payment succeeds and nothing
    /// on this side ever hears about it. Gumroad's ping token is the same
    /// thing under another name, so it answers here too.
    pub fn webhook_secret(&self, provider: &str) -> Option<String> {
        match provider {
            "stripe" => self.stripe().webhook_secret,
            "paddle" => self.paddle().webhook_secret,
            "lemonsqueezy" => self.lemonsqueezy().webhook_secret,
            "gumroad" => self.gumroad().ping_token,
            _ => None,
        }
    }

    /// Providers with credentials present, in registry order. Webhooks are
    /// accepted for all of these, not only the active one — a subscription
    /// created at a provider keeps sending events there after new signups move
    /// away, and dropping those would mean missing renewals and cancellations.
    pub fn configured_providers(&self) -> Vec<String> {
        ["stripe", "paddle", "lemonsqueezy", "gumroad"]
            .into_iter()
            .filter(|p| self.provider_is_configured(p))
            .map(str::to_string)
            .collect()
    }

    // ---- signup ----------------------------------------------------------

    pub fn signup_enabled(&self) -> bool {
        self.bool("signup.enabled")
    }

    pub fn require_email_verification(&self) -> bool {
        self.bool("signup.requireEmailVerification")
    }

    /// The plan a signup with no choice lands on: the configured default if it
    /// still exists and is purchasable, else the cheapest that is.
    pub fn default_plan(&self) -> Option<crate::plans::Plan> {
        let provider = self.billing_provider();
        let named = self.opt_string("signup.defaultPlan");
        if let Some(slug) = named {
            if let Some(p) = self.config.plan(&slug) {
                return Some(p.clone());
            }
            // A default pointing at a plan that has been removed should not
            // break signup; fall through to the cheapest.
            tracing::warn!("signup.defaultPlan names “{slug}”, which is not in the catalogue");
        }
        let mut plans: Vec<&crate::plans::Plan> = if provider == "none" {
            self.config.plans.iter().filter(|p| !p.hidden).collect()
        } else {
            self.config.plans.iter().filter(|p| p.is_purchasable(&provider)).collect()
        };
        plans.sort_by_key(|p| (p.sort_order, p.price_cents));
        plans.first().map(|p| (*p).clone())
    }

    // ---- platform --------------------------------------------------------

    pub fn platform_name(&self) -> String {
        let n = self.string("platform.name");
        if n.trim().is_empty() {
            "Vuuut".into()
        } else {
            n
        }
    }

    pub fn support_email(&self) -> Option<String> {
        self.opt_string("platform.supportEmail")
    }

    pub fn idle_timeout_secs(&self) -> u64 {
        self.int("tenants.idleTimeoutSecs").max(0) as u64
    }

    pub fn max_running(&self) -> usize {
        self.int("tenants.maxRunning").max(1) as usize
    }

    // ---- mail ------------------------------------------------------------

    pub fn mail(&self) -> MailConfig {
        MailConfig {
            transport: {
                let t = self.string("mail.transport");
                if t.is_empty() {
                    "log".into()
                } else {
                    t
                }
            },
            host: self.opt_string("mail.host"),
            port: {
                let p = self.int("mail.port");
                if (1..=65_535).contains(&p) {
                    Some(p as u16)
                } else {
                    None
                }
            },
            user: self.opt_string("mail.user"),
            pass: self.opt_string("mail.pass"),
            from_address: self.opt_string("mail.fromAddress"),
            from_name: self.opt_string("mail.fromName"),
            reply_to: self.opt_string("platform.supportEmail"),
            tenant_provider: self.string("mail.tenantProvider"),
            auto_stop_at_limit: self.bool("mail.autoStopAtLimit"),
            ses: crate::config::SesConfig {
                endpoint: self.opt_string("mail.ses.endpoint"),
                max_send_rate: Some(self.int("mail.ses.maxSendRate").clamp(1, 10_000) as u32),
                region: self.opt_string("mail.ses.region"),
                user: self.opt_string("mail.ses.user"),
                pass: self.opt_string("mail.ses.pass"),
            },
            mailgun: crate::config::MailgunConfig {
                api_key: self.opt_string("mail.mailgun.apiKey"),
                domain: self.opt_string("mail.mailgun.domain"),
                base_url: self.opt_string("mail.mailgun.baseUrl"),
                webhook_signing_key: self.opt_string("mail.mailgun.webhookSigningKey"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn base() -> Arc<Config> {
        let mut c: Config = serde_json::from_value(serde_json::json!({
            "url": "https://pro.example.com",
            "subdomainBase": "sites.example.com",
            "billing": { "provider": "stripe", "stripe": { "secretKey": "sk_from_file" } },
        }))
        .unwrap();
        c.plans = crate::plans::default_plans();
        // Priced at Stripe, so the fixture has something that can actually be
        // bought — otherwise every default-plan lookup correctly returns None.
        c.plans[0].price_ids.insert("stripe".into(), "price_starter".into());
        Arc::new(c)
    }

    fn eff(overrides: &[(&str, Value)]) -> Effective {
        Effective::new(
            base(),
            overrides.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        )
    }

    // ---- the plan catalogue ----------------------------------------------

    /// A product id set in the console has to reach the live catalogue, since
    /// nothing can be sold until it does.
    #[test]
    fn a_console_product_id_reaches_the_catalogue() {
        let e = eff(&[(
            "billing.priceIds",
            serde_json::json!({ "starter": { "lemonsqueezy": "812345" } }),
        )]);
        assert_eq!(
            e.plan("starter").unwrap().price_id("lemonsqueezy"),
            Some("812345")
        );
        assert!(e.plan("starter").unwrap().is_purchasable("lemonsqueezy"));
        // Untouched plans are untouched.
        assert_eq!(e.plan("creator").unwrap().price_id("lemonsqueezy"), None);
    }

    /// A merge, not a replacement. An operator adding a Lemon Squeezy variant
    /// must not silently drop the Stripe price that existing customers are
    /// still billed on — that would orphan every one of them.
    #[test]
    fn an_override_does_not_drop_the_files_other_ids() {
        let mut cfg = (*base()).clone();
        cfg.plans[0].price_ids.insert("stripe".into(), "price_file".into());
        let e = Effective::new(
            std::sync::Arc::new(cfg),
            [(
                "billing.priceIds".to_string(),
                serde_json::json!({ "starter": { "lemonsqueezy": "812345" } }),
            )]
            .into_iter()
            .collect(),
        );
        let starter = e.plan("starter").unwrap();
        assert_eq!(starter.price_id("stripe"), Some("price_file"), "the file's id survives");
        assert_eq!(starter.price_id("lemonsqueezy"), Some("812345"), "and the new one is there");
    }

    /// The console can override an id the file already set, which is how a
    /// mistyped one is corrected without a deploy.
    #[test]
    fn an_override_wins_over_the_file() {
        let mut cfg = (*base()).clone();
        cfg.plans[0].price_ids.insert("stripe".into(), "price_wrong".into());
        let e = Effective::new(
            std::sync::Arc::new(cfg),
            [(
                "billing.priceIds".to_string(),
                serde_json::json!({ "starter": { "stripe": "price_right" } }),
            )]
            .into_iter()
            .collect(),
        );
        assert_eq!(e.plan("starter").unwrap().price_id("stripe"), Some("price_right"));
    }

    /// A blank clears an id rather than storing an empty string, so a wrong
    /// one can be removed and not only replaced. An empty price id would make
    /// a plan look purchasable and then fail at the provider.
    #[test]
    fn a_blank_override_clears_an_id() {
        let mut cfg = (*base()).clone();
        cfg.plans[0].price_ids.insert("stripe".into(), "price_file".into());
        let e = Effective::new(
            std::sync::Arc::new(cfg),
            [(
                "billing.priceIds".to_string(),
                serde_json::json!({ "starter": { "stripe": "   " } }),
            )]
            .into_iter()
            .collect(),
        );
        assert_eq!(e.plan("starter").unwrap().price_id("stripe"), None);
        assert!(!e.plan("starter").unwrap().is_purchasable("stripe"));
    }

    /// `purchasable_plans` is what the pricing page and the plan chooser are
    /// built from, so it has to agree with the overridden catalogue.
    #[test]
    fn only_plans_with_an_id_at_the_active_provider_are_on_sale() {
        let e = eff(&[
            ("billing.provider", Value::String("lemonsqueezy".into())),
            ("billing.lemonsqueezy.apiKey", Value::String("k".into())),
            ("billing.lemonsqueezy.storeId", Value::String("4242".into())),
            (
                "billing.priceIds",
                serde_json::json!({ "starter": { "lemonsqueezy": "812345" } }),
            ),
        ]);
        let on_sale: Vec<String> = e.purchasable_plans().into_iter().map(|p| p.slug).collect();
        assert_eq!(on_sale, vec!["starter".to_string()]);
    }

    /// With no provider, a price id would be meaningless — every visible plan
    /// is on offer.
    #[test]
    fn with_no_provider_every_visible_plan_is_offered() {
        let e = eff(&[("billing.provider", Value::String("none".into()))]);
        assert!(
            e.purchasable_plans().len() >= 2,
            "nothing is being charged, so nothing needs an id"
        );
    }

    /// A junk override must not panic or wipe the catalogue — the value is
    /// stored as JSON and nothing stops a future shape from differing.
    #[test]
    fn a_malformed_override_is_ignored() {
        for junk in [
            Value::String("not a map".into()),
            serde_json::json!([1, 2, 3]),
            serde_json::json!({ "starter": "not a map either" }),
            serde_json::json!({ "no-such-plan": { "stripe": "x" } }),
        ] {
            let e = eff(&[("billing.priceIds", junk.clone())]);
            assert_eq!(e.plans().len(), base().plans.len(), "catalogue intact for {junk}");
            assert!(e.plan("starter").is_some());
        }
    }

    /// With nothing overridden, the file is what is in force.
    #[test]
    fn falls_back_to_config() {
        let e = eff(&[]);
        assert_eq!(e.billing_provider(), "stripe");
        assert_eq!(e.stripe().secret_key.as_deref(), Some("sk_from_file"));
        assert!(!e.is_overridden("billing.provider"));
    }

    /// The whole point: the console beats the file, with no restart.
    #[test]
    fn console_overrides_the_file() {
        let e = eff(&[
            ("billing.provider", Value::String("lemonsqueezy".into())),
            ("billing.lemonsqueezy.apiKey", Value::String("ls_key".into())),
            ("billing.lemonsqueezy.storeId", Value::String("4242".into())),
        ]);
        assert_eq!(e.billing_provider(), "lemonsqueezy");
        assert!(e.is_overridden("billing.provider"));
        assert!(e.provider_is_configured("lemonsqueezy"));
    }

    /// Both providers keep working for webhooks while only one takes new
    /// checkouts, so a switch does not drop renewals from the old one.
    #[test]
    fn every_credentialled_provider_stays_active_for_webhooks() {
        let e = eff(&[
            ("billing.provider", Value::String("paddle".into())),
            ("billing.paddle.apiKey", Value::String("pdl".into())),
        ]);
        assert_eq!(e.billing_provider(), "paddle");
        assert_eq!(e.configured_providers(), vec!["stripe", "paddle"]);
    }

    /// Switching to a provider with no keys is the mistake that breaks every
    /// new checkout, so the console must be able to catch it.
    #[test]
    fn detects_an_unconfigured_provider() {
        let e = eff(&[]);
        assert!(e.provider_is_configured("stripe"));
        assert!(!e.provider_is_configured("paddle"));
        // Lemon Squeezy needs both a key and a store.
        let partial = eff(&[("billing.lemonsqueezy.apiKey", Value::String("k".into()))]);
        assert!(!partial.provider_is_configured("lemonsqueezy"));
    }

    /// Secrets are stored but never served back to a browser.
    #[test]
    fn secrets_are_write_only() {
        let e = eff(&[("billing.stripe.secretKey", Value::String("sk_live_secret".into()))]);
        assert_eq!(e.stripe().secret_key.as_deref(), Some("sk_live_secret"));
        assert_eq!(e.form_value("billing.stripe.secretKey"), "", "must not echo a secret");
        assert!(e.secret_is_set("billing.stripe.secretKey"));
        // A non-secret is shown as normal.
        assert_eq!(e.form_value("billing.provider"), "stripe");
    }

    /// Clearing a field removes the override rather than storing a blank that
    /// would mask the config value.
    #[test]
    fn blank_clears_an_override() {
        assert_eq!(validate("mail.host", "").unwrap(), None);
        assert_eq!(validate("billing.dunningGraceDays", "").unwrap(), None);
        assert_eq!(
            validate("mail.host", " smtp.example.com ").unwrap(),
            Some(Value::String("smtp.example.com".into()))
        );
    }

    #[test]
    fn validates_against_the_declaration() {
        assert!(validate("billing.provider", "stripe").is_ok());
        assert!(validate("billing.provider", "braintree").is_err());
        assert!(validate("billing.dunningGraceDays", "14").is_ok());
        assert!(validate("billing.dunningGraceDays", "400").is_err(), "above max");
        assert!(validate("billing.dunningGraceDays", "nope").is_err());
        assert!(validate("nonexistent.key", "x").is_err());
        assert_eq!(validate("signup.enabled", "on").unwrap(), Some(Value::Bool(true)));
        assert_eq!(validate("signup.enabled", "0").unwrap(), Some(Value::Bool(false)));
    }

    /// A numeric-looking id must stay a string, or it stops matching what the
    /// provider's API returns.
    #[test]
    fn store_ids_stay_strings() {
        assert_eq!(
            validate("billing.lemonsqueezy.storeId", "4242").unwrap(),
            Some(Value::String("4242".into()))
        );
    }

    /// A default plan that has been deleted from the catalogue must not break
    /// signup.
    #[test]
    fn default_plan_survives_a_bad_slug() {
        let e = eff(&[("signup.defaultPlan", Value::String("gone".into()))]);
        let plan = e.default_plan().expect("falls back to the cheapest");
        // Nothing has a stripe price id in this fixture, so nothing is
        // purchasable and the fallback is empty — set one and retry.
        assert_eq!(plan.slug, "starter", "cheapest by sort order");
    }

    /// Only the platform owner may change how money is taken. An
    /// administrator runs tenants; it must not be able to point new checkouts
    /// at a different provider or replace the keys behind one.
    #[test]
    fn only_the_owner_may_change_payment_settings() {
        for key in [
            "billing.provider",
            "billing.stripe.secretKey",
            "billing.stripe.webhookSecret",
            "billing.paddle.apiKey",
            "billing.lemonsqueezy.apiKey",
            "billing.lemonsqueezy.storeId",
            "billing.requirePaymentUpFront",
        ] {
            assert!(can_edit(key, StaffRole::Owner), "owner must be able to set {key}");
            assert!(!can_edit(key, StaffRole::Admin), "admin must not be able to set {key}");
            assert!(!can_edit(key, StaffRole::Support), "support must not be able to set {key}");
            assert!(authorize(key, StaffRole::Admin).is_err());
            assert!(authorize(key, StaffRole::Owner).is_ok());
        }
    }

    /// Support is read-only everywhere — it exists to help a customer, not to
    /// reconfigure the platform.
    #[test]
    fn support_edits_nothing() {
        assert!(editable_by(StaffRole::Support).is_empty());
        let admin = editable_by(StaffRole::Admin);
        assert!(!admin.is_empty());
        assert!(admin.iter().all(|d| d.min_role == StaffRole::Admin));
        assert_eq!(editable_by(StaffRole::Owner).len(), REGISTRY.len(), "owner edits everything");
    }

    /// Operational settings are an administrator's job, so running the
    /// platform day to day does not need the owner login.
    #[test]
    fn admins_run_the_platform() {
        for key in ["signup.enabled", "tenants.idleTimeoutSecs", "mail.host", "platform.name"] {
            assert!(can_edit(key, StaffRole::Admin), "admin should set {key}");
        }
    }

    /// An admin cannot promote their way to the money settings.
    #[test]
    fn admins_cannot_create_owners() {
        assert!(!StaffRole::Admin.assignable().contains(&StaffRole::Owner));
        assert!(StaffRole::Owner.assignable().contains(&StaffRole::Owner));
    }

    /// Every key the console renders must be reachable, and every registry
    /// entry must have a group — the console renders one card per group, so a
    /// groupless entry would simply never be drawn.
    #[test]
    fn registry_is_well_formed() {
        let mut seen = std::collections::BTreeSet::new();
        for d in REGISTRY {
            assert!(seen.insert(d.key), "duplicate setting key {}", d.key);
            assert!(!d.group.is_empty(), "{} has no group", d.key);
            assert!(!d.label.is_empty(), "{} has no label", d.key);
            if let Kind::Select(opts) = &d.kind {
                assert!(!opts.is_empty(), "{} is a select with no options", d.key);
            }
        }
        assert!(groups().contains(&"Billing"));
    }

    /// A provider with credentials but no signing secret refuses every
    /// delivery, which loses a payment as completely as having no credentials
    /// at all — so the secret has to be readable to be checked for.
    #[test]
    fn a_webhook_secret_is_readable_per_provider() {
        let e = eff(&[
            ("billing.lemonsqueezy.webhookSecret", Value::String("ls_whsec".into())),
            ("billing.gumroad.pingToken", Value::String("tok".into())),
        ]);
        assert_eq!(e.webhook_secret("lemonsqueezy").as_deref(), Some("ls_whsec"));
        // Gumroad signs nothing, so its ping token is the same thing by
        // another name and must answer here too.
        assert_eq!(e.webhook_secret("gumroad").as_deref(), Some("tok"));
        // Credentials present, secret absent: the state this exists to catch.
        assert!(e.provider_is_configured("stripe"), "the file gives it a key");
        assert_eq!(e.webhook_secret("stripe"), None);
        assert_eq!(e.webhook_secret("braintree"), None);
    }

    /// Mail's reply-to is the support address, so an operator sets it once.
    #[test]
    fn mail_reply_to_follows_support_email() {
        let e = eff(&[("platform.supportEmail", Value::String("help@example.com".into()))]);
        assert_eq!(e.mail().reply_to.as_deref(), Some("help@example.com"));
        assert_eq!(e.support_email().as_deref(), Some("help@example.com"));
    }
}
