//! `/dashboard/dns-providers`: DNS hosts connected to the account once.
//!
//! A customer who keeps their domains at a supported DNS host (Cloudflare,
//! GoDaddy, Namecheap, HostGator, …) connects API credentials here, one time. From then on, adding a domain hosted there writes
//! its records straight away — no token to paste, no provider dashboard to
//! open. The page also answers "which providers have I connected, and which of
//! my domains do they cover?", and lets the customer replace or disconnect a
//! token.
//!
//! Credentials are checked against the provider before they are kept, sealed
//! with the platform's credential key (`pro_core::secret_box`), and never
//! shown again: only the last four characters of the secret are.
use axum::extract::{Form, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use pro_core::secret_box::SecretBox;
use pro_core::{Account, Domain, DomainKind, DomainStatus, ProError, Result};
use pro_db::dns_providers::DnsProvider;
use pro_proxy::dns_api::{self, Context, Credentials};
use pro_proxy::{dns, domain_connect::{self, Provider}};
use std::collections::HashMap;

use crate::auth;
use crate::reply::{self, redirect_with};
use crate::state::AppState;
use crate::view::{self, attr, esc, BannerKind, Flash};

pub const PAGE: &str = "/dashboard/dns-providers";

/// The JSON list of zones has to fit its column; a connection that sees more
/// zones than this still works, the list on the page is just shortened.
const MAX_ZONES_JSON: usize = 3900;

/// Providers a customer can connect with credentials, in display order.
pub fn connectable() -> impl Iterator<Item = Provider> {
    Provider::ALL.into_iter().filter(|p| dns_api::supports(*p))
}

/// Their names, for one-line summaries.
pub fn connectable_names() -> String {
    connectable().map(|p| p.name()).collect::<Vec<_>>().join(", ")
}

fn secret_box(state: &AppState) -> Result<SecretBox> {
    SecretBox::load_or_create(&state.config.data_dir())
}

/// What the platform tells providers about itself: Namecheap needs the IPv4
/// address calls come from, which is the first configured public IPv4.
pub(crate) fn api_context(state: &AppState) -> Context {
    Context {
        client_ipv4: state.config.public_addresses.iter().find_map(|a| a.trim().parse().ok()),
    }
}

/// What a sealed value is bound to, so it opens only for its own row.
fn aad(account_id: &str, provider: Provider) -> String {
    format!("dns-provider:{account_id}:{}", provider.slug())
}

/// Opened credentials: a JSON object of fields. A bare string is a single
/// token, as the first version stored it.
fn decode(plain: &str) -> Credentials {
    serde_json::from_str(plain).unwrap_or_else(|_| [("token".to_string(), plain.to_string())].into())
}

/// The account's connection to `provider`, with its credentials in the clear.
///
/// `None` when nothing is connected — or when the stored value no longer
/// opens (the key file was replaced), which the page reports as "reconnect".
pub(crate) async fn stored(
    state: &AppState,
    account_id: &str,
    provider: Provider,
) -> Result<Option<(DnsProvider, Credentials)>> {
    let Some(row) = pro_db::dns_providers::get(&state.db, account_id, provider.slug()).await? else {
        return Ok(None);
    };
    let plain = secret_box(state)?.open(&row.secret, &aad(account_id, provider));
    Ok(plain.map(|plain| (row, decode(&plain))))
}

/// Check credentials with the provider and keep them on the account.
pub(crate) async fn connect(
    state: &AppState,
    account: &Account,
    provider: Provider,
    form: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let creds = dns_api::clean(provider, form)?;
    let mut zones = dns_api::verify(provider, &creds, &api_context(state)).await?;
    while serde_json::to_string(&zones).map(|j| j.len()).unwrap_or(0) > MAX_ZONES_JSON {
        zones.pop();
    }
    let sealed = secret_box(state)?.seal(&serde_json::to_string(&creds)?, &aad(&account.id, provider))?;
    pro_db::dns_providers::save(
        &state.db,
        &account.id,
        provider.slug(),
        &sealed,
        &dns_api::hint(provider, &creds),
        &zones,
    )
    .await?;
    pro_db::audit::record(
        &state.db,
        &format!("account:{}", account.id),
        "dns_provider.connected",
        None,
        Some(&account.id),
        serde_json::json!({ "provider": provider.slug(), "zones": zones.len() }),
    )
    .await;
    Ok(zones)
}

/// What happened when a newly added domain was set up automatically.
pub(crate) enum AutoSetup {
    /// The DNS host is not one the account has connected (or not detectable).
    NotConnected,
    /// Records were written; `live` says whether the domain already verifies.
    Done { provider: Provider, live: bool },
    /// A connection exists but the write failed; the reason is for the customer.
    Failed { provider: Provider, reason: String },
}

impl AutoSetup {
    pub fn flash(&self, domain: &str) -> Flash {
        match self {
            AutoSetup::NotConnected => Flash::ok(format!("{domain} added. Set up its DNS to connect it.")),
            AutoSetup::Done { provider, live: true } => Flash::ok(format!(
                "{domain} added and set up through your connected {} account. It is live.",
                provider.name()
            )),
            AutoSetup::Done { provider, live: false } => Flash::ok(format!(
                "{domain} added and its records were created at {}. DNS can take a few minutes to \
                 update; Vuuut Pro keeps checking and connects it on its own.",
                provider.name()
            )),
            AutoSetup::Failed { provider, reason } => Flash::warn(format!(
                "{domain} added, but your connected {} account could not set it up: {reason}",
                provider.name()
            )),
        }
    }
}

/// Publish `domain`'s records with the account's stored credentials for its
/// DNS host, if there are any. Used right after a domain is added, and by
/// the one-click button on the domain page.
pub(crate) async fn auto_setup(state: &AppState, account: &Account, domain: &Domain) -> AutoSetup {
    if domain.kind != DomainKind::Custom || domain.status == DomainStatus::Active {
        return AutoSetup::NotConnected;
    }
    let Ok(Some(authority)) = domain_connect::detect(&domain.domain).await else {
        return AutoSetup::NotConnected;
    };
    let Some(provider) = authority.provider.filter(|p| dns_api::supports(*p)) else {
        return AutoSetup::NotConnected;
    };
    let Ok(Some((row, creds))) = stored(state, &account.id, provider).await else {
        return AutoSetup::NotConnected;
    };
    match apply_with(state, domain, &authority.zone, provider, &creds).await {
        Ok(live) => {
            let _ = pro_db::dns_providers::record_use(&state.db, &row.id, None).await;
            AutoSetup::Done { provider, live }
        }
        Err(error) => {
            let reason = error.to_string();
            let _ = pro_db::dns_providers::record_use(&state.db, &row.id, Some(&reason)).await;
            AutoSetup::Failed { provider, reason }
        }
    }
}

/// Write the domain's records into `zone` at `provider`, then check at once.
/// Returns whether the domain already verifies.
pub(crate) async fn apply_with(
    state: &AppState,
    domain: &Domain,
    zone: &str,
    provider: Provider,
    creds: &Credentials,
) -> Result<bool> {
    let expect = crate::domain_expectation(state);
    if dns::is_apex(&domain.domain, Some(zone)) && expect.addresses.is_empty() {
        return Err(ProError::BadRequest(
            "Root domains can’t be connected until the platform’s address is published.".into(),
        ));
    }
    let instructions = dns::instructions_for_zone(&domain.domain, &domain.verification_token, &expect, Some(zone));
    dns_api::apply(provider, zone, creds, &api_context(state), &instructions.records).await?;
    tracing::info!(domain = %domain.domain, provider = provider.name(), "DNS records written via provider API");
    Ok(crate::verify_domain_now(state, domain).await?.is_servable())
}

/// The credential inputs for `provider`, with ids unique on the page.
pub(crate) fn credential_fields(provider: Provider, id_prefix: &str) -> String {
    dns_api::fields(provider)
        .iter()
        .map(|f| {
            let id = format!("{id_prefix}-{}-{}", provider.slug(), f.key);
            format!(
                r#"<label class="field" for="{id}">
<input type="{kind}" id="{id}" name="{name}" value="" placeholder="{ph}" required autocomplete="off" spellcheck="false">
<span>{label}</span></label>"#,
                id = attr(&id),
                kind = if f.secret { "password" } else { "text" },
                name = attr(f.key),
                ph = attr(if f.placeholder.is_empty() { " " } else { f.placeholder }),
                label = esc(&format!("{} {}", provider.name(), f.label)),
            )
        })
        .collect()
}

/// How to get credentials, and anything else the provider requires.
pub(crate) fn credential_help(state: &AppState, provider: Provider) -> String {
    let (href, how) = dns_api::token_help(provider).unwrap_or(("", ""));
    let mut out = format!(
        r#"<p class="hint">{} <a href="{}" target="_blank" rel="noopener noreferrer">Open {} API settings</a>.</p>"#,
        esc(how),
        attr(href),
        esc(provider.name())
    );
    if let Some(note) = dns_api::requirements(provider, &api_context(state)) {
        out.push_str(&format!(r#"<p class="hint">{}</p>"#, esc(&note)));
    }
    out
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

pub async fn page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/dashboard/signin",
        &chrome,
        async {
            let account = auth::account(&state, &headers).await?;
            let body = page_body(&state, &account).await?;
            Ok(crate::dashboard::shell(
                &state,
                chrome.theme,
                "DNS providers",
                crate::dashboard::Tab::Domains,
                &account,
                Flash::from_query(&query).as_ref(),
                &body,
            ))
        }
        .await,
    )
}

async fn page_body(state: &AppState, account: &Account) -> Result<String> {
    let connected = pro_db::dns_providers::list_for_account(&state.db, &account.id).await?;
    let domains = pro_db::domains::list_for_account(&state.db, &account.id).await?;
    let boxed = secret_box(state)?;

    let mut body = view::page_head(
        "DNS providers",
        "Connect your DNS host once, and every domain you add there is set up for you.",
        r#"<a class="btn text" href="/dashboard/domains">Back to domains</a>"#,
    );

    // Connected first: that is what a returning customer came to check.
    let mut any = false;
    for provider in connectable() {
        if let Some(row) = connected.iter().find(|c| c.provider == provider.slug()) {
            if !any {
                body.push_str(&view::section("Connected"));
                any = true;
            }
            let readable = boxed.open(&row.secret, &aad(&account.id, provider)).is_some();
            body.push_str(&connected_card(state, provider, row, readable, &domains));
        }
    }

    let others: String = connectable()
        .filter(|p| !connected.iter().any(|c| c.provider == p.slug()))
        .map(|p| connect_disclosure(state, p))
        .collect();
    if !others.is_empty() {
        body.push_str(&view::section(if any { "Add another provider" } else { "Connect a provider" }));
        body.push_str(&format!(r#"<div class="card rows settings-providers">{others}</div>"#));
    }

    body.push_str(&view::banner(
        BannerKind::Info,
        "Not listed? Your domain still works: add it, and its page shows the records to copy into \
         your provider by hand. Credentials are checked with the provider before they are saved, \
         stored encrypted, and only ever used to add the records your domains need. \
         Disconnecting deletes them here; revoke them at the provider too if you no longer use them.",
    ));
    Ok(body)
}

fn credential_form(state: &AppState, provider: Provider, button: &str, id_prefix: &str) -> String {
    view::form(
        PAGE,
        &format!(
            r#"<input type="hidden" name="provider" value="{slug}">
<div class="inline-form">{fields}
<button class="btn" type="submit">{icon} {button}</button></div>{help}"#,
            slug = attr(provider.slug()),
            fields = credential_fields(provider, id_prefix),
            icon = view::svg("key"),
            button = esc(button),
            help = credential_help(state, provider),
        ),
    )
}

/// A provider not yet connected: a closed row that opens to its form.
fn connect_disclosure(state: &AppState, provider: Provider) -> String {
    format!(
        r#"<details id="connect-{slug}">
<summary class="row hoverable"><span class="ic tint-slate">{dns}</span><span class="body"><span class="title">{name}</span>
<span class="detail">Not connected</span></span>{chevron}</summary>
<div style="padding:4px 16px 16px">{form}</div></details>"#,
        slug = attr(provider.slug()),
        dns = view::svg("dns"),
        name = esc(provider.name()),
        chevron = view::svg("down"),
        form = credential_form(state, provider, "Connect", "new"),
    )
}

fn connected_card(
    state: &AppState,
    provider: Provider,
    row: &DnsProvider,
    readable: bool,
    domains: &[pro_db::domains::DomainListing],
) -> String {
    let (chip, detail) = if !readable {
        (
            r#"<span class="chip bad">Reconnect needed</span>"#,
            "The saved credentials can no longer be read on this server. Enter them again below.".to_string(),
        )
    } else if let Some(error) = &row.last_error {
        (
            r#"<span class="chip warn">Last use failed</span>"#,
            format!("{} · last attempt failed: {}", esc(&row.hint), esc(error)),
        )
    } else {
        (
            r#"<span class="chip ok">Connected</span>"#,
            format!(
                "{} · connected {} · {}",
                esc(&row.hint),
                esc(&view::when(Some(&row.created_at))),
                match row.last_used_at.as_deref() {
                    Some(at) => format!("last used {}", esc(&view::when(Some(at)))),
                    None => "not used yet".into(),
                },
            ),
        )
    };

    // The customer's own custom domains whose zone this connection reaches.
    let covered: Vec<&str> = domains
        .iter()
        .filter(|l| l.domain.kind == DomainKind::Custom)
        .map(|l| l.domain.domain.as_str())
        .filter(|d| row.zones.iter().any(|z| d == z || d.ends_with(&format!(".{z}"))))
        .collect();
    let zones = if row.zones.is_empty() {
        r#"<p class="hint">The provider did not list the domains these credentials can reach; they are tried whenever you add a domain hosted there.</p>"#.to_string()
    } else {
        let chips: String = row
            .zones
            .iter()
            .map(|z| format!(r#"<span class="chip outline mono">{}</span> "#, esc(z)))
            .collect();
        let yours = if covered.is_empty() {
            String::new()
        } else {
            format!(
                r#"<p class="hint">Covers your domains: {}</p>"#,
                covered.iter().map(|d| format!("<code>{}</code>", esc(d))).collect::<Vec<_>>().join(", ")
            )
        };
        format!(
            r#"<p class="hint" style="margin-bottom:6px">Domains it can manage ({n}):</p><div>{chips}</div>{yours}"#,
            n = row.zones.len(),
        )
    };

    let disconnect = view::action_button(
        &format!("{PAGE}/{}/remove", provider.slug()),
        "Disconnect",
        "text danger small",
        Some(&format!(
            "Disconnect {}?|New domains hosted there will need their DNS set up by hand. Domains already connected keep working.",
            provider.name()
        )),
    );

    view::card(&format!(
        r#"<div class="rows settings">{row}</div><div style="padding:0 16px 8px">{zones}</div>
<details style="padding:0 16px 12px"{open}><summary class="btn text small">Replace credentials</summary>{form}</details>
<div style="padding:0 16px 16px">{disconnect}</div>"#,
        row = view::row("dns", "green", &format!("{} connected", provider.name()), &detail, chip),
        open = if readable { "" } else { " open" },
        form = credential_form(state, provider, "Save", "replace"),
    ))
}

/// `POST /dashboard/dns-providers`: connect or replace credentials.
pub async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect(PAGE, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let provider = form
            .get("provider")
            .and_then(|slug| Provider::from_slug(slug))
            .filter(|p| dns_api::supports(*p))
            .ok_or_else(|| ProError::BadRequest("Choose a DNS provider to connect.".into()))?;
        let zones = connect(&state, &account, provider, &form).await?;
        let message = match zones.len() {
            0 => format!("{} connected.", provider.name()),
            1 => format!("{} connected. It can manage {}.", provider.name(), zones[0]),
            n => format!("{} connected. It can manage {n} domains.", provider.name()),
        };
        Ok(redirect_with(PAGE, Flash::ok(message)))
    }.await)
}

/// `POST /dashboard/dns-providers/{provider}/remove`.
pub async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect(PAGE, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let account = auth::account(&state, &headers).await?;
        let provider = Provider::from_slug(&slug).ok_or_else(|| ProError::NotFound("No such provider.".into()))?;
        if !pro_db::dns_providers::remove(&state.db, &account.id, provider.slug()).await? {
            return Err(ProError::NotFound(format!("{} is not connected.", provider.name())));
        }
        pro_db::audit::record(
            &state.db,
            &format!("account:{}", account.id),
            "dns_provider.disconnected",
            None,
            Some(&account.id),
            serde_json::json!({ "provider": provider.slug() }),
        )
        .await;
        Ok(redirect_with(PAGE, Flash::ok(format!("{} disconnected.", provider.name()))))
    }.await)
}

/// A one-line summary for the Domains page: which providers are connected.
pub(crate) async fn summary_card(state: &AppState, account: &Account) -> Result<String> {
    let connected = pro_db::dns_providers::list_for_account(&state.db, &account.id).await?;
    let (detail, button) = if connected.is_empty() {
        (
            format!(
                "Connect your DNS host once and new domains there are set up automatically. Supported: {}.",
                esc(&connectable_names())
            ),
            "Connect a provider",
        )
    } else {
        let chips: Vec<String> = connected
            .iter()
            .filter_map(|c| Provider::from_slug(&c.provider).map(|p| (p, c.last_error.is_some())))
            .map(|(p, warn)| format!(r#"<span class="chip {}">{}</span>"#, if warn { "warn" } else { "ok" }, esc(p.name())))
            .collect();
        (
            format!("Connected: {} · new domains hosted there are set up automatically.", chips.join(" ")),
            "Manage",
        )
    };
    Ok(view::rows(&view::row(
        "dns",
        "blue",
        "DNS providers",
        &detail,
        &format!(r#"<a class="btn tonal small" href="{PAGE}">{}</a>"#, esc(button)),
    )))
}
