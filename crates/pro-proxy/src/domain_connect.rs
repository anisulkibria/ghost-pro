//! Discover the active DNS zone and build a signed Domain Connect approval URL.
use std::{net::IpAddr, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use hickory_resolver::proto::rr::{RData, RecordType};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use pro_core::{config::DomainConnectConfig, normalize_host, ProError, Result};
use reqwest::Url;
use serde::Deserialize;

use crate::dns::{self, Expectation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Cloudflare,
    GoDaddy,
    Ionos,
    Namecheap,
    Hostinger,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cloudflare => "Cloudflare",
            Self::GoDaddy => "GoDaddy",
            Self::Ionos => "IONOS",
            Self::Namecheap => "Namecheap",
            Self::Hostinger => "Hostinger",
        }
    }

    pub fn dns_dashboard(self) -> &'static str {
        match self {
            Self::Cloudflare => "https://dash.cloudflare.com/",
            Self::GoDaddy => "https://dcc.godaddy.com/control/portfolio",
            Self::Ionos => "https://my.ionos.com/domains",
            Self::Namecheap => "https://ap.www.namecheap.com/domains/list/",
            Self::Hostinger => "https://hpanel.hostinger.com/domains",
        }
    }

    fn matches(self, nameserver: &str) -> bool {
        let suffixes: &[&str] = match self {
            Self::Cloudflare => &["ns.cloudflare.com", "cloudflare.com", "cloudflare.net"],
            Self::GoDaddy => &["domaincontrol.com"],
            Self::Ionos => &["ui-dns.com", "ui-dns.de", "ui-dns.org", "ui-dns.biz"],
            Self::Namecheap => &["registrar-servers.com", "namecheaphosting.com"],
            Self::Hostinger => &["dns-parking.com", "hostinger.com"],
        };
        suffixes.iter().any(|suffix| {
            nameserver == *suffix
                || nameserver
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
    }

    // DNS records and settings are user-controlled. HTTP requests and browser
    // redirects use only the matched provider's known HTTPS endpoints.
    fn api_prefixes(self) -> &'static [&'static str] {
        match self {
            Self::Cloudflare => &["https://api.cloudflare.com/client/v4/dns/domainconnect"],
            Self::GoDaddy => &["https://domainconnect.api.godaddy.com"],
            Self::Ionos => &[
                "https://api.domainconnect.ionos.com",
                "https://api.domainconnect.1and1.com",
            ],
            Self::Namecheap | Self::Hostinger => &[],
        }
    }

    fn approval_prefixes(self) -> &'static [&'static str] {
        match self {
            Self::Cloudflare => &["https://dash.cloudflare.com/domainconnect"],
            Self::GoDaddy => &[
                "https://domainconnect.godaddy.com",
                "https://domainconnect.gd.godaddy.com",
                "https://dcc.godaddy.com/manage",
            ],
            Self::Ionos => &[
                "https://domainconnect.ionos.com",
                "https://domainconnect.1and1.com",
            ],
            Self::Namecheap | Self::Hostinger => &[],
        }
    }
}

#[derive(Debug)]
pub struct Authority {
    pub zone: String,
    pub nameservers: Vec<String>,
    pub provider: Option<Provider>,
}

fn provider_for(nameservers: &[String]) -> Option<Provider> {
    [
        Provider::Cloudflare,
        Provider::GoDaddy,
        Provider::Ionos,
        Provider::Namecheap,
        Provider::Hostinger,
    ]
    .into_iter()
    .find(|provider| !nameservers.is_empty() && nameservers.iter().all(|ns| provider.matches(ns)))
}

