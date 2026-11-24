//! Write a domain's records straight into its DNS host, using an API token the
//! customer pastes once.
//!
//! This is the automatic path that works today, without waiting for a provider
//! to onboard a Domain Connect template. The token is used for this one call
//! and never stored. Only the records Vuuut Pro needs are touched: routing
//! records at the connected name replace whatever routed it before, and the
//! ownership TXT replaces only an earlier Vuuut Pro verification value.
use std::time::Duration;

use pro_core::{ProError, Result};
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde_json::{json, Value};

use crate::{dns::DnsRecord, domain_connect::Provider};

const ROUTING: [&str; 3] = ["A", "AAAA", "CNAME"];

/// Providers whose own API we can write to with a pasted token.
pub fn supports(provider: Provider) -> bool {
    matches!(provider, Provider::Cloudflare | Provider::Hostinger)
}

/// Where the customer creates a token, and what to give it.
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
        _ => None,
    }
}

/// Publish `records` (names relative to `zone`, `@` for the apex).
pub async fn apply(provider: Provider, zone: &str, token: &str, records: &[DnsRecord]) -> Result<()> {
    let token = token.trim();
    if token.is_empty() || token.len() > 400 || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ProError::BadRequest("Paste the API token exactly as your DNS host shows it.".into()));
    }
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| ProError::Unavailable(format!("DNS API client: {e}")))?;
    match provider {
        Provider::Cloudflare => cloudflare(&client, zone, token, records).await,
        Provider::Hostinger => hostinger(&client, zone, token, records).await,
        other => Err(ProError::BadRequest(format!(
            "Automatic setup with a token is not available for {} yet.",
            other.name()
        ))),
    }
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
        return Err(ProError::BadRequest(format!(
            "{} rejected the token. Check it is current and allowed to edit this domain’s DNS.",
            provider.name()
        )));
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
    let mut sets: Vec<(&str, &str, Vec<Value>)> = Vec::new();
    for record in records {
        let content = json!({ "content": record.value });
        match sets.iter_mut().find(|(n, k, _)| *n == record.name && *k == record.kind) {
            Some(set) => set.2.push(content),
            None => sets.push((&record.name, record.kind, vec![content])),
        }
    }
    let zone_body: Vec<Value> = sets
        .into_iter()
        .map(|(name, kind, records)| json!({ "name": name, "type": kind, "ttl": 300, "records": records }))
        .collect();
    send(
        p,
        client.put(&url).bearer_auth(token).json(&json!({ "overwrite": true, "zone": zone_body })),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve_against_the_zone() {
        assert_eq!(fqdn("@", "example.com"), "example.com");
        assert_eq!(fqdn("www", "example.com"), "www.example.com");
        assert_eq!(fqdn("_vuuut-pro-challenge.www", "example.com"), "_vuuut-pro-challenge.www.example.com");
    }

    #[test]
    fn only_api_providers_take_a_token() {
        assert!(supports(Provider::Cloudflare));
        assert!(supports(Provider::Hostinger));
        assert!(!supports(Provider::GoDaddy));
    }

    #[tokio::test]
    async fn a_malformed_token_is_refused_before_any_request() {
        let error = apply(Provider::Cloudflare, "example.com", "has space", &[]).await.unwrap_err();
        assert!(error.to_string().contains("exactly"));
    }
}
