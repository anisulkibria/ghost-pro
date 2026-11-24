//! Write a domain's records straight into its DNS host, using API credentials
//! the customer gives once.
//!
//! This is the automatic path that works today, without waiting for a provider
//! to onboard a Domain Connect template. A customer may connect credentials to
//! their account once (sealed at rest, see `pro_core::secret_box`) or use them
//! for one domain only. Only the records Vuuut Pro needs are touched: routing
//! records at the connected name replace whatever routed it before, and the
//! ownership TXT replaces only an earlier Vuuut Pro verification value.
//!
//! Each provider wants different credentials — one token, a key and secret,
//! a username and key, a cPanel server and token — so they are described by
//! [`fields`] and carried as a small map.
use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use pro_core::{ProError, Result};
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde_json::{json, Value};

use crate::{dns::DnsRecord, domain_connect::Provider};

const ROUTING: [&str; 3] = ["A", "AAAA", "CNAME"];

/// Credentials as given, keyed by [`Field::key`].
pub type Credentials = BTreeMap<String, String>;

/// One credential the customer types in.
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    /// Shown as a password box, and never echoed back.
    pub secret: bool,
    pub placeholder: &'static str,
}

/// What the platform knows about itself that some providers need.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// Namecheap only answers whitelisted IPv4 addresses, and wants to be
    /// told which one the call comes from.
    pub client_ipv4: Option<Ipv4Addr>,
}

/// Providers whose own API we can write to.
pub fn supports(provider: Provider) -> bool {
    !fields(provider).is_empty()
}

pub fn fields(provider: Provider) -> &'static [Field] {
    const TOKEN: &[Field] = &[Field { key: "token", label: "API token", secret: true, placeholder: "" }];
    match provider {
        Provider::Cloudflare | Provider::Hostinger => TOKEN,
        Provider::GoDaddy => &[
            Field { key: "key", label: "API key", secret: false, placeholder: "" },
            Field { key: "secret", label: "API secret", secret: true, placeholder: "" },
        ],
        Provider::Namecheap => &[
            Field { key: "user", label: "Namecheap username", secret: false, placeholder: "" },
            Field { key: "key", label: "API key", secret: true, placeholder: "" },
        ],
        Provider::HostGator => &[
            Field { key: "host", label: "cPanel server", secret: false, placeholder: "gator1234.hostgator.com" },
            Field { key: "user", label: "cPanel username", secret: false, placeholder: "" },
            Field { key: "token", label: "cPanel API token", secret: true, placeholder: "" },
        ],
        Provider::DigitalOcean | Provider::Gandi | Provider::Hetzner => TOKEN,
        Provider::Ionos => &[Field { key: "token", label: "API key (prefix.secret)", secret: true, placeholder: "" }],
        Provider::Porkbun => &[
            Field { key: "key", label: "API key", secret: false, placeholder: "pk1_…" },
            Field { key: "secret", label: "Secret API key", secret: true, placeholder: "sk1_…" },
        ],
        Provider::NameCom => &[
            Field { key: "user", label: "Name.com username", secret: false, placeholder: "" },
            Field { key: "token", label: "API token", secret: true, placeholder: "" },
        ],
    }
}

/// Where the customer creates credentials, and what to do there.
pub fn token_help(provider: Provider) -> Option<(&'static str, &'static str)> {
    match provider {
        Provider::Cloudflare => Some((
            "https://dash.cloudflare.com/profile/api-tokens",
            "Create a token from the “Edit zone DNS” template, limited to this domain’s zone.",
        )),
        Provider::Hostinger => Some((
            "https://hpanel.hostinger.com/profile/api",
            "Create a token under Account → API in hPanel.",
        )),
        Provider::GoDaddy => Some((
            "https://developer.godaddy.com/keys",
            "Create a Production API key and copy both the key and the secret.",
        )),
        Provider::Namecheap => Some((
            "https://ap.www.namecheap.com/settings/tools/apiaccess/",
            "Turn on API access under Profile → Tools → Namecheap API Access, copy the API key, and whitelist the IP address below.",
        )),
        Provider::HostGator => Some((
            "https://portal.hostgator.com/",
            "In cPanel, open Security → Manage API Tokens and create a token. The server name is in your cPanel address (for example gator1234.hostgator.com).",
        )),
        Provider::DigitalOcean => Some((
            "https://cloud.digitalocean.com/account/api/tokens",
            "Generate a personal access token with write access (or the domain read, create, update and delete scopes).",
        )),
        Provider::Porkbun => Some((
            "https://porkbun.com/account/api",
            "Create an API key and copy both the key and the secret.",
        )),
        Provider::Gandi => Some((
            "https://admin.gandi.net/organizations/account/pat",
            "Create a personal access token allowed to manage your domains’ technical configuration (LiveDNS).",
        )),
        Provider::Ionos => Some((
            "https://developer.hosting.ionos.com/keys",
            "Create an API key, then paste it as prefix.secret — the public prefix, a dot, and the secret.",
        )),
        Provider::NameCom => Some((
            "https://www.name.com/account/settings/api",
            "Create a production API token and use it with your Name.com username.",
        )),
        Provider::Hetzner => Some((
            "https://console.hetzner.com/",
            "In the Hetzner Console, open the project that holds your DNS zones → Security → API tokens, and create a Read & Write token.",
        )),
    }
}

/// Anything the customer has to know before connecting, beyond credentials.
pub fn requirements(provider: Provider, ctx: &Context) -> Option<String> {
    match provider {
        Provider::GoDaddy => Some("GoDaddy only allows API access on accounts that meet its eligibility rules; if it refuses the key, set up DNS manually.".into()),
        Provider::Namecheap => Some(match ctx.client_ipv4 {
            Some(ip) => format!(
                "Whitelist this server’s IP address in Namecheap: {ip}. Namecheap enables API access only for accounts with 20+ domains, a $50 balance, or $50 spent in the last two years."
            ),
            None => "Namecheap needs this platform’s public IPv4 address whitelisted, and it is not configured yet. Ask support to connect Namecheap.".into(),
        }),
        Provider::HostGator => Some("Works when your domain’s DNS is managed in a HostGator cPanel account.".into()),
        Provider::Porkbun => Some("Porkbun also needs API access switched on for each domain: Domain Management → the domain → Details → API Access.".into()),
        Provider::Hetzner => Some("Use a token from the new Hetzner Console; tokens from the retired DNS Console no longer work.".into()),
        _ => None,
    }
}

fn clean_secret(value: &str, what: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 400 || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ProError::BadRequest(format!("Paste the {what} exactly as your DNS host shows it.")));
    }
    Ok(value.to_string())
}

