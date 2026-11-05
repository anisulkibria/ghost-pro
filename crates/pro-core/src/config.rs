//! Platform configuration: `pro.json`, overridable by `PRO_*` environment
//! variables.
//!
//! Layered the same way Ghost's own config is, and for the same reason — an
//! operator edits a file, a container sets environment variables, and neither
//! has to know about the other:
//!
//! 1. `PRO_*` environment variables (`__` nests: `PRO_BILLING__PROVIDER`)
//! 2. `pro.json` in the working directory
//! 3. the defaults below
//!
//! Unlike Ghost's loader, a `PRO_*` value that parses as JSON is *used* as
//! JSON. That matters: the tenant-facing half of this platform has to write
//! whole objects (a plan's limits, a provider's keys) and Ghost's loader turns
//! `{"a":1}` into the string `"{\"a\":1}"`, which silently becomes a config
//! value nothing can read. Writing an object from the environment is rare but
//! being unable to is worse.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::{ProError, Result};
use crate::plans::{default_plans, Plan};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// Where the platform itself lives — the dashboard, the pricing page, the
    /// webhook endpoints. Must carry a scheme.
    pub url: String,
    /// The zone tenant subdomains are issued under. A tenant with slug `acme`
    /// is served at `acme.<subdomain_base>`.
    ///
    /// Kept separate from `url` because they are usually different hosts:
    /// the dashboard at `pro.example.com` and sites at `*.example.com`, or a
    /// dedicated `*.ghost.example.com` so that no tenant slug can ever
    /// collide with a platform route.
    pub subdomain_base: String,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub tenants: TenantsConfig,
    #[serde(default)]
    pub ssl: SslConfig,
    #[serde(default)]
    pub billing: BillingConfig,
    #[serde(default)]
    pub mail: MailConfig,
    /// The plan catalogue. Replaces the built-in list wholesale when present,
    /// so an operator's prices are never merged with ours field by field.
    #[serde(default)]
    pub plans: Vec<Plan>,
    /// Slugs nobody may register, on top of the built-in list. An operator
    /// adds the names their own marketing site uses here.
    #[serde(default)]
    pub reserved_slugs: Vec<String>,
    /// The public IP addresses this platform answers on.
    ///
    /// These are what a customer is told to point an `A`/`AAAA` record at, and
    /// what a DNS check compares against. An operator sets them because the
    /// machine cannot reliably know its own public address — behind NAT, a
    /// load balancer or an anycast front end, the address a visitor resolves
    /// is not one any local interface has.
    ///
    /// Left empty, the platform falls back to resolving its own hostname,
    /// which is correct whenever the dashboard and the tenant sites are served
    /// from the same place — the common single-machine case.
    #[serde(default)]
    pub public_addresses: Vec<String>,
    #[serde(skip)]
    pub env: String,
    #[serde(skip)]
    pub base_dir: PathBuf,
}

/// The control plane's own listeners.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    /// The port the *edge* binds — the one real traffic arrives on, for both
    /// the dashboard and every tenant site. 443 with TLS on, 8080 without.
    #[serde(default)]
    pub port: Option<u16>,
    /// Plain-HTTP port, which redirects to HTTPS and answers ACME's HTTP-01
    /// challenge. `0` disables it.
    #[serde(default = "default_http_port")]
    pub http_port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { host: default_host(), port: None, http_port: default_http_port() }
    }
}

fn default_host() -> String {
    // The edge is the public front door, so it binds every interface — unlike
    // a tenant process, which binds loopback only and is reachable through
    // this proxy and nothing else.
    "0.0.0.0".into()
}

fn default_http_port() -> u16 {
    80
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathsConfig {
    /// Root of everything mutable: the control-plane database, tenant
    /// directories, certificates, backups, logs.
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// The `ghost` binary tenant processes are spawned from. Looked up on
    /// `PATH` when it is a bare name.
    #[serde(default = "default_ghost_binary")]
    pub ghost_binary: String,
    /// Themes copied into every new tenant. Points at a Ghost-rust
    /// `content/themes` directory.
    #[serde(default)]
    pub theme_source: Option<String>,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            ghost_binary: default_ghost_binary(),
            theme_source: None,
        }
    }
}

fn default_data_dir() -> String {
    "data".into()
}

fn default_ghost_binary() -> String {
    "ghost".into()
}

/// Which database engine the platform and its tenants run on.
///
/// Every field is optional, and a config with no `database` block at all is
/// the local default: one SQLite file for the control plane, one per tenant
/// inside its own directory. Nothing has to be installed or created for that
/// to work, which is the point — a fresh clone runs.
///
/// Production sets `url`, and usually `tenantUrl` as well. The two are
/// separate because they are genuinely separate decisions: a small deployment
/// can keep the control plane on MySQL for failover while tenants stay on
/// their own files, and a large one wants both on a managed server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseConfig {
    /// The control plane's own database.
    ///
    /// `mysql://user:pass@host:3306/pro` in production; unset locally, which
    /// means SQLite at `<dataDir>/pro.db`.
    #[serde(default)]
    pub url: Option<String>,
    /// Pool ceiling. Defaults per engine — see [`DatabaseConfig::pool_size`].
    #[serde(default)]
    pub max_connections: Option<u32>,
    /// The MySQL server tenant databases are created on, as
    /// `mysql://admin:pass@host:3306` with no database named.
    ///
    /// The account in this URL needs `CREATE DATABASE`, `CREATE USER` and
    /// `GRANT` — it provisions a database and a scoped login per tenant, and
    /// is used for nothing else. Unset means every tenant keeps its own
    /// SQLite file, as it does locally.
    #[serde(default)]
    pub tenant_url: Option<String>,
    /// Prepended to each tenant's database and user name, so platform
    /// databases are recognisable on a server that holds other things.
    #[serde(default = "default_tenant_db_prefix")]
    pub tenant_prefix: String,
}

fn default_tenant_db_prefix() -> String {
    "ghost_".into()
}