/// Walk up to the registered domain, stopping at the nearest delegated zone.
pub async fn detect(domain: &str) -> Result<Option<Authority>> {
    let domain = normalize_host(domain);
    let Some(apex) = public_domain(&domain) else {
        return Ok(None);
    };
    let resolver = dns::resolver()?;
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut zone = domain.as_str();
        loop {
            if let Ok(response) = resolver.lookup(format!("{zone}."), RecordType::NS).await {
                let mut nameservers: Vec<_> = response
                    .answers()
                    .iter()
                    .filter_map(|record| match &record.data {
                        RData::NS(ns) if normalize_host(&record.name.to_utf8()) == zone => {
                            Some(normalize_host(&ns.to_utf8()))
                        }
                        _ => None,
                    })
                    .collect();
                if !nameservers.is_empty() {
                    nameservers.sort_unstable();
                    nameservers.dedup();
                    let provider = provider_for(&nameservers);
                    return Some(Authority {
                        zone: zone.into(),
                        nameservers,
                        provider,
                    });
                }
            }
            if zone == apex {
                return None;
            }
            zone = zone.split_once('.')?.1;
        }
    })
    .await
    .map_err(|_| ProError::Unavailable("DNS provider detection took too long.".into()))
}

#[derive(Deserialize)]
struct Settings {
    #[serde(rename = "urlAPI")]
    api: String,
    #[serde(rename = "urlSyncUX")]
    approval: String,
}

fn allowed_prefix<'a>(candidate: &str, prefixes: &'a [&str]) -> Option<&'a str> {
    let candidate = candidate.trim_end_matches('/');
    prefixes.iter().copied().find(|prefix| *prefix == candidate)
}

async fn discover_settings(
    client: &reqwest::Client,
    authority: &Authority,
    provider: Provider,
) -> Option<Settings> {
    let resolver = dns::resolver().ok()?;
    let response = resolver
        .lookup(
            format!("_domainconnect.{}.", authority.zone),
            RecordType::TXT,
        )
        .await
        .ok()?;
    for record in response.answers() {
        let RData::TXT(txt) = &record.data else {
            continue;
        };
        let content: Vec<u8> = txt
            .txt_data
            .iter()
            .flat_map(|segment| segment.iter().copied())
            .collect();
        let Ok(content) = std::str::from_utf8(&content) else {
            continue;
        };
        let prefix = format!("https://{}", content.trim());
        let Some(prefix) = allowed_prefix(&prefix, provider.api_prefixes()) else {
            continue;
        };

        let Ok(mut response) = client
            .get(format!("{prefix}/v2/{}/settings", authority.zone))
            .send()
            .await
        else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if bytes.len() + chunk.len() <= 16 * 1024 => {
                    bytes.extend_from_slice(&chunk)
                }
                Ok(None) => {
                    if let Ok(settings) = serde_json::from_slice(&bytes) {
                        return Some(settings);
                    }
                    break;
                }
                _ => break,
            }
        }
    }
    None
}

struct Template<'a> {
    service: &'static str,
    host: &'a str,
    records: Vec<(&'static str, String)>,
}

impl<'a> Template<'a> {
    fn for_domain(domain: &'a str, zone: &str, expect: &Expectation) -> Option<Self> {
        let host = dns::zone_host(domain, zone)?;
        if !host.is_empty() {
            let target = normalize_host(&expect.cname_target);
            if target == domain || public_domain(&target).is_none() {
                return None;
            }
            return Some(Self {
                service: "subdomain",
                host,
                records: vec![("target", format!("{target}."))],
            });
        }

        let ipv4: Vec<_> = expect.addresses.iter().filter(|ip| ip.is_ipv4()).collect();
        let ipv6: Vec<_> = expect.addresses.iter().filter(|ip| ip.is_ipv6()).collect();
        let (service, records) = match (ipv4.as_slice(), ipv6.as_slice()) {
            ([v4], []) if public_address(v4) => ("apex-ipv4", vec![("ipv4", v4.to_string())]),
            ([], [v6]) if public_address(v6) => ("apex-ipv6", vec![("ipv6", v6.to_string())]),
            ([v4], [v6]) if public_address(v4) && public_address(v6) => (
                "apex-dual",
                vec![("ipv4", v4.to_string()), ("ipv6", v6.to_string())],
            ),
            _ => return None,
        };
        Some(Self {
            service,
            host,
            records,
        })
    }
}