/// A HostGator cPanel server name. Only HostGator's own hosts are accepted:
/// the platform will send requests there, and a customer-chosen address
/// must not be able to point those at anything else.
fn clean_cpanel_host(value: &str) -> Result<String> {
    let host = value
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let ok_label = |l: &str| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let ours = ["hostgator.com", "websitewelcome.com"]
        .iter()
        .any(|base| host.strip_suffix(base).is_some_and(|p| p.ends_with('.') && p.len() > 1));
    if !ours || !host.split('.').all(ok_label) {
        return Err(ProError::BadRequest(
            "Enter your HostGator cPanel server, for example gator1234.hostgator.com.".into(),
        ));
    }
    Ok(host)
}

/// Check what the customer typed against [`fields`], returning clean values.
///
/// `form` is keyed by field key; anything else in it is ignored.
pub fn clean(provider: Provider, form: &HashMap<String, String>) -> Result<Credentials> {
    let wanted = fields(provider);
    if wanted.is_empty() {
        return Err(ProError::BadRequest(format!(
            "Automatic setup with credentials is not available for {} yet.",
            provider.name()
        )));
    }
    let mut out = Credentials::new();
    for field in wanted {
        let raw = form.get(field.key).map(String::as_str).unwrap_or("");
        let what = format!("{} {}", provider.name(), field.label);
        let value = match (provider, field.key) {
            (Provider::HostGator, "host") => clean_cpanel_host(raw)?,
            _ => clean_secret(raw, &what)?,
        };
        out.insert(field.key.to_string(), value);
    }
    Ok(out)
}

/// Whether every field has a value (i.e. the form was filled in at all).
pub fn any_given(provider: Provider, form: &HashMap<String, String>) -> bool {
    fields(provider).iter().any(|f| form.get(f.key).is_some_and(|v| !v.trim().is_empty()))
}

/// The last few characters of the main secret, for telling connections apart.
pub fn hint(provider: Provider, creds: &Credentials) -> String {
    let secret = fields(provider)
        .iter()
        .find(|f| f.secret)
        .and_then(|f| creds.get(f.key))
        .map(String::as_str)
        .unwrap_or("");
    let tail: String = secret.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let prefix = match provider {
        Provider::HostGator | Provider::Namecheap | Provider::NameCom => creds.get("user").map(|u| format!("{u} · ")).unwrap_or_default(),
        _ => String::new(),
    };
    format!("{prefix}…{tail}")
}

fn get<'a>(creds: &'a Credentials, key: &str) -> Result<&'a str> {
    creds
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ProError::BadRequest("The saved credentials are incomplete. Connect the provider again.".into()))
}

fn client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| ProError::Unavailable(format!("DNS API client: {e}")))
}

/// Check credentials work and list the zones (domains) they can reach.
///
/// Called when a customer connects a provider to their account, so a typo is
/// caught then, not on their next domain. An empty list means the provider
/// accepted the credentials but would not list zones; writes are then the test.
pub async fn verify(provider: Provider, creds: &Credentials, ctx: &Context) -> Result<Vec<String>> {
    let client = client()?;
    let mut zones = match provider {
        Provider::Cloudflare => {
            let body = send(
                provider,
                client
                    .get(format!("{CLOUDFLARE}/zones"))
                    .bearer_auth(get(creds, "token")?)
                    .query(&[("per_page", "50")]),
            )
            .await?;
            names(&body["result"], "name")
        }
        Provider::Hostinger => {
            let body = send(provider, client.get(HOSTINGER_DOMAINS).bearer_auth(get(creds, "token")?)).await?;
            let list = if body.is_array() { &body } else { &body["data"] };
            names(list, "domain")
        }
        Provider::GoDaddy => {
            let body = send(
                provider,
                godaddy_auth(client.get(format!("{GODADDY}/v1/domains")), creds)?
                    .query(&[("statuses", "ACTIVE"), ("limit", "500")]),
            )
            .await?;
            names(&body, "domain")
        }
        Provider::Namecheap => {
            let doc = namecheap(&client, creds, ctx, "namecheap.domains.getList", &[("PageSize", "100".into())]).await?;
            let xml = roxmltree::Document::parse(&doc).map_err(|_| unexpected(provider))?;
            xml.descendants()
                .filter(|n| n.has_tag_name("Domain"))
                .filter_map(|n| n.attribute("Name").map(str::to_string))
                .collect()
        }
        Provider::HostGator => {
            let data = cpanel(&client, creds, "DomainInfo/list_domains", &[]).await?;
            let mut zones: Vec<String> = data["main_domain"].as_str().map(str::to_string).into_iter().collect();
            for key in ["addon_domains", "parked_domains"] {
                zones.extend(data[key].as_array().into_iter().flatten().filter_map(|d| d.as_str().map(str::to_string)));
            }
            zones
        }
        Provider::DigitalOcean => {
            let body = send(provider, digitalocean_get(&client, creds, "/domains")?.query(&[("per_page", "200")])).await?;
            names(&body["domains"], "name")
        }
        Provider::Porkbun => {
            let body = send(provider, porkbun(&client, creds, "/domain/listAll")?).await?;
            names(&body["domains"], "domain")
        }
        Provider::Gandi => {
            let body = send(provider, client.get(format!("{GANDI}/domains")).bearer_auth(get(creds, "token")?)).await?;
            names(&body, "fqdn")
        }
        Provider::Ionos => {
            let body = send(provider, ionos(&client, creds, Method::GET, "/zones")?).await?;
            names(&body, "name")
        }
        Provider::NameCom => {
            let body = send(provider, namecom(&client, creds, Method::GET, "/domains")?.query(&[("perPage", "1000")])).await?;
            names(&body["domains"], "domainName")
        }
        Provider::Hetzner => {
            let body = send(
                provider,
                client.get(format!("{HETZNER}/zones")).bearer_auth(get(creds, "token")?).query(&[("per_page", "50")]),
            )
            .await?;
            names(&body["zones"], "name")
        }
    };
    zones.iter_mut().for_each(|z| *z = z.trim_end_matches('.').to_ascii_lowercase());
    zones.sort_unstable();
    zones.dedup();
    Ok(zones)
}