impl DatabaseConfig {
    /// Connections to open, defaulted per engine.
    ///
    /// The two engines want opposite things. SQLite serialises writes, so a
    /// large pool only converts waiting into lock contention; MySQL handles
    /// real concurrency, and a pool of 8 would become the bottleneck once
    /// several app nodes serve the dashboard.
    pub fn pool_size(&self, engine: crate::database::Engine) -> u32 {
        self.max_connections.unwrap_or(match engine {
            crate::database::Engine::Sqlite => 8,
            crate::database::Engine::MySql => 32,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantsConfig {
    /// Loopback ports handed to tenant processes. Each tenant keeps its port
    /// for life, so a restart does not move it.
    #[serde(default = "default_port_floor")]
    pub port_floor: u16,
    #[serde(default = "default_port_ceiling")]
    pub port_ceiling: u16,
    /// Stop a tenant that has had no request for this long, and start it again
    /// on the next one.
    ///
    /// This is what makes density possible: an idle Rust Ghost still holds its
    /// pools and caches, and most sites on a hosting platform are idle most of
    /// the time. `0` keeps every tenant running.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// How long to wait for a freshly spawned tenant to answer before calling
    /// the start a failure.
    #[serde(default = "default_start_timeout")]
    pub start_timeout_secs: u64,
    /// Most tenants that may be running at once. A request for a hibernating
    /// tenant beyond this evicts the least recently used one first.
    #[serde(default = "default_max_running")]
    pub max_running: usize,
    /// `GHOST_ENV` for tenant processes. Production turns on the behaviour
    /// that matters for a real site, including secure cookies.
    #[serde(default = "default_tenant_env")]
    pub env: String,
}

impl Default for TenantsConfig {
    fn default() -> Self {
        Self {
            port_floor: default_port_floor(),
            port_ceiling: default_port_ceiling(),
            idle_timeout_secs: default_idle_timeout(),
            start_timeout_secs: default_start_timeout(),
            max_running: default_max_running(),
            env: default_tenant_env(),
        }
    }
}

fn default_port_floor() -> u16 {
    // Clear of Ghost's own 2368-and-upwards range and of the ephemeral range
    // most kernels allocate from, so a tenant port is neither taken by a
    // comparison install nor stolen by an outbound connection.
    21_000
}
fn default_port_ceiling() -> u16 {
    28_999
}
fn default_idle_timeout() -> u64 {
    900
}
fn default_start_timeout() -> u64 {
    30
}
fn default_max_running() -> usize {
    200
}
fn default_tenant_env() -> String {
    "production".into()
}

/// TLS at the edge.
///
/// One certificate per hostname, issued on demand: a tenant subdomain is
/// covered the moment it is created, and a custom domain the moment its DNS
/// points here. A wildcard for `*.subdomain_base` would cover every tenant at
/// once, but needs DNS-01 and therefore credentials for the operator's DNS
/// provider, so it is offered rather than assumed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SslConfig {
    /// Off means plain HTTP at the edge — right for local development, and
    /// for sitting behind something that has already terminated TLS.
    #[serde(default)]
    pub enabled: bool,
    /// Registered with the CA; where expiry warnings go if renewal stops
    /// working.
    #[serde(default)]
    pub email: Option<String>,
    /// Let's Encrypt's staging CA: untrusted certificates, far looser rate
    /// limits. The right place to prove a setup works before spending real
    /// quota, and the difference between a mistake that costs an afternoon and
    /// one that costs a week of rate-limit lockout.
    #[serde(default)]
    pub staging: bool,
    /// Trust `X-Forwarded-Proto` and `X-Forwarded-For` from the proxy in
    /// front. Only set this when something really is in front: it lets a
    /// client claim its own scheme and address otherwise.
    #[serde(default)]
    pub behind_proxy: bool,
}

impl Default for SslConfig {
    fn default() -> Self {
        Self { enabled: false, email: None, staging: false, behind_proxy: false }
    }
}

/// Which payment provider the platform sells through, and the keys for it.
///
/// All four are configured at once if an operator wants; `provider` picks the
/// one new checkouts go to. The others stay live for webhooks, because
/// subscriptions created at a provider keep sending events there long after
/// new signups have moved elsewhere — so a migration does not mean a gap in
/// renewal handling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingConfig {
    /// `stripe`, `paddle`, `lemonsqueezy`, `gumroad`, or `none`.
    ///
    /// `none` is a platform that does not charge — an internal deployment, or
    /// one where an operator provisions every tenant by hand. Signup still
    /// works; it just never asks for a card.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Require payment before a site opens to the public. On, a new site is
    /// reserved and suspended, the customer goes straight to checkout, and the
    /// payment webhook opens it. Off means the trial runs first and the card is
    /// asked for when it ends, which is how Ghost Pro does it.
    ///
    /// Ignored when `provider` is `none`: a platform that does not charge
    /// cannot take the payment this would wait for.
    #[serde(default)]
    pub require_payment_up_front: bool,
    /// Days of grace after a payment fails before the site is suspended.
    #[serde(default = "default_grace_days")]
    pub dunning_grace_days: i64,
    #[serde(default)]
    pub stripe: StripeConfig,
    #[serde(default)]
    pub paddle: PaddleConfig,
    #[serde(default)]
    pub lemonsqueezy: LemonSqueezyConfig,
    #[serde(default)]
    pub gumroad: GumroadConfig,
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            require_payment_up_front: false,
            dunning_grace_days: default_grace_days(),
            stripe: StripeConfig::default(),
            paddle: PaddleConfig::default(),
            lemonsqueezy: LemonSqueezyConfig::default(),
            gumroad: GumroadConfig::default(),
        }
    }
}