fn public_domain(host: &str) -> Option<&str> {
    if host.len() > 253
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        || !psl::suffix(host.as_bytes()).is_some_and(|suffix| suffix.is_known())
    {
        return None;
    }
    psl::domain_str(host)
}

fn public_address(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [first, second, _, _] = v4.octets();
            !v4.is_private()
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_documentation()
                && first != 0
                && first < 224
                && !(first == 100 && (64..=127).contains(&second))
                && !(first == 198 && matches!(second, 18 | 19))
        }
        IpAddr::V6(v6) => {
            let [first, second, ..] = v6.segments();
            (first & 0xe000) == 0x2000 && (first, second) != (0x2001, 0xdb8)
        }
    }
}

pub struct Approval {
    endpoint: String,
    zone: String,
    host: String,
    records: Vec<(&'static str, String)>,
}

impl Approval {
    pub async fn discover(
        domain: &str,
        authority: &Authority,
        expect: &Expectation,
        config: &DomainConnectConfig,
    ) -> Option<Self> {
        let provider = authority.provider?;
        if provider.api_prefixes().is_empty() || !valid_id(&config.provider_id, false) {
            return None;
        }
        let template = Template::for_domain(domain, &authority.zone, expect)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(3))
            .build()
            .ok()?;

        // Apply the deadline to the DNS, settings and capability lookups together.
        let endpoint = tokio::time::timeout(Duration::from_secs(6), async {
            let settings = discover_settings(&client, authority, provider).await?;
            let api = allowed_prefix(&settings.api, provider.api_prefixes())?;
            let web = allowed_prefix(&settings.approval, provider.approval_prefixes())?;
            let path = format!(
                "/v2/domainTemplates/providers/{}/services/{}",
                config.provider_id, template.service
            );
            let response = client.get(format!("{api}{path}")).send().await.ok()?;
            (response.status() == reqwest::StatusCode::OK).then(|| format!("{web}{path}/apply"))
        })
        .await
        .ok()??;

        Some(Self {
            endpoint,
            zone: authority.zone.clone(),
            host: template.host.into(),
            records: template.records,
        })
    }

    pub fn signed_url(
        self,
        redirect_uri: &str,
        ownership_token: &str,
        config: &DomainConnectConfig,
        deployment: &std::path::Path,
    ) -> Result<Url> {
        let error = || {
            ProError::Config(
                "Domain Connect requires a valid PKCS#8 RSA signing key and key ID.".into(),
            )
        };
        if !valid_id(&config.key_id, true) {
            return Err(error());
        }
        let pem = std::fs::read(deployment.join(&config.private_key_file)).map_err(|_| error())?;
        let pem = pem::parse(pem).map_err(|_| error())?;
        if pem.tag() != "PRIVATE KEY" {
            return Err(error());
        }
        let signing_key =
            ring::signature::RsaKeyPair::from_pkcs8(pem.contents()).map_err(|_| error())?;

        // The template supplies the prefix. DNS still receives the full token.
        let ownership = ownership_token
            .strip_prefix(pro_core::ids::DOMAIN_VERIFICATION_PREFIX)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ProError::BadRequest("Invalid DNS verification token.".into()))?;
        let mut fields = self.records;
        fields.extend([
            ("domain", self.zone),
            ("ownership", ownership.into()),
            ("redirect_uri", redirect_uri.into()),
        ]);
        if !self.host.is_empty() {
            fields.push(("host", self.host));
        }
        fields.sort_unstable_by_key(|(name, _)| *name);
        let canonical = fields
            .iter()
            .map(|(name, value)| format!("{name}={}", encode(value)))
            .collect::<Vec<_>>()
            .join("&");