/// Publish `records` (names relative to `zone`, `@` for the apex).
pub async fn apply(
    provider: Provider,
    zone: &str,
    creds: &Credentials,
    ctx: &Context,
    records: &[DnsRecord],
) -> Result<()> {
    let client = client()?;
    match provider {
        Provider::Cloudflare => cloudflare(&client, zone, get(creds, "token")?, records).await,
        Provider::Hostinger => hostinger(&client, zone, get(creds, "token")?, records).await,
        Provider::GoDaddy => godaddy(&client, zone, creds, records).await,
        Provider::Namecheap => namecheap_apply(&client, zone, creds, ctx, records).await,
        Provider::HostGator => hostgator(&client, zone, creds, records).await,
        Provider::DigitalOcean => digitalocean_apply(&client, zone, creds, records).await,
        Provider::Porkbun => porkbun_apply(&client, zone, creds, records).await,
        Provider::Gandi => gandi(&client, zone, get(creds, "token")?, records).await,
        Provider::Ionos => ionos_apply(&client, zone, creds, records).await,
        Provider::NameCom => namecom_apply(&client, zone, creds, records).await,
        Provider::Hetzner => hetzner(&client, zone, get(creds, "token")?, records).await,
    }
}

fn names(list: &Value, key: &str) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v[key].as_str().map(str::to_string))
        .collect()
}

fn unexpected(provider: Provider) -> ProError {
    ProError::Unavailable(format!("{} sent a response Vuuut Pro could not read. Try again in a moment.", provider.name()))
}

fn rejected(provider: Provider) -> ProError {
    ProError::BadRequest(format!(
        "{} rejected the credentials. Check they are current and allowed to edit this domain’s DNS.",
        provider.name()
    ))
}

async fn send(provider: Provider, request: RequestBuilder) -> Result<Value> {
    let response = request.send().await.map_err(|_| {
        ProError::Unavailable(format!("Could not reach {}. Try again in a moment.", provider.name()))
    })?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(body);
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        if provider == Provider::GoDaddy && body["code"].as_str() == Some("ACCESS_DENIED") {
            return Err(ProError::BadRequest(
                "GoDaddy accepted the key but does not allow API access on this account. Set up DNS manually, or ask GoDaddy to enable API access.".into(),
            ));
        }
        return Err(rejected(provider));
    }
    let detail = body["errors"][0]["message"]
        .as_str()
        .or_else(|| body["message"].as_str())
        .unwrap_or("unexpected response");
    tracing::warn!(provider = provider.name(), %status, "DNS API: {body}");
    Err(ProError::BadRequest(format!("{} refused the change: {detail}", provider.name())))
}

fn fqdn(name: &str, zone: &str) -> String {
    if name == "@" || name.is_empty() {
        zone.to_string()
    } else {
        format!("{name}.{zone}")
    }
}

fn unquote(value: &str) -> &str {
    value.trim().trim_matches('"')
}