fn default_provider() -> String {
    "none".into()
}
fn default_grace_days() -> i64 {
    14
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StripeConfig {
    /// `sk_live_…` / `sk_test_…`.
    #[serde(default, alias = "secretKey")]
    pub secret_key: Option<String>,
    /// `pk_live_…`, for the browser.
    #[serde(default, alias = "publishableKey")]
    pub publishable_key: Option<String>,
    /// `whsec_…`. Webhooks are rejected without it rather than trusted — an
    /// unverified billing event is how a stranger grants themselves a plan.
    #[serde(default, alias = "webhookSecret")]
    pub webhook_secret: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaddleConfig {
    /// Paddle Billing API key.
    #[serde(default, alias = "apiKey")]
    pub api_key: Option<String>,
    /// Client-side token for Paddle.js.
    #[serde(default, alias = "clientToken")]
    pub client_token: Option<String>,
    /// The notification destination's secret, for `Paddle-Signature`.
    #[serde(default, alias = "webhookSecret")]
    pub webhook_secret: Option<String>,
    /// Use `sandbox-api.paddle.com` instead of the live API.
    #[serde(default)]
    pub sandbox: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LemonSqueezyConfig {
    #[serde(default, alias = "apiKey")]
    pub api_key: Option<String>,
    /// The store a checkout is created in.
    #[serde(default, alias = "storeId")]
    pub store_id: Option<String>,
    /// Signing secret for `X-Signature`.
    #[serde(default, alias = "webhookSecret")]
    pub webhook_secret: Option<String>,
    /// Lemon Squeezy's own test mode, set per checkout.
    #[serde(default, alias = "testMode")]
    pub test_mode: bool,
}

/// Gumroad.
///
/// The odd one out, and worth being explicit about why, because the
/// differences are not cosmetic:
///
/// * **There is no checkout API.** A customer is sent to a product's own
///   permalink with query parameters; nothing is created server-side first.
///   So a plan's `priceIds.gumroad` is a product *permalink*, not an id.
/// * **Pings are form-encoded**, not JSON, and arrive with a flat body.
/// * **Pings are not signed.** Gumroad offers no signature header of any
///   kind, so the only thing that can authenticate one is a secret the
///   operator puts in the ping URL themselves — see [`GumroadConfig::ping_token`].
/// * **There is no cancellation endpoint** in Gumroad's public API, so a
///   customer cancels from their own Gumroad subscription page.
///
/// It is offered anyway because it is the least work for a small platform to
/// start charging with: one product per plan and a URL, with Gumroad acting as
/// merchant of record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GumroadConfig {
    /// An application access token from Gumroad's API settings. Used to read a
    /// sale or a subscriber back — which is how an unsigned ping is confirmed
    /// to be real — and to register the ping endpoint.
    #[serde(default, alias = "accessToken")]
    pub access_token: Option<String>,
    /// The seller's Gumroad subdomain, so checkout links are
    /// `https://<subdomain>.gumroad.com/l/<permalink>`. Empty falls back to
    /// `gumroad.com/l/<permalink>`, which also works.
    #[serde(default)]
    pub subdomain: Option<String>,
    /// A secret the operator appends to the ping URL they paste into Gumroad
    /// (`https://…/webhooks/gumroad?token=…`), since Gumroad signs nothing.
    /// Pings are rejected without it rather than trusted — an unverified
    /// billing event is how a stranger grants themselves a plan.
    #[serde(default, alias = "pingToken")]
    pub ping_token: Option<String>,
}

/// Transactional mail the *platform* sends: confirm your address, your site is
/// ready, your card was declined.
///
/// Nothing to do with the newsletters a tenant sends — those go through that
/// tenant's own Ghost mail config, which is separate on purpose so one
/// tenant's sending reputation cannot be spent by another's.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailConfig {
    /// `smtp` or `log`. `log` writes the message to the log instead of
    /// sending it, which is what development should use.
    #[serde(default = "default_mail_transport")]
    pub transport: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub pass: Option<String>,
    #[serde(default, alias = "fromAddress")]
    pub from_address: Option<String>,
    #[serde(default, alias = "fromName")]
    pub from_name: Option<String>,
    /// Where a customer's reply goes, if that is not the from address.
    #[serde(default, alias = "replyTo")]
    pub reply_to: Option<String>,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            transport: default_mail_transport(),
            host: None,
            port: None,
            user: None,
            pass: None,
            from_address: None,
            from_name: None,
            reply_to: None,
        }
    }
}

fn default_mail_transport() -> String {
    "log".into()
}

/// Slugs that must never become a tenant subdomain.
///
/// Two separate hazards. Some of these would shadow the platform itself if the
/// dashboard shares a zone with tenant subdomains (`app`, `admin`, `billing`).
/// The rest are the names that make a subdomain dangerous rather than merely
/// confusing: `www` and `mail` collide with infrastructure, and the others are
/// what a phishing page wants to be called.
pub const RESERVED_SLUGS: &[&str] = &[
    "www", "api", "admin", "app", "dashboard", "billing", "pro", "status", "help", "support",
    "docs", "blog", "mail", "smtp", "imap", "pop", "ftp", "ns", "ns1", "ns2", "dns", "mx", "cdn",
    "static", "assets", "img", "images", "media", "files", "download", "downloads", "test",
    "staging", "dev", "demo", "localhost", "internal", "private", "secure", "ssl", "login",
    "signin", "signup", "register", "account", "accounts", "auth", "oauth", "sso", "password",
    "reset", "verify", "payment", "payments", "pay", "checkout", "invoice", "invoices",
    "subscribe", "unsubscribe", "webhook", "webhooks", "ghost", "about", "contact", "legal",
    "privacy", "terms", "security", "abuse", "postmaster", "hostmaster", "webmaster", "root",
    "system", "metrics", "health", "monitor", "backup", "backups",
];