        let mut signature = vec![0; signing_key.public().modulus_len()];
        signing_key
            .sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                canonical.as_bytes(),
                &mut signature,
            )
            .map_err(|_| error())?;
        // Cloudflare requires sig to be the final parameter.
        let query = format!(
            "{canonical}&key={}&sig={}",
            encode(&config.key_id),
            encode(&STANDARD.encode(signature))
        );
        let mut url = Url::parse(&self.endpoint).map_err(ProError::internal)?;
        url.set_query(Some(&query));
        Ok(url)
    }
}

fn valid_id(value: &str, key: bool) -> bool {
    !value.is_empty()
        && value.len() <= if key { 63 } else { 253 }
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || b == b'-' || if key { b == b'_' } else { b == b'.' }
        })
}

fn encode(value: &str) -> String {
    const ENCODE: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(value, ENCODE).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_dns_host_by_nameserver_suffix() {
        for (nameservers, provider) in [
            (
                ["hera.ns.cloudflare.com", "elliott.ns.cloudflare.com"],
                Some(Provider::Cloudflare),
            ),
            (
                ["ns1.dns-parking.com", "ns2.dns-parking.com"],
                Some(Provider::Hostinger),
            ),
            (
                ["dns1.registrar-servers.com", "dns2.registrar-servers.com"],
                Some(Provider::Namecheap),
            ),
            (
                ["ns53.domaincontrol.com", "ns54.domaincontrol.com"],
                Some(Provider::GoDaddy),
            ),
            (["ns1.notcloudflare.com", "ns2.notcloudflare.com"], None),
            (["hera.ns.cloudflare.com", "ns1.dns-parking.com"], None),
        ] {
            assert_eq!(provider_for(&nameservers.map(String::from)), provider);
        }
        assert_eq!(provider_for(&[]), None);
    }

    #[test]
    fn approval_uses_only_the_detected_providers_own_endpoints() {
        let allowed = Provider::Cloudflare.api_prefixes();
        let expected = allowed[0];
        assert_eq!(
            allowed_prefix(&format!("{expected}/"), allowed),
            Some(expected)
        );
        assert_eq!(allowed_prefix(&format!("{expected}/../"), allowed), None);
        assert_eq!(
            allowed_prefix(expected, Provider::GoDaddy.api_prefixes()),
            None
        );
        assert_eq!(
            allowed_prefix(
                "https://api.cloudflare.com.evil.com/client/v4/dns/domainconnect",
                allowed
            ),
            None
        );
    }

    #[test]
    fn chooses_routing_records_for_the_detected_zone() {
        let expect = Expectation {
            addresses: vec![
                "1.1.1.1".parse().unwrap(),
                "2606:4700:4700::1111".parse().unwrap(),
            ],
            cname_target: "sites.example.com".into(),
        };
        let subdomain =
            Template::for_domain("news.blog.example.co.uk", "example.co.uk", &expect).unwrap();
        assert_eq!(subdomain.service, "subdomain");
        assert_eq!(subdomain.host, "news.blog");
        assert_eq!(subdomain.records, [("target", "sites.example.com.".into())]);

        let root =
            Template::for_domain("blog.example.co.uk", "blog.example.co.uk", &expect).unwrap();
        assert_eq!(root.service, "apex-dual");
        assert_eq!(root.host, "");
        assert_eq!(root.records[0].0, "ipv4");
        assert_eq!(root.records[1].0, "ipv6");
        assert_eq!(
            public_domain("news.blog.example.co.uk"),
            Some("example.co.uk")
        );
        assert_eq!(public_domain("sites.localhost"), None);
    }

    #[test]
    fn percent_encodes_apply_values_for_signing() {
        assert_eq!(
            encode("https://example.com/~user?a=b+c"),
            "https%3A%2F%2Fexample.com%2F~user%3Fa%3Db%2Bc"
        );
        assert_eq!(encode("ghost-pro-verify=abcd"), "ghost-pro-verify%3Dabcd");
    }
}