/// Names at which we publish a routing record.
fn routed_names(records: &[DnsRecord]) -> Vec<&str> {
    let mut names: Vec<&str> = records.iter().filter(|r| ROUTING.contains(&r.kind)).map(|r| r.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names
}

// ---- Cloudflare -----------------------------------------------------------

const CLOUDFLARE: &str = "https://api.cloudflare.com/client/v4";

async fn cloudflare(client: &Client, zone: &str, token: &str, records: &[DnsRecord]) -> Result<()> {
    let p = Provider::Cloudflare;
    let call = |method: Method, path: String| client.request(method, format!("{CLOUDFLARE}{path}")).bearer_auth(token);

    let zones = send(p, call(Method::GET, "/zones".into()).query(&[("name", zone)])).await?;
    let zone_id = zones["result"][0]["id"].as_str().ok_or_else(|| {
        ProError::BadRequest(format!(
            "This Cloudflare token cannot see the {zone} zone. Give it Zone → DNS → Edit for {zone}."
        ))
    })?;

    // Clear what would conflict, then add.
    let mut names: Vec<String> = records.iter().map(|r| fqdn(&r.name, zone)).collect();
    names.sort_unstable();
    names.dedup();
    for name in &names {
        let routing_here = records.iter().any(|r| ROUTING.contains(&r.kind) && fqdn(&r.name, zone) == *name);
        let existing = send(
            p,
            call(Method::GET, format!("/zones/{zone_id}/dns_records")).query(&[("name", name.as_str()), ("per_page", "100")]),
        )
        .await?;
        for record in existing["result"].as_array().into_iter().flatten() {
            let kind = record["type"].as_str().unwrap_or_default();
            let content = unquote(record["content"].as_str().unwrap_or_default());
            let clash = (routing_here && ROUTING.contains(&kind))
                || (kind == "TXT" && pro_core::ids::domain_verification_secret(content).is_some());
            if let (true, Some(id)) = (clash, record["id"].as_str()) {
                send(p, call(Method::DELETE, format!("/zones/{zone_id}/dns_records/{id}"))).await?;
            }
        }
    }
    for record in records {
        let content = if record.kind == "TXT" {
            format!("\"{}\"", record.value)
        } else {
            record.value.trim_end_matches('.').to_string()
        };
        let mut body = json!({
            "type": record.kind,
            "name": fqdn(&record.name, zone),
            "content": content,
            "ttl": 1,
        });
        if ROUTING.contains(&record.kind) {
            // Proxied records would hide the platform's address from verification
            // and break certificate issuance.
            body["proxied"] = json!(false);
        }
        send(p, call(Method::POST, format!("/zones/{zone_id}/dns_records")).json(&body)).await?;
    }
    Ok(())
}

// ---- Hostinger ------------------------------------------------------------

const HOSTINGER: &str = "https://developers.hostinger.com/api/dns/v1/zones";
const HOSTINGER_DOMAINS: &str = "https://developers.hostinger.com/api/domains/v1/portfolio";

async fn hostinger(client: &Client, zone: &str, token: &str, records: &[DnsRecord]) -> Result<()> {
    let p = Provider::Hostinger;
    let url = format!("{HOSTINGER}/{zone}");
    let existing = send(p, client.get(&url).bearer_auth(token)).await?;

    // Hostinger's overwrite replaces records with the same name *and* type, so
    // a routing record of another type at the same name (a parking A next to
    // our CNAME, a stale AAAA beside our A) must be removed first.
    let mut stale = Vec::new();
    for set in existing.as_array().into_iter().flatten() {
        let name = set["name"].as_str().unwrap_or_default();
        let kind = set["type"].as_str().unwrap_or_default();
        let ours = records.iter().any(|r| r.name == name && r.kind == kind);
        let routing_here = records.iter().any(|r| r.name == name && ROUTING.contains(&r.kind));
        if routing_here && ROUTING.contains(&kind) && !ours {
            stale.push(json!({ "name": name, "type": kind }));
        }
    }
    if !stale.is_empty() {
        send(p, client.delete(&url).bearer_auth(token).json(&json!({ "filters": stale }))).await?;
    }

    // One record set per name and type.
    let zone_body: Vec<Value> = grouped(records)
        .into_iter()
        .map(|(name, kind, values)| {
            let records: Vec<Value> = values.iter().map(|v| json!({ "content": v })).collect();
            json!({ "name": name, "type": kind, "ttl": 300, "records": records })
        })
        .collect();
    send(
        p,
        client.put(&url).bearer_auth(token).json(&json!({ "overwrite": true, "zone": zone_body })),
    )
    .await?;
    Ok(())
}

/// Records grouped by (name, type), values in order.
fn grouped(records: &[DnsRecord]) -> Vec<(&str, &'static str, Vec<&str>)> {
    let mut sets: Vec<(&str, &'static str, Vec<&str>)> = Vec::new();
    for record in records {
        match sets.iter_mut().find(|(n, k, _)| *n == record.name && *k == record.kind) {
            Some(set) => set.2.push(&record.value),
            None => sets.push((&record.name, record.kind, vec![&record.value])),
        }
    }
    sets
}

// ---- GoDaddy --------------------------------------------------------------

const GODADDY: &str = "https://api.godaddy.com";

fn godaddy_auth(request: RequestBuilder, creds: &Credentials) -> Result<RequestBuilder> {
    Ok(request.header(
        "Authorization",
        format!("sso-key {}:{}", get(creds, "key")?, get(creds, "secret")?),
    ))
}

async fn godaddy(client: &Client, zone: &str, creds: &Credentials, records: &[DnsRecord]) -> Result<()> {
    let p = Provider::GoDaddy;
    let base = format!("{GODADDY}/v1/domains/{zone}/records");

    // A routing record of another type at a routed name (GoDaddy's parked
    // A record at the apex beside our CNAME, say) must go first: PUT only
    // replaces records of its own type.
    for name in routed_names(records) {
        for kind in ROUTING {
            if records.iter().any(|r| r.name == name && r.kind == kind) {
                continue;
            }
            let response = godaddy_auth(client.delete(format!("{base}/{kind}/{name}")), creds)?
                .send()
                .await
                .map_err(|_| ProError::Unavailable("Could not reach GoDaddy. Try again in a moment.".into()))?;
            match response.status() {
                s if s.is_success() || s == StatusCode::NOT_FOUND => {}
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(rejected(p)),
                s => tracing::warn!(%s, kind, name, "GoDaddy: could not clear a conflicting record"),
            }
        }
    }
    for (name, kind, values) in grouped(records) {
        let body: Vec<Value> = values
            .iter()
            .map(|v| json!({ "data": v.trim_end_matches('.'), "ttl": 600 }))
            .collect();
        send(p, godaddy_auth(client.put(format!("{base}/{kind}/{name}")), creds)?.json(&body)).await?;
    }
    Ok(())
}

// ---- Namecheap ------------------------------------------------------------

const NAMECHEAP: &str = "https://api.namecheap.com/xml.response";

/// One Namecheap API call, returning the XML when it says `Status="OK"`.
async fn namecheap(
    client: &Client,
    creds: &Credentials,
    ctx: &Context,
    command: &str,
    params: &[(&str, String)],
) -> Result<String> {
    let p = Provider::Namecheap;
    let ip = ctx.client_ipv4.ok_or_else(|| {
        ProError::BadRequest("Namecheap needs this platform’s public IPv4 address, which is not configured. Ask support.".into())
    })?;
    let user = get(creds, "user")?;
    let mut form: Vec<(&str, String)> = vec![
        ("ApiUser", user.to_string()),
        ("ApiKey", get(creds, "key")?.to_string()),
        ("UserName", user.to_string()),
        ("ClientIp", ip.to_string()),
        ("Command", command.to_string()),
    ];
    form.extend(params.iter().cloned());
    // POST: setHosts carries the whole zone, too long for a query string.
    let response = client
        .post(NAMECHEAP)
        .form(&form)
        .send()
        .await
        .map_err(|_| ProError::Unavailable("Could not reach Namecheap. Try again in a moment.".into()))?;
    let text = response.text().await.map_err(|_| unexpected(p))?;
    let doc = roxmltree::Document::parse(&text).map_err(|_| unexpected(p))?;
    if doc.root_element().attribute("Status") == Some("OK") {
        return Ok(text);
    }
    let error = doc.descendants().find(|n| n.has_tag_name("Error"));
    let number = error.and_then(|e| e.attribute("Number")).unwrap_or("");
    let message = error.and_then(|e| e.text()).unwrap_or("unexpected response").trim().to_string();
    tracing::warn!(command, number, "Namecheap API: {message}");
    Err(match number {
        // Invalid request IP / IP not whitelisted.
        "1011150" | "1011147" => ProError::BadRequest(format!(
            "Namecheap refused this server’s IP address ({ip}). Add it to the whitelist under Profile → Tools → API Access."
        )),
        // Invalid ApiUser / ApiKey / disabled API.
        "1011102" | "1010101" | "1010102" | "1011101" | "1011104" => rejected(p),
        _ => ProError::BadRequest(format!("Namecheap refused the change: {message}")),
    })
}

/// Namecheap's routing-like record types: anything at a name that would
/// fight our A or CNAME for its traffic.
const NAMECHEAP_ROUTING: [&str; 7] = ["A", "AAAA", "CNAME", "ALIAS", "URL", "URL301", "FRAME"];

#[derive(Clone, Debug, PartialEq)]
struct NcHost {
    name: String,
    kind: String,
    address: String,
    mx_pref: String,
    ttl: String,
}

/// The host list and email setting from a getHosts response.
fn namecheap_hosts(xml: &str) -> Result<(Vec<NcHost>, Option<String>)> {
    let doc = roxmltree::Document::parse(xml).map_err(|_| unexpected(Provider::Namecheap))?;
    let result = doc
        .descendants()
        .find(|n| n.has_tag_name("DomainDNSGetHostsResult"))
        .ok_or_else(|| unexpected(Provider::Namecheap))?;
    if result.attribute("IsUsingOurDNS").is_some_and(|v| v.eq_ignore_ascii_case("false")) {
        return Err(ProError::BadRequest(
            "This domain is not using Namecheap BasicDNS, so its records can’t be set through Namecheap.".into(),
        ));
    }
    let hosts = result
        .children()
        .filter(|n| n.has_tag_name("host") || n.has_tag_name("Host"))
        .map(|n| NcHost {
            name: n.attribute("Name").unwrap_or("@").to_string(),
            kind: n.attribute("Type").unwrap_or("").to_ascii_uppercase(),
            address: n.attribute("Address").unwrap_or("").to_string(),
            mx_pref: n.attribute("MXPref").unwrap_or("10").to_string(),
            ttl: n.attribute("TTL").unwrap_or("1800").to_string(),
        })
        .collect();
    Ok((hosts, result.attribute("EmailType").map(str::to_string)))
}

/// The zone after our change: every existing host kept except the ones ours
/// replace, then ours. setHosts replaces the *whole* zone, so leaving any
/// existing record out of this list would delete it.
fn namecheap_merge(existing: Vec<NcHost>, records: &[DnsRecord]) -> Vec<NcHost> {
    let routed = routed_names(records);
    let mut out: Vec<NcHost> = existing
        .into_iter()
        .filter(|h| {
            let routing_clash = routed.contains(&h.name.as_str()) && NAMECHEAP_ROUTING.contains(&h.kind.as_str());
            let old_ownership = h.kind == "TXT" && pro_core::ids::domain_verification_secret(unquote(&h.address)).is_some();
            !routing_clash && !old_ownership
        })
        .collect();
    out.extend(records.iter().map(|r| NcHost {
        name: r.name.clone(),
        kind: r.kind.to_string(),
        address: r.value.clone(),
        mx_pref: "10".into(),
        ttl: "1800".into(),
    }));
    out
}

async fn namecheap_apply(
    client: &Client,
    zone: &str,
    creds: &Credentials,
    ctx: &Context,
    records: &[DnsRecord],
) -> Result<()> {
    let suffix = psl::suffix_str(zone).filter(|s| *s != zone).ok_or_else(|| {
        ProError::BadRequest(format!("{zone} is not a domain Namecheap can manage."))
    })?;
    let sld = zone.strip_suffix(suffix).and_then(|s| s.strip_suffix('.')).unwrap_or(zone);
    let target = [("SLD", sld.to_string()), ("TLD", suffix.to_string())];

    let xml = namecheap(client, creds, ctx, "namecheap.domains.dns.getHosts", &target).await?;
    let (existing, email_type) = namecheap_hosts(&xml)?;
    let hosts = namecheap_merge(existing, records);

    let mut params: Vec<(&str, String)> = target.to_vec();
    let mut owned: Vec<(String, String)> = Vec::new();
    for (i, h) in hosts.iter().enumerate() {
        let n = i + 1;
        owned.push((format!("HostName{n}"), h.name.clone()));
        owned.push((format!("RecordType{n}"), h.kind.clone()));
        owned.push((format!("Address{n}"), h.address.clone()));
        owned.push((format!("MXPref{n}"), h.mx_pref.clone()));
        owned.push((format!("TTL{n}"), h.ttl.clone()));
    }
    // Without EmailType, setHosts resets the domain's mail settings.
    if let Some(email) = email_type {
        owned.push(("EmailType".into(), email));
    }
    params.extend(owned.iter().map(|(k, v)| (k.as_str(), v.clone())));
    namecheap(client, creds, ctx, "namecheap.domains.dns.setHosts", &params).await?;
    Ok(())
}

// ---- HostGator (cPanel UAPI) ----------------------------------------------

/// One cPanel UAPI call, returning `data` when `status` is 1.
async fn cpanel(client: &Client, creds: &Credentials, function: &str, form: &[(&str, String)]) -> Result<Value> {
    let p = Provider::HostGator;
    // Re-checked here as well as on input: this host is where the request goes.
    let host = clean_cpanel_host(get(creds, "host")?)?;
    let request = client
        .post(format!("https://{host}:2083/execute/{function}"))
        .header("Authorization", format!("cpanel {}:{}", get(creds, "user")?, get(creds, "token")?))
        .form(form);
    let response = request.send().await.map_err(|_| {
        ProError::Unavailable(format!("Could not reach {host}. Check the cPanel server name and try again."))
    })?;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(rejected(p));
    }
    let body: Value = response.json().await.map_err(|_| unexpected(p))?;
    if body["status"].as_i64() == Some(1) {
        return Ok(body["data"].clone());
    }
    let message = body["errors"][0].as_str().unwrap_or("unexpected response").to_string();
    tracing::warn!(function, "cPanel API: {message}");
    Err(ProError::BadRequest(format!("HostGator refused the change: {message}")))
}

fn b64(value: &Value) -> String {
    value
        .as_str()
        .and_then(|s| STANDARD.decode(s).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default()
}

/// A zone-file owner name made relative to `zone` (`@` for the apex).
fn relative(name: &str, zone: &str) -> String {
    let name = name.trim_end_matches('.');
    if name.is_empty() || name.eq_ignore_ascii_case(zone) {
        "@".into()
    } else if let Some(rest) = name.strip_suffix(&format!(".{zone}")) {
        rest.to_string()
    } else {
        name.to_string()
    }
}

/// What to remove from a parsed cPanel zone, and its SOA serial.
fn cpanel_plan(parsed: &Value, zone: &str, records: &[DnsRecord]) -> Result<(String, Vec<i64>)> {
    let routed = routed_names(records);
    let mut serial = None;
    let mut remove = Vec::new();
    for entry in parsed.as_array().into_iter().flatten() {
        if entry["type"].as_str() != Some("record") {
            continue;
        }
        let kind = entry["record_type"].as_str().unwrap_or_default();
        let data: Vec<String> = entry["data_b64"].as_array().into_iter().flatten().map(b64).collect();
        if kind == "SOA" {
            serial = data.get(2).cloned();
            continue;
        }
        let name = relative(&b64(&entry["dname_b64"]), zone);
        let clash = (routed.contains(&name.as_str()) && ROUTING.contains(&kind))
            || (kind == "TXT"
                && data.iter().any(|d| pro_core::ids::domain_verification_secret(unquote(d)).is_some()));
        if clash {
            if let Some(index) = entry["line_index"].as_i64() {
                remove.push(index);
            }
        }
    }
    let serial = serial.ok_or_else(|| unexpected(Provider::HostGator))?;
    Ok((serial, remove))
}

async fn hostgator(client: &Client, zone: &str, creds: &Credentials, records: &[DnsRecord]) -> Result<()> {
    let parsed = cpanel(client, creds, "DNS/parse_zone", &[("zone", zone.to_string())]).await?;
    let (serial, remove) = cpanel_plan(&parsed, zone, records)?;
    let mut form: Vec<(&str, String)> = vec![("zone", zone.to_string()), ("serial", serial)];
    form.extend(remove.into_iter().map(|i| ("remove", i.to_string())));
    for record in records {
        // Absolute owner names: cPanel refuses an empty one for the apex.
        let data = if record.kind == "CNAME" {
            format!("{}.", record.value.trim_end_matches('.'))
        } else {
            record.value.clone()
        };
        let add = json!({
            "dname": format!("{}.", fqdn(&record.name, zone)),
            "ttl": 14400,
            "record_type": record.kind,
            "data": [data],
        });
        form.push(("add", add.to_string()));
    }
    cpanel(client, creds, "DNS/mass_edit_zone", &form).await?;
    Ok(())
}

// ---- Shared: providers that list records with ids ------------------------

/// A record already at the provider, as far as conflicts are concerned.
#[derive(Debug)]
struct Existing {
    id: String,
    /// Relative to the zone, `@` for the apex.
    name: String,
    kind: String,
    content: String,
}

/// The existing records ours replace: routing records at a name we route,
/// and any earlier Vuuut Pro ownership TXT. Nothing else is touched.
fn conflicts<'a>(existing: &'a [Existing], records: &[DnsRecord]) -> Vec<&'a Existing> {
    let routed = routed_names(records);
    existing
        .iter()
        .filter(|e| {
            let kind = e.kind.to_ascii_uppercase();
            (routed.contains(&e.name.as_str()) && ROUTING.contains(&kind.as_str()))
                || (kind == "TXT" && pro_core::ids::domain_verification_secret(unquote(&e.content)).is_some())
        })
        .collect()
}