impl Config {
    /// Load config for `env` (from `PRO_ENV`, else `NODE_ENV`, else
    /// `development`).
    pub fn load(env: Option<&str>) -> Result<Self> {
        let env = env
            .map(str::to_string)
            .or_else(|| std::env::var("PRO_ENV").ok().or_else(|| std::env::var("NODE_ENV").ok()))
            .unwrap_or_else(|| "development".into());

        let base_dir = std::env::current_dir()?;
        let mut merged = default_config_value(&env);

        let file = base_dir.join("pro.json");
        if file.exists() {
            let raw = std::fs::read_to_string(&file)?;
            let val: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| ProError::Config(format!("{}: {e}", file.display())))?;
            merge_into(&mut merged, val);
        }
        // A per-environment file layered on top, so one checkout can hold both
        // a development and a production config without them being the same
        // file.
        let env_file = base_dir.join(format!("pro.{env}.json"));
        if env_file.exists() {
            let raw = std::fs::read_to_string(&env_file)?;
            let val: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| ProError::Config(format!("{}: {e}", env_file.display())))?;
            merge_into(&mut merged, val);
        }

        apply_env_overrides(&mut merged);

        let mut cfg: Config = serde_json::from_value(merged)
            .map_err(|e| ProError::Config(format!("invalid config: {e}")))?;
        cfg.env = env;
        cfg.base_dir = base_dir;

        if cfg.plans.is_empty() {
            cfg.plans = default_plans();
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject a config that would produce a broken platform, at boot, with a
    /// reason — rather than at the first signup, with a stack trace.
    fn validate(&self) -> Result<()> {
        if !self.url.contains("://") {
            return Err(ProError::Config(format!("url needs a scheme: {}", self.url)));
        }
        if self.subdomain_base.trim().is_empty() {
            return Err(ProError::Config("subdomainBase must be set".into()));
        }
        if self.subdomain_base.contains("://") || self.subdomain_base.contains('/') {
            return Err(ProError::Config(format!(
                "subdomainBase is a bare hostname, not a url: {}",
                self.subdomain_base
            )));
        }
        if self.tenants.port_ceiling <= self.tenants.port_floor {
            return Err(ProError::Config("tenants.portCeiling must exceed tenants.portFloor".into()));
        }
        const KNOWN_PROVIDERS: &[&str] =
            &["none", "stripe", "paddle", "lemonsqueezy", "gumroad"];
        if !KNOWN_PROVIDERS.contains(&self.billing.provider.as_str()) {
            return Err(ProError::Config(format!(
                "billing.provider must be one of {}: got {}",
                KNOWN_PROVIDERS.join(", "),
                self.billing.provider
            )));
        }
        // A provider that is selected but missing its keys is the failure that
        // is worst to discover late: signup works right up to the checkout,
        // and then every customer sees an error. Caught here instead.
        match self.billing.provider.as_str() {
            "stripe" => {
                if self.billing.stripe.secret_key.is_none() {
                    return Err(ProError::Config(
                        "billing.provider is stripe but billing.stripe.secretKey is unset".into(),
                    ));
                }
            }
            "paddle" => {
                if self.billing.paddle.api_key.is_none() {
                    return Err(ProError::Config(
                        "billing.provider is paddle but billing.paddle.apiKey is unset".into(),
                    ));
                }
            }
            "lemonsqueezy" => {
                let ls = &self.billing.lemonsqueezy;
                if ls.api_key.is_none() {
                    return Err(ProError::Config(
                        "billing.provider is lemonsqueezy but billing.lemonsqueezy.apiKey is unset".into(),
                    ));
                }
                if ls.store_id.is_none() {
                    return Err(ProError::Config(
                        "billing.provider is lemonsqueezy but billing.lemonsqueezy.storeId is unset".into(),
                    ));
                }
            }
            "gumroad" => {
                let g = &self.billing.gumroad;
                if g.access_token.is_none() {
                    return Err(ProError::Config(
                        "billing.provider is gumroad but billing.gumroad.accessToken is unset".into(),
                    ));
                }
                // Gumroad signs nothing, so without this a ping endpoint is
                // open to anyone who learns its URL — and a forged ping is a
                // free plan. Required at boot rather than left to fail
                // closed later, where an operator would see only silence.
                if g.ping_token.is_none() {
                    return Err(ProError::Config(
                        "billing.provider is gumroad but billing.gumroad.pingToken is unset — \
                         Gumroad does not sign its pings, so this secret is what authenticates \
                         them. Put it in the ping URL: /webhooks/gumroad?token=…"
                            .into(),
                    ));
                }
            }
            _ => {}
        }
        // A provider with no sellable plan is a real problem, but no longer a
        // *config file* problem: product ids are set from the console's Plans
        // screen and stored as settings, so this file is only one of the two
        // places they can come from. Refusing to boot over it would mean a
        // platform whose ids live in the console could not start at all.
        //
        // It is still reported in every place an operator looks — `doctor`,
        // the console overview, the Settings banner and the Plans screen —
        // each of which reads the live catalogue rather than this file.
        if self.billing.provider != "none"
            && !self.plans.iter().any(|p| p.is_purchasable(&self.billing.provider))
        {
            tracing::warn!(
                "no plan in this file has a {} product id. If they are set in the console \
                 this is fine; otherwise nothing can be bought — see /console/plans",
                self.billing.provider
            );
        }
        if self.ssl.enabled && self.ssl.email.is_none() {
            // Not fatal: Let's Encrypt accepts an order with no contact. But
            // it is the address that would warn an operator before a renewal
            // failure took their whole platform offline.
            tracing::warn!(
                "ssl.enabled is on with no ssl.email — nobody will be told if renewal starts failing"
            );
        }
        Ok(())
    }

    /// Resolve a configured path against the deployment directory.
    ///
    /// A relative path in `pro.json` is relative to the directory the config
    /// lives in, not to whatever directory the process happened to be started
    /// from. That is what makes a config portable between machines: the
    /// binary and themes this platform needs belong to a *different* checkout
    /// (Ghost-rust), and `../../Ghost-rust/target/release/ghost` names it
    /// correctly wherever the pair is cloned, while an absolute path names it
    /// on exactly one machine. A clone that had to be hand-edited before it
    /// would start is the thing being avoided here.
    pub fn resolve_path(&self, configured: &str) -> PathBuf {
        let p = Path::new(configured);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.base_dir.join(p)
        }
    }

    pub fn data_dir(&self) -> PathBuf {
        self.resolve_path(&self.paths.data_dir)
    }

    /// The `ghost` binary as a path, or `None` when it is a bare name that
    /// should be looked up on `PATH` the way a shell would.
    pub fn ghost_binary_path(&self) -> Option<PathBuf> {
        let p = Path::new(&self.paths.ghost_binary);
        (p.is_absolute() || p.components().count() > 1)
            .then(|| self.resolve_path(&self.paths.ghost_binary))
    }

    /// The themes copied into every new tenant, if any are configured.
    pub fn theme_source_path(&self) -> Option<PathBuf> {
        self.paths.theme_source.as_deref().map(|p| self.resolve_path(p))
    }

    /// The control-plane database as a SQLite file.
    ///
    /// Still meaningful on a MySQL deployment: it is where a local database
    /// *would* be, which is what `migrate` and an import both need in order
    /// to find the file being moved off.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join("pro.db")
    }

    /// The database the control plane runs on.
    ///
    /// This is the function that decides SQLite versus MySQL, and it is the
    /// only one that does. A configured `database.url` wins; with none, the
    /// local file. See [`crate::database`] for why presence of a URL is the
    /// trigger rather than `PRO_ENV`.
    pub fn db_target(&self) -> Result<crate::database::Target> {
        match self.database.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(url) => crate::database::Target::parse(url),
            None => Ok(crate::database::Target::sqlite_file(&self.db_path())),
        }
    }

    /// Where each tenant's own database lives.
    ///
    /// Independent of [`Self::db_target`]: tenants can stay on their own
    /// files while the control plane is on MySQL. Only an explicit
    /// `database.tenantUrl` moves them.
    pub fn tenant_store(&self) -> Result<crate::database::TenantStore> {
        let Some(url) = self.database.tenant_url.as_deref().map(str::trim).filter(|u| !u.is_empty())
        else {
            return Ok(crate::database::TenantStore::SqliteFile);
        };

        // Parsed through the same gate as the control-plane URL so that a
        // `postgres://` here is refused at boot rather than at the first
        // signup, when a customer is waiting for a site.
        let target = crate::database::Target::parse(url)?;
        if target.engine() != crate::database::Engine::MySql {
            return Err(ProError::Config(format!(
                "database.tenantUrl must be a mysql:// server, not {}. Leave it \
                 unset to keep one SQLite file per tenant",
                target.engine().name()
            )));
        }

        // A database name in the server URL is a mistake worth catching: each
        // tenant gets its own, so a name here would be silently ignored and
        // the operator would wonder where their data went.
        let server = url.trim_end_matches('/');
        let after_host = server.split_once("://").map(|(_, r)| r).unwrap_or(server);
        if let Some((_, path)) = after_host.split_once('/') {
            if !path.is_empty() {
                return Err(ProError::Config(format!(
                    "database.tenantUrl names a database ({path}), but each tenant \
                     gets its own — give the server only, as mysql://user:pass@host:3306"
                )));
            }
        }

        Ok(crate::database::TenantStore::MySqlServer {
            server: server.to_string(),
            prefix: self.database.tenant_prefix.clone(),
        })
    }

    /// Root of all tenant directories; one subdirectory per tenant.
    pub fn tenants_dir(&self) -> PathBuf {
        self.data_dir().join("tenants")
    }

    /// Everything a single tenant owns: its config, its database, its
    /// uploads. One directory, so a backup is one archive and a move between
    /// machines is one copy.
    pub fn tenant_dir(&self, tenant_id: &str) -> PathBuf {
        self.tenants_dir().join(tenant_id)
    }

    /// Where issued certificates are cached between restarts.
    pub fn cert_dir(&self) -> PathBuf {
        self.data_dir().join("certs")
    }

    pub fn backup_dir(&self) -> PathBuf {
        self.data_dir().join("backups")
    }

    /// Per-tenant process logs, so one tenant's noise is separable from the
    /// platform's.
    pub fn log_dir(&self) -> PathBuf {
        self.data_dir().join("logs")
    }

    /// The hostname the dashboard is served on.
    pub fn host(&self) -> String {
        self.url
            .splitn(4, '/')
            .nth(2)
            .unwrap_or_default()
            .rsplit_once(':')
            .filter(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
            .map(|(h, _)| h.to_string())
            .unwrap_or_else(|| self.url.splitn(4, '/').nth(2).unwrap_or_default().to_string())
    }

    /// The platform url with no trailing slash, which every link is built from.
    pub fn canonical_url(&self) -> String {
        self.url.trim_end_matches('/').to_string()
    }

    /// Whether [`Self::host`] is an address only this machine or this network
    /// can resolve.
    ///
    /// One function because three callers need the same answer and must not
    /// each invent it: a payment provider's webhook, an ACME challenge and a
    /// customer's DNS all depend on this platform being reachable from
    /// outside, and a hostname that is not is the single cause behind all
    /// three failing.
    pub fn host_is_local(&self) -> bool {
        let host = self.host().to_ascii_lowercase();
        let host = host.trim_start_matches('[').trim_end_matches(']');

        if host.is_empty()
            || host == "localhost"
            || host == "::1"
            || host == "0.0.0.0"
            || host.ends_with(".localhost")
        {
            return true;
        }
        // Reserved and special-use suffixes: never delegated in the public DNS,
        // so nothing outside can look one up.
        if [".local", ".localdomain", ".internal", ".home.arpa", ".test", ".invalid", ".example"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
        {
            return true;
        }
        // Literal addresses in the loopback, private and link-local ranges.
        if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
            return ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified();
        }
        if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
            return ip.is_loopback() || ip.is_unspecified();
        }
        false
    }

    /// Why a payment provider's webhook could not reach this platform, or
    /// `None` when it can.
    ///
    /// This is the failure that looks like a broken button. A provider takes
    /// the customer's money and then calls back to say so, and reopening the
    /// site happens *only* on that callback — so when the callback cannot
    /// arrive, checkout completes, the money moves, and the site stays shut
    /// with the same "Complete payment" banner on it. Nothing in the checkout
    /// path can detect it: from here the checkout was created successfully.
    ///
    /// So it is reported where the configuration is, by `doctor` and by the
    /// console, rather than waited for.
    pub fn webhook_unreachable_reason(&self) -> Option<String> {
        if !self.host_is_local() {
            return None;
        }
        Some(format!(
            "{} is not an address a payment provider can reach, so the webhook that \
             confirms a payment will never arrive — a customer would be charged and \
             their site would stay closed",
            self.host()
        ))
    }

    /// The full hostname for a tenant slug.
    pub fn tenant_host(&self, slug: &str) -> String {
        format!("{slug}.{}", self.subdomain_base.trim_matches('.'))
    }

    /// The url a tenant's site is reachable at, which becomes the `url` in its
    /// own Ghost config and therefore the base of every link it emits.
    pub fn tenant_url(&self, slug: &str) -> String {
        let scheme = if self.public_tls() { "https" } else { "http" };
        let host = self.tenant_host(slug);
        // In development the edge is on a port that is not the scheme's
        // default, and a tenant url without it points nowhere.
        match self.edge_port() {
            443 | 80 => format!("{scheme}://{host}"),
            port => format!("{scheme}://{host}:{port}"),
        }
    }

    /// Whether visitors reach this platform over HTTPS — true when the edge
    /// terminates TLS itself, and also when something in front of it does.
    ///
    /// This is the one that decides a tenant's scheme, because a tenant's
    /// `url` has to describe what the *browser* sees, not what the proxy
    /// speaks to the tenant process.
    pub fn public_tls(&self) -> bool {
        self.ssl.enabled || self.ssl.behind_proxy || self.url.starts_with("https://")
    }

    /// The port the edge binds: explicit if set, else 443 with TLS and 8080
    /// without.
    pub fn edge_port(&self) -> u16 {
        self.server.port.unwrap_or(if self.ssl.enabled { 443 } else { 8080 })
    }

    pub fn plan(&self, slug: &str) -> Option<&Plan> {
        self.plans.iter().find(|p| p.slug == slug)
    }

    /// Plans a new customer may buy, cheapest first.
    pub fn purchasable_plans(&self) -> Vec<&Plan> {
        let mut plans: Vec<&Plan> = if self.billing.provider == "none" {
            // Nothing is being charged for, so every visible plan is on offer
            // — a price id would be meaningless.
            self.plans.iter().filter(|p| !p.hidden).collect()
        } else {
            self.plans.iter().filter(|p| p.is_purchasable(&self.billing.provider)).collect()
        };
        plans.sort_by_key(|p| (p.sort_order, p.price_cents));
        plans
    }

    /// Where a customer manages their subscription. Handed to tenants as the
    /// `hostSettings.billing.url` their admin screens link to when a limit is
    /// hit.
    pub fn billing_url(&self) -> String {
        format!("{}/billing", self.canonical_url())
    }

    /// Whether `slug` may be registered: shape, length, and not reserved.
    ///
    /// The shape rules are DNS's, not ours — a label may not exceed 63
    /// characters or start or end with a hyphen, and a slug that breaks those
    /// produces a hostname no certificate authority will issue for.
    pub fn slug_available(&self, slug: &str) -> Result<()> {
        if slug.len() < 3 {
            return Err(ProError::BadRequest("Site address must be at least 3 characters.".into()));
        }
        if slug.len() > 63 {
            return Err(ProError::BadRequest("Site address must be 63 characters or fewer.".into()));
        }
        if !slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(ProError::BadRequest(
                "Site address may only contain lowercase letters, numbers and hyphens.".into(),
            ));
        }
        if slug.starts_with('-') || slug.ends_with('-') {
            return Err(ProError::BadRequest("Site address may not start or end with a hyphen.".into()));
        }
        // `xn--` is the punycode prefix: a slug that begins with it claims to
        // be an internationalised name and would resolve to something other
        // than what it reads as.
        if slug.starts_with("xn--") {
            return Err(ProError::BadRequest("Site address may not start with “xn--”.".into()));
        }
        if RESERVED_SLUGS.contains(&slug) || self.reserved_slugs.iter().any(|s| s == slug) {
            return Err(ProError::Conflict(format!("“{slug}” is reserved. Please choose another.")));
        }
        Ok(())
    }
}

fn default_config_value(env: &str) -> serde_json::Value {
    let dev = env == "development";
    serde_json::json!({
        // Development defaults that work with no config file at all. The
        // `.localhost` suffix resolves to loopback in every current browser
        // without a hosts-file entry, which is what makes wildcard subdomain
        // routing testable locally.
        "url": if dev { "http://pro.localhost:8080" } else { "" },
        "subdomainBase": if dev { "localhost" } else { "" },
        "server": { "host": "0.0.0.0", "httpPort": if dev { 0 } else { 80 } },
        "paths": { "dataDir": "data", "ghostBinary": "ghost" },
        "tenants": {},
        "ssl": { "enabled": false },
        "billing": { "provider": "none" },
        "mail": { "transport": if dev { "log" } else { "smtp" } },
        "plans": [],
        "reservedSlugs": [],
    })
}

/// Deep-merge `src` over `dst`, object by object. A non-object replaces
/// whatever was there, so an array in a config file wins outright rather than
/// being concatenated — which is what makes `plans` a replacement.
fn merge_into(dst: &mut serde_json::Value, src: serde_json::Value) {
    match (dst, src) {
        (serde_json::Value::Object(d), serde_json::Value::Object(s)) => {
            for (k, v) in s {
                match d.get_mut(&k) {
                    Some(existing) => merge_into(existing, v),
                    None => {
                        d.insert(k, v);
                    }
                }
            }
        }
        (d, s) => *d = s,
    }
}