fn id_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// A DELETE where "already gone" is fine.
async fn delete_ok(provider: Provider, request: RequestBuilder) -> Result<()> {
    let response = request
        .send()
        .await
        .map_err(|_| ProError::Unavailable(format!("Could not reach {}. Try again in a moment.", provider.name())))?;
    match response.status() {
        s if s.is_success() || s == StatusCode::NOT_FOUND => Ok(()),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(rejected(provider)),
        s => {
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(provider = provider.name(), %s, "DNS API delete: {body}");
            Err(ProError::BadRequest(format!("{} refused to remove an old record ({s}).", provider.name())))
        }
    }
}

fn dotted(value: &str) -> String {
    format!("{}.", value.trim_end_matches('.'))
}

// ---- DigitalOcean ---------------------------------------------------------

const DIGITALOCEAN: &str = "https://api.digitalocean.com/v2";

fn digitalocean_get(client: &Client, creds: &Credentials, path: &str) -> Result<RequestBuilder> {
    Ok(client.get(format!("{DIGITALOCEAN}{path}")).bearer_auth(get(creds, "token")?))
}

async fn digitalocean_apply(client: &Client, zone: &str, creds: &Credentials, records: &[DnsRecord]) -> Result<()> {
    let p = Provider::DigitalOcean;
    let token = get(creds, "token")?;
    let body = send(p, digitalocean_get(client, creds, &format!("/domains/{zone}/records"))?.query(&[("per_page", "200")])).await?;
    let existing: Vec<Existing> = body["domain_records"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| Existing {
            id: id_string(&r["id"]),
            name: r["name"].as_str().unwrap_or("@").to_string(),
            kind: r["type"].as_str().unwrap_or_default().to_string(),
            content: r["data"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    for old in conflicts(&existing, records) {
        delete_ok(p, client.delete(format!("{DIGITALOCEAN}/domains/{zone}/records/{}", old.id)).bearer_auth(token)).await?;
    }
    for record in records {
        // DigitalOcean wants a CNAME target fully qualified, with its dot.
        let data = if record.kind == "CNAME" { dotted(&record.value) } else { record.value.clone() };
        send(
            p,
            client
                .post(format!("{DIGITALOCEAN}/domains/{zone}/records"))
                .bearer_auth(token)
                .json(&json!({ "type": record.kind, "name": record.name, "data": data, "ttl": 1800 })),
        )
        .await?;
    }
    Ok(())
}

// ---- Porkbun --------------------------------------------------------------

const PORKBUN: &str = "https://api.porkbun.com/api/json/v3";

fn porkbun(client: &Client, creds: &Credentials, path: &str) -> Result<RequestBuilder> {
    Ok(client
        .post(format!("{PORKBUN}{path}"))
        .header("X-API-Key", get(creds, "key")?)
        .header("X-Secret-API-Key", get(creds, "secret")?)
        .json(&json!({})))
}

async fn porkbun_apply(client: &Client, zone: &str, creds: &Credentials, records: &[DnsRecord]) -> Result<()> {
    let p = Provider::Porkbun;
    let body = send(p, porkbun(client, creds, &format!("/dns/retrieve/{zone}"))?).await?;
    let existing: Vec<Existing> = body["records"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| Existing {
            id: id_string(&r["id"]),
            name: relative(r["name"].as_str().unwrap_or(zone), zone),
            kind: r["type"].as_str().unwrap_or_default().to_string(),
            content: r["content"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    for old in conflicts(&existing, records) {
        send(p, porkbun(client, creds, &format!("/dns/delete/{zone}/{}", old.id))?).await?;
    }
    for record in records {
        let name = if record.name == "@" { "" } else { record.name.as_str() };
        send(
            p,
            porkbun(client, creds, &format!("/dns/create/{zone}"))?.json(&json!({
                "name": name,
                "type": record.kind,
                "content": record.value.trim_end_matches('.'),
                "ttl": "600",
            })),
        )
        .await?;
    }
    Ok(())
}

// ---- Gandi LiveDNS ---------------------------------------------------------

const GANDI: &str = "https://api.gandi.net/v5/livedns";

async fn gandi(client: &Client, zone: &str, token: &str, records: &[DnsRecord]) -> Result<()> {
    let p = Provider::Gandi;
    let base = format!("{GANDI}/domains/{zone}/records");
    // Record sets are per name and type: clear the routing types we do not
    // publish at each routed name, then replace each of ours whole.
    for name in routed_names(records) {
        for kind in ROUTING {
            if !records.iter().any(|r| r.name == name && r.kind == kind) {
                delete_ok(p, client.delete(format!("{base}/{name}/{kind}")).bearer_auth(token)).await?;
            }
        }
    }
    for (name, kind, values) in grouped(records) {
        let values: Vec<String> = values
            .iter()
            .map(|v| match kind {
                "CNAME" => dotted(v),
                "TXT" => format!("\"{v}\""),
                _ => v.to_string(),
            })
            .collect();
        send(
            p,
            client
                .put(format!("{base}/{name}/{kind}"))
                .bearer_auth(token)
                .json(&json!({ "rrset_values": values, "rrset_ttl": 300 })),
        )
        .await?;
    }
    Ok(())
}

// ---- IONOS (hosting DNS API) ------------------------------------------------

const IONOS: &str = "https://api.hosting.ionos.com/dns/v1";

fn ionos(client: &Client, creds: &Credentials, method: Method, path: &str) -> Result<RequestBuilder> {
    Ok(client.request(method, format!("{IONOS}{path}")).header("X-API-Key", get(creds, "token")?))
}

async fn ionos_apply(client: &Client, zone: &str, creds: &Credentials, records: &[DnsRecord]) -> Result<()> {
    let p = Provider::Ionos;
    let zones = send(p, ionos(client, creds, Method::GET, "/zones")?).await?;
    let zone_id = zones
        .as_array()
        .into_iter()
        .flatten()
        .find(|z| z["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(zone)))
        .and_then(|z| z["id"].as_str())
        .ok_or_else(|| ProError::BadRequest(format!("This IONOS key cannot see the {zone} zone.")))?
        .to_string();
    let detail = send(p, ionos(client, creds, Method::GET, &format!("/zones/{zone_id}"))?).await?;
    let existing: Vec<Existing> = detail["records"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| Existing {
            id: id_string(&r["id"]),
            name: relative(r["name"].as_str().unwrap_or(zone), zone),
            kind: r["type"].as_str().unwrap_or_default().to_string(),
            content: r["content"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    for old in conflicts(&existing, records) {
        delete_ok(p, ionos(client, creds, Method::DELETE, &format!("/zones/{zone_id}/records/{}", old.id))?).await?;
    }
    let new: Vec<Value> = records
        .iter()
        .map(|r| {
            json!({
                "name": fqdn(&r.name, zone),
                "type": r.kind,
                "content": r.value.trim_end_matches('.'),
                "ttl": 3600,
                "disabled": false,
            })
        })
        .collect();
    send(p, ionos(client, creds, Method::POST, &format!("/zones/{zone_id}/records"))?.json(&new)).await?;
    Ok(())
}

// ---- Name.com (Core API v1) -------------------------------------------------

const NAMECOM: &str = "https://api.name.com/core/v1";

fn namecom(client: &Client, creds: &Credentials, method: Method, path: &str) -> Result<RequestBuilder> {
    Ok(client
        .request(method, format!("{NAMECOM}{path}"))
        .basic_auth(get(creds, "user")?, Some(get(creds, "token")?)))
}

async fn namecom_apply(client: &Client, zone: &str, creds: &Credentials, records: &[DnsRecord]) -> Result<()> {
    let p = Provider::NameCom;
    let body = send(p, namecom(client, creds, Method::GET, &format!("/domains/{zone}/records"))?.query(&[("perPage", "1000")])).await?;
    let existing: Vec<Existing> = body["records"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| Existing {
            id: id_string(&r["id"]),
            name: match r["host"].as_str() {
                Some("") | None => "@".into(),
                Some(h) => h.to_string(),
            },
            kind: r["type"].as_str().unwrap_or_default().to_string(),
            content: r["answer"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    for old in conflicts(&existing, records) {
        delete_ok(p, namecom(client, creds, Method::DELETE, &format!("/domains/{zone}/records/{}", old.id))?).await?;
    }
    for record in records {
        let host = if record.name == "@" { "" } else { record.name.as_str() };
        send(
            p,
            namecom(client, creds, Method::POST, &format!("/domains/{zone}/records"))?.json(&json!({
                "host": host,
                "type": record.kind,
                "answer": record.value.trim_end_matches('.'),
                "ttl": 300,
            })),
        )
        .await?;
    }
    Ok(())
}

// ---- Hetzner (Cloud API DNS) ------------------------------------------------

const HETZNER: &str = "https://api.hetzner.cloud/v1";

async fn hetzner(client: &Client, zone: &str, token: &str, records: &[DnsRecord]) -> Result<()> {
    let p = Provider::Hetzner;
    let base = format!("{HETZNER}/zones/{zone}/rrsets");
    // RRSets are per name and type. Clear the routing types we do not publish
    // at each routed name, and each set of ours, then create ours.
    for name in routed_names(records) {
        for kind in ROUTING {
            if !records.iter().any(|r| r.name == name && r.kind == kind) {
                delete_ok(p, client.delete(format!("{base}/{name}/{kind}")).bearer_auth(token)).await?;
            }
        }
    }
    for (name, kind, values) in grouped(records) {
        delete_ok(p, client.delete(format!("{base}/{name}/{kind}")).bearer_auth(token)).await?;
        let values: Vec<Value> = values
            .iter()
            .map(|v| {
                let value = match kind {
                    "CNAME" => dotted(v),
                    "TXT" => format!("\"{v}\""),
                    _ => v.to_string(),
                };
                json!({ "value": value })
            })
            .collect();
        send(
            p,
            client
                .post(&base)
                .bearer_auth(token)
                .json(&json!({ "name": name, "type": kind, "ttl": 300, "records": values })),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &'static str, name: &str, value: &str) -> DnsRecord {
        DnsRecord { kind, name: name.into(), value: value.into(), note: "" }
    }

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn names_resolve_against_the_zone() {
        assert_eq!(fqdn("@", "example.com"), "example.com");
        assert_eq!(fqdn("www", "example.com"), "www.example.com");
        assert_eq!(fqdn("_vuuut-pro-challenge.www", "example.com"), "_vuuut-pro-challenge.www.example.com");
        assert_eq!(relative("example.com.", "example.com"), "@");
        assert_eq!(relative("www.example.com.", "example.com"), "www");
        assert_eq!(relative("www", "example.com"), "www");
    }

    #[test]
    fn every_api_provider_has_fields_and_help() {
        for p in Provider::ALL {
            assert!(supports(p), "{}", p.name());
            assert!(token_help(p).is_some(), "{}", p.name());
            assert!(fields(p).iter().any(|f| f.secret), "{} has a secret", p.name());
        }
    }

    #[test]
    fn conflicts_are_routing_at_our_names_and_old_ownership_only() {
        let e = |id: &str, name: &str, kind: &str, content: &str| Existing {
            id: id.into(), name: name.into(), kind: kind.into(), content: content.into(),
        };
        let old_token = pro_core::ids::domain_verification_token();
        let existing = vec![
            e("1", "www", "A", "1.1.1.1"),
            e("2", "www", "AAAA", "::1"),
            e("3", "@", "A", "2.2.2.2"),
            e("4", "@", "MX", "mail"),
            e("5", "_vuuut-pro-challenge.www", "TXT", &old_token),
            e("6", "www", "TXT", "google-site-verification=x"),
        ];
        let ours = [rec("CNAME", "www", "sites.example.net."), rec("TXT", "_vuuut-pro-challenge.www", "new")];
        let ids: Vec<&str> = conflicts(&existing, &ours).iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["1", "2", "5"]);
    }

    #[test]
    fn credentials_are_checked_field_by_field() {
        let ok = clean(Provider::GoDaddy, &form(&[("key", " k1 "), ("secret", "s1")])).unwrap();
        assert_eq!(ok["key"], "k1");
        assert!(clean(Provider::GoDaddy, &form(&[("key", "k1")])).is_err(), "secret missing");
        assert!(clean(Provider::Cloudflare, &form(&[("token", "has space")])).unwrap_err().to_string().contains("exactly"));
        assert!(clean(Provider::Ionos, &form(&[])).is_err());
    }

    #[test]
    fn a_cpanel_server_must_be_hostgators_own() {
        for good in ["gator1234.hostgator.com", "https://gator1234.hostgator.com:2083/", "box5.websitewelcome.com"] {
            assert!(clean_cpanel_host(good).is_ok(), "{good}");
        }
        for bad in ["hostgator.com", "127.0.0.1", "localhost", "evilhostgator.com", "gator.hostgator.com.evil.com", "10.0.0.1:2083", "a b.hostgator.com"] {
            assert!(clean_cpanel_host(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_hint_shows_only_the_tail() {
        let creds: Credentials = [("token".to_string(), "abcdefgh1234".to_string())].into();
        assert_eq!(hint(Provider::Cloudflare, &creds), "…1234");
        let creds: Credentials = [("user".to_string(), "ada".to_string()), ("key".to_string(), "zzzz9999".to_string())].into();
        assert_eq!(hint(Provider::Namecheap, &creds), "ada · …9999");
    }

    #[test]
    fn provider_slugs_round_trip() {
        for p in Provider::ALL {
            assert_eq!(Provider::from_slug(p.slug()), Some(p));
        }
        assert_eq!(Provider::from_slug("nope"), None);
    }

    #[test]
    fn namecheap_keeps_every_record_it_does_not_replace() {
        let xml = r#"<?xml version="1.0"?><ApiResponse Status="OK" xmlns="http://api.namecheap.com/xml.response"><CommandResponse Type="namecheap.domains.dns.getHosts">
<DomainDNSGetHostsResult Domain="example.com" EmailType="MX" IsUsingOurDNS="true">
<host HostId="1" Name="@" Type="URL" Address="http://www.example.com/" MXPref="10" TTL="1800" />
<host HostId="2" Name="www" Type="CNAME" Address="parkingpage.namecheap.com." MXPref="10" TTL="1800" />
<host HostId="3" Name="@" Type="MX" Address="mail.example.com." MXPref="5" TTL="1800" />
<host HostId="4" Name="@" Type="TXT" Address="v=spf1 include:x ~all" MXPref="10" TTL="1800" />
<host HostId="5" Name="blog" Type="A" Address="1.2.3.4" MXPref="10" TTL="1800" />
</DomainDNSGetHostsResult></CommandResponse></ApiResponse>"#;
        let (hosts, email) = namecheap_hosts(xml).unwrap();
        assert_eq!(email.as_deref(), Some("MX"));
        assert_eq!(hosts.len(), 5);

        let merged = namecheap_merge(hosts, &[rec("CNAME", "www", "sites.example.net.")]);
        let has = |name: &str, kind: &str| merged.iter().any(|h| h.name == name && h.kind == kind);
        assert!(has("@", "MX") && has("@", "TXT") && has("blog", "A") && has("@", "URL"), "unrelated records survive");
        assert_eq!(merged.iter().filter(|h| h.name == "www").count(), 1, "the parking CNAME is replaced");
        assert_eq!(merged.iter().find(|h| h.kind == "MX").unwrap().mx_pref, "5", "MX priority kept");

        let apex = namecheap_merge(merged, &[rec("A", "@", "203.0.113.7")]);
        assert!(!apex.iter().any(|h| h.name == "@" && h.kind == "URL"), "a redirect at the apex gives way");
        assert!(apex.iter().any(|h| h.name == "@" && h.kind == "MX"));
    }

    #[test]
    fn namecheap_refuses_a_domain_on_other_nameservers() {
        let xml = r#"<ApiResponse Status="OK"><CommandResponse><DomainDNSGetHostsResult Domain="x.com" IsUsingOurDNS="false"/></CommandResponse></ApiResponse>"#;
        assert!(namecheap_hosts(xml).unwrap_err().to_string().contains("BasicDNS"));
    }

    #[test]
    fn cpanel_removes_only_what_ours_replaces() {
        let e = |s: &str| STANDARD.encode(s);
        let parsed = json!([
            { "type": "record", "record_type": "SOA", "line_index": 1, "dname_b64": e("example.com."),
              "data_b64": [e("ns1.x."), e("root.x."), e("2026112401"), e("3600")] },
            { "type": "record", "record_type": "A", "line_index": 10, "dname_b64": e("example.com."), "data_b64": [e("1.1.1.1")] },
            { "type": "record", "record_type": "MX", "line_index": 11, "dname_b64": e("example.com."), "data_b64": [e("0"), e("mail")] },
            { "type": "record", "record_type": "CNAME", "line_index": 12, "dname_b64": e("www"), "data_b64": [e("example.com.")] },
            { "type": "comment", "line_index": 13 },
        ]);
        let (serial, remove) = cpanel_plan(&parsed, "example.com", &[rec("A", "@", "203.0.113.7")]).unwrap();
        assert_eq!(serial, "2026112401");
        assert_eq!(remove, vec![10], "only the old apex A goes; MX and www stay");
    }

    #[test]
    fn nameservers_identify_the_new_providers() {
        let cases = [
            ("ns1.digitalocean.com", Provider::DigitalOcean),
            ("curitiba.ns.porkbun.com", Provider::Porkbun),
            ("ns-12-a.gandi.net", Provider::Gandi),
            ("ns1fkx.name.com", Provider::NameCom),
            ("helium.ns.hetzner.de", Provider::Hetzner),
            ("ns1234.hostgator.com", Provider::HostGator),
        ];
        for (ns, want) in cases {
            let found = Provider::ALL.into_iter().find(|p| crate::domain_connect::matches_for_test(*p, ns));
            assert_eq!(found, Some(want), "{ns}");
        }
        assert!(!crate::domain_connect::matches_for_test(Provider::NameCom, "ns1.domainname.com"));
    }
}