fn apply_env_overrides(cfg: &mut serde_json::Value) {
    for (k, v) in std::env::vars() {
        if !k.starts_with("PRO_") || k == "PRO_ENV" {
            continue;
        }
        let path: Vec<String> = k["PRO_".len()..].split("__").map(snake_to_camel).collect();
        if path.iter().any(String::is_empty) {
            continue;
        }
        set_path(cfg, &path, parse_env_value(&v));
    }
}

fn set_path(cfg: &mut serde_json::Value, path: &[String], val: serde_json::Value) {
    let last = path.len() - 1;
    let mut cur = cfg;
    for (i, seg) in path.iter().enumerate() {
        if i == last {
            cur[seg.as_str()] = val;
            return;
        }
        if !cur.get(seg.as_str()).map(serde_json::Value::is_object).unwrap_or(false) {
            cur[seg.as_str()] = serde_json::json!({});
        }
        cur = cur.get_mut(seg.as_str()).unwrap();
    }
}

/// Interpret an environment value as the JSON it looks like.
///
/// Objects and arrays are parsed, which is the difference from Ghost's loader
/// and the reason this platform can set a whole plan or a provider's keys from
/// the environment. Everything else is scalar-parsed, with one deliberate
/// exception: a string of digits long enough to be an id — a Lemon Squeezy
/// variant, a Paddle numeric — stays a string, because turning `"812345"`
/// into `812345` makes it compare unequal to the id every API returns.
fn parse_env_value(v: &str) -> serde_json::Value {
    let t = v.trim();
    match t {
        "true" => return serde_json::json!(true),
        "false" => return serde_json::json!(false),
        "null" => return serde_json::json!(null),
        _ => {}
    }
    if (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(t) {
            return parsed;
        }
    }
    if let Ok(n) = t.parse::<i64>() {
        // A leading zero or a very long run of digits is an identifier
        // somebody wrote, not a quantity.
        if !(t.len() > 1 && t.starts_with('0')) && t.len() <= 9 {
            return serde_json::json!(n);
        }
    }
    serde_json::json!(v)
}

fn snake_to_camel(s: &str) -> String {
    let mut out = String::new();
    let mut upper_next = false;
    for c in s.chars() {
        if c == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            url: "https://pro.example.com".into(),
            subdomain_base: "sites.example.com".into(),
            server: ServerConfig { host: "0.0.0.0".into(), port: Some(443), http_port: 80 },
            paths: PathsConfig::default(),
            database: DatabaseConfig::default(),
            tenants: TenantsConfig::default(),
            ssl: SslConfig { enabled: true, ..Default::default() },
            billing: BillingConfig::default(),
            mail: MailConfig::default(),
            plans: default_plans(),
            reserved_slugs: vec!["acme".into()],
            public_addresses: vec![],
            env: "production".into(),
            base_dir: PathBuf::from("/srv/pro"),
        }
    }

    #[test]
    fn builds_tenant_hosts_and_urls() {
        let c = cfg();
        assert_eq!(c.tenant_host("alice"), "alice.sites.example.com");
        assert_eq!(c.tenant_url("alice"), "https://alice.sites.example.com");
    }

    /// On a non-default port the tenant url has to carry it, or every link the
    /// site emits points at a port nothing is listening on.
    #[test]
    fn tenant_url_keeps_a_nonstandard_port() {
        let mut c = cfg();
        c.ssl.enabled = false;
        // The platform url is part of the signal `public_tls` reads, so a
        // genuinely plain-HTTP deployment says so here too.
        c.url = "http://pro.localhost:8080".into();
        c.server.port = Some(8080);
        assert_eq!(c.tenant_url("alice"), "http://alice.sites.example.com:8080");
    }

    /// Behind a TLS-terminating proxy the edge speaks plain HTTP, but the
    /// browser is on HTTPS — and the tenant's url must describe the browser's
    /// view, or it will emit `http://` links and set non-secure cookies.
    #[test]
    fn behind_a_proxy_tenants_are_still_https() {
        let mut c = cfg();
        c.ssl.enabled = false;
        c.ssl.behind_proxy = true;
        c.server.port = Some(443);
        assert!(c.public_tls());
        assert_eq!(c.tenant_url("alice"), "https://alice.sites.example.com");
    }

    #[test]
    fn rejects_bad_slugs() {
        let c = cfg();
        assert!(c.slug_available("goodsite").is_ok());
        assert!(c.slug_available("ab").is_err(), "too short");
        assert!(c.slug_available(&"a".repeat(64)).is_err(), "too long for a DNS label");
        assert!(c.slug_available("-lead").is_err());
        assert!(c.slug_available("trail-").is_err());
        assert!(c.slug_available("Caps").is_err());
        assert!(c.slug_available("under_score").is_err());
        assert!(c.slug_available("xn--80ak6aa92e").is_err(), "punycode prefix");
        assert!(c.slug_available("www").is_err(), "built-in reserved");
        assert!(c.slug_available("acme").is_err(), "operator reserved");
    }

    /// A selected provider with no keys must fail at boot, not at the first
    /// customer's checkout. Credentials are a property of this file, so this
    /// file is where they are enforced.
    #[test]
    fn validates_provider_credentials() {
        let mut c = cfg();
        c.billing.provider = "stripe".into();
        assert!(c.validate().is_err(), "no secret key");

        c.billing.stripe.secret_key = Some("sk_test_1".into());
        // A missing *product id* is deliberately not fatal: those can be set
        // from the console's Plans screen, so a file with none is a platform
        // configured elsewhere, not a broken one. It is warned about at boot
        // and reported by `doctor` and in three places in the console.
        assert!(
            c.validate().is_ok(),
            "product ids may live in the console, so their absence must not block boot"
        );

        c.plans[0].price_ids.insert("stripe".into(), "price_1".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validates_lemonsqueezy_needs_a_store() {
        let mut c = cfg();
        c.billing.provider = "lemonsqueezy".into();
        c.billing.lemonsqueezy.api_key = Some("key".into());
        c.plans[0].price_ids.insert("lemonsqueezy".into(), "812345".into());
        assert!(c.validate().is_err(), "store id is required to create a checkout");
        c.billing.lemonsqueezy.store_id = Some("1".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_an_unknown_provider() {
        let mut c = cfg();
        c.billing.provider = "braintree".into();
        assert!(c.validate().is_err());
    }

    /// With no provider the platform still works and every visible plan is on
    /// offer — a price id would mean nothing.
    #[test]
    fn no_provider_offers_every_visible_plan() {
        let c = cfg();
        assert_eq!(c.billing.provider, "none");
        assert_eq!(c.purchasable_plans().len(), default_plans().len());
        assert!(c.validate().is_ok());
    }

    /// Only plans priced at the *active* provider are offered, so a catalogue
    /// mid-migration does not list what cannot be charged for.
    #[test]
    fn offers_only_plans_priced_at_the_active_provider() {
        let mut c = cfg();
        c.billing.provider = "paddle".into();
        c.billing.paddle.api_key = Some("key".into());
        c.plans[0].price_ids.insert("stripe".into(), "price_old".into());
        c.plans[1].price_ids.insert("paddle".into(), "pri_new".into());
        let offered: Vec<&str> = c.purchasable_plans().iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(offered, vec![c.plans[1].slug.as_str()]);
    }

    #[test]
    fn rejects_a_subdomain_base_that_is_a_url() {
        let mut c = cfg();
        c.subdomain_base = "https://sites.example.com".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_an_inverted_port_range() {
        let mut c = cfg();
        c.tenants.port_floor = 30_000;
        c.tenants.port_ceiling = 21_000;
        assert!(c.validate().is_err());
    }

    /// Objects from the environment have to survive as objects — the whole
    /// reason this loader differs from Ghost's.
    #[test]
    fn env_values_parse_as_json_when_they_are_json() {
        assert_eq!(parse_env_value("true"), serde_json::json!(true));
        assert_eq!(parse_env_value("8080"), serde_json::json!(8080));
        assert_eq!(parse_env_value(r#"{"a":1}"#), serde_json::json!({"a": 1}));
        assert_eq!(parse_env_value("[1,2]"), serde_json::json!([1, 2]));
        assert_eq!(parse_env_value("hello"), serde_json::json!("hello"));
    }

    /// Provider ids are strings even when they are all digits. Coercing a
    /// Lemon Squeezy variant id to a number makes it compare unequal to the
    /// string the API returns.
    #[test]
    fn numeric_looking_ids_stay_strings() {
        assert_eq!(parse_env_value("0123"), serde_json::json!("0123"));
        assert_eq!(parse_env_value("1234567890123"), serde_json::json!("1234567890123"));
    }

    #[test]
    fn env_paths_camel_case() {
        assert_eq!(snake_to_camel("SUBDOMAIN_BASE"), "subdomainBase");
        assert_eq!(snake_to_camel("PROVIDER"), "provider");
    }

    /// `plans` in a config file replaces the catalogue rather than merging
    /// into it element by element, so an operator's list is exactly what they
    /// wrote.
    #[test]
    fn arrays_replace_rather_than_merge() {
        let mut base = serde_json::json!({"plans": [{"slug": "a"}, {"slug": "b"}], "ssl": {"enabled": false}});
        merge_into(&mut base, serde_json::json!({"plans": [{"slug": "c"}], "ssl": {"staging": true}}));
        assert_eq!(base["plans"].as_array().unwrap().len(), 1);
        // objects still merge key by key
        assert_eq!(base["ssl"]["enabled"], false);
        assert_eq!(base["ssl"]["staging"], true);
    }

    #[test]
    fn strips_the_port_from_host() {
        let mut c = cfg();
        c.url = "http://pro.localhost:8080".into();
        assert_eq!(c.host(), "pro.localhost");
        assert_eq!(c.canonical_url(), "http://pro.localhost:8080");
    }

    /// The addresses nothing outside this machine can resolve. Getting this
    /// list wrong in either direction matters: a false negative is a platform
    /// that charges customers and never opens their sites, and a false
    /// positive nags a production deployment that is working.
    #[test]
    fn local_hostnames_are_recognised() {
        let mut c = cfg();
        for url in [
            "http://pro.localhost:8080",
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "http://127.1.2.3",
            "http://10.0.0.5:8080",
            "http://192.168.1.20",
            "http://172.16.4.4",
            "http://169.254.1.1",
            "http://0.0.0.0:8080",
            "http://[::1]:8080",
            "https://pro.mymac.local",
            "https://pro.internal",
            "https://pro.home.arpa",
            "https://pro.test",
        ] {
            c.url = url.into();
            assert!(c.host_is_local(), "{url} must be recognised as local");
            assert!(c.webhook_unreachable_reason().is_some(), "{url} must warn");
        }
    }

    #[test]
    fn public_hostnames_are_left_alone() {
        let mut c = cfg();
        for url in [
            "https://pro.example.com",
            "https://ghost-pro.io",
            "http://203.0.113.10",
            // Not a reserved suffix, however much it looks like one.
            "https://pro.testing.example.com",
            "https://localhost.example.com",
        ] {
            c.url = url.into();
            assert!(!c.host_is_local(), "{url} must be treated as public");
            assert_eq!(c.webhook_unreachable_reason(), None, "{url} must not warn");
        }
    }

    /// The reason names the host, because an operator reading it in `doctor`
    /// needs to know which address is the problem.
    #[test]
    fn the_webhook_reason_names_the_host() {
        let mut c = cfg();
        c.url = "http://pro.localhost:8080".into();
        let reason = c.webhook_unreachable_reason().unwrap();
        assert!(reason.contains("pro.localhost"), "got {reason}");
    }
}
