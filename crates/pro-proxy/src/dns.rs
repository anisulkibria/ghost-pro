//! Checking that a custom domain really points here.
//!
//! This is the gate before a certificate is requested and before a byte is
//! served for a hostname. It matters more than it looks: without it, one
//! customer could attach `www.somebody-else.com`, and the platform would both
//! serve their content on it and ask a certificate authority to vouch for it.
//!
//! Two ways to pass, because customers' DNS situations differ:
//!
//! * **Pointing** — an `A`/`AAAA` record at the platform's address, or a
//!   `CNAME` to the platform's hostname. This is what actually has to be true
//!   for traffic to arrive, so it is the real test.
//! * **Ownership** — a `TXT` record carrying the token the platform issued.
//!   This proves control of the zone before the cutover, so a customer can
//!   prepare a migration without taking their live site down first.
//!
//! Both are checked; passing either marks the domain verified, but only
//! *pointing* lets it serve, because a certificate order over HTTP or
//! TLS-ALPN cannot succeed until traffic arrives here anyway.
use pro_core::{normalize_host, Result};
use std::net::IpAddr;

/// What a check found.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Traffic for this hostname will arrive here. Ready for a certificate.
    PointsHere,
    /// The zone is controlled by whoever holds our token, but the records do
    /// not point here yet.
    OwnershipProven,
    /// Resolves, but somewhere else.
    PointsElsewhere { found: String },
    /// No usable records at all.
    NotFound,
}

impl Verdict {
    /// Whether the domain may now serve traffic and hold a certificate.
    pub fn is_servable(&self) -> bool {
        matches!(self, Verdict::PointsHere)
    }

    /// A sentence for the customer's screen.
    pub fn message(&self) -> String {
        match self {
            Verdict::PointsHere => "DNS is pointing here.".into(),
            Verdict::OwnershipProven => {
                "Ownership is confirmed, but DNS is not pointing here yet.".into()
            }
            Verdict::PointsElsewhere { found } => {
                format!("This domain currently points to {found}.")
            }
            Verdict::NotFound => {
                "No DNS records found for this domain yet. Changes can take up to an hour \
                 to propagate."
                    .into()
            }
        }
    }
}

/// What the platform expects a customer's DNS to say.
#[derive(Debug, Clone)]
pub struct Expectation {
    /// Addresses the platform answers on. A domain pointing at any of these
    /// passes.
    pub addresses: Vec<IpAddr>,
    /// The hostname a `CNAME` should target.
    pub cname_target: String,
}

/// Resolve and judge.
pub async fn check(
    domain: &str,
    verification_token: &str,
    expect: &Expectation,
) -> Result<Verdict> {
    let domain = normalize_host(domain);
    let resolver = resolver()?;

    // Ownership first, because it is cheap and its answer is useful even when
    // the pointing check fails.
    let owns = check_txt(&resolver, &domain, verification_token).await;

    // A CNAME to our hostname is the recommended setup for a subdomain: it
    // keeps working when the platform's address changes, which an A record
    // pinned by a customer does not.
    use hickory_resolver::proto::rr::{RData, RecordType};
    if let Ok(response) = resolver.lookup(format!("{domain}."), RecordType::CNAME).await {
        for record in response.answers() {
            if let RData::CNAME(cname) = &record.data {
                let target = normalize_host(&cname.to_utf8());
                if target == normalize_host(&expect.cname_target) {
                    return Ok(Verdict::PointsHere);
                }
            }
        }
    }

    // Then the addresses. Resolving the name follows any CNAME chain, so a
    // CNAME to something that ultimately lands on us also passes here — which
    // is what makes a customer's existing CDN or DNS-level redirect work.
    match resolver.lookup_ip(format!("{domain}.")).await {
        Ok(response) => {
            let found: Vec<IpAddr> = response.iter().collect();
            if found.iter().any(|ip| expect.addresses.contains(ip)) {
                return Ok(Verdict::PointsHere);
            }
            if owns {
                return Ok(Verdict::OwnershipProven);
            }
            if found.is_empty() {
                return Ok(Verdict::NotFound);
            }
            Ok(Verdict::PointsElsewhere {
                found: found.iter().map(|ip| ip.to_string()).collect::<Vec<_>>().join(", "),
            })
        }
        Err(_) if owns => Ok(Verdict::OwnershipProven),
        // No records, or NXDOMAIN. Not an error: a customer who has not
        // finished editing their DNS is the normal case, and this is retried.
        Err(_) => Ok(Verdict::NotFound),
    }
}

/// Is our token published as a TXT record?
///
/// Checked both at the domain itself and at the `_ghost-pro-challenge`
/// subdomain, because a customer who already has TXT records at the apex —
/// SPF, DMARC, another provider's verification — often cannot add another
/// there, and a dedicated name avoids the collision entirely.
async fn check_txt(
    resolver: &hickory_resolver::TokioResolver,
    domain: &str,
    token: &str,
) -> bool {
    use hickory_resolver::proto::rr::{RData, RecordType};
    if token.is_empty() {
        return false;
    }
    for name in [format!("{domain}."), format!("_ghost-pro-challenge.{domain}.")] {
        if let Ok(response) = resolver.lookup(name, RecordType::TXT).await {
            for record in response.answers() {
                if let RData::TXT(txt) = &record.data {
                    // A TXT record is a list of strings; a long value is split
                    // across them and has to be rejoined before comparing.
                    let joined: String = txt
                        .txt_data
                        .iter()
                        .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                        .collect();
                    if joined.trim() == token.trim() {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn resolver() -> Result<hickory_resolver::TokioResolver> {
    // The system resolver, so an operator's own DNS choices — a local
    // forwarder, a split-horizon zone — are honoured rather than bypassed.
    // A platform whose resolver cannot be built is misconfigured in a way
    // worth reporting, not worth silently routing around.
    Ok(hickory_resolver::Resolver::builder_tokio()
        .map_err(|e| {
            pro_core::ProError::Unavailable(format!("could not read DNS configuration: {e}"))
        })?
        .build()
        .map_err(|e| {
            pro_core::ProError::Unavailable(format!("could not start DNS resolver: {e}"))
        })?)
}

/// The instructions shown to a customer attaching a domain.
///
/// Returned as data rather than rendered here so the dashboard can present it
/// as a table, and so this is testable.
pub struct Instructions {
    pub records: Vec<DnsRecord>,
}

pub struct DnsRecord {
    pub kind: &'static str,
    pub name: String,
    pub value: String,
    pub note: &'static str,
}

/// Build the DNS instructions for one domain.
///
/// Whether a customer can use a `CNAME` depends on where the hostname sits: a
/// subdomain can, and should, because it survives the platform changing
/// address. An apex domain usually cannot — `CNAME` at a zone apex is
/// forbidden by the DNS specification and only works where a provider offers a
/// proprietary flattening — so it gets `A` records instead.
pub fn instructions(domain: &str, token: &str, expect: &Expectation) -> Instructions {
    let domain = normalize_host(domain);
    let is_apex = domain.matches('.').count() <= 1;
    let mut records = Vec::new();

    if is_apex {
        for address in &expect.addresses {
            records.push(DnsRecord {
                kind: if address.is_ipv4() { "A" } else { "AAAA" },
                name: "@".into(),
                value: address.to_string(),
                note: "Points your domain at the platform.",
            });
        }
    } else {
        let label = domain.split('.').next().unwrap_or("www").to_string();
        records.push(DnsRecord {
            kind: "CNAME",
            name: label,
            value: format!("{}.", expect.cname_target.trim_end_matches('.')),
            note: "Preferred: keeps working if the platform's address changes.",
        });
    }

    records.push(DnsRecord {
        kind: "TXT",
        name: if is_apex { "_ghost-pro-challenge".into() } else { "_ghost-pro-challenge".into() },
        value: token.to_string(),
        note: "Optional. Proves you own the domain before you switch traffic over.",
    });

    Instructions { records }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect() -> Expectation {
        Expectation {
            addresses: vec!["203.0.113.10".parse().unwrap()],
            cname_target: "sites.example.com".into(),
        }
    }

    #[test]
    fn verdicts_explain_themselves() {
        assert!(Verdict::PointsHere.is_servable());
        assert!(!Verdict::OwnershipProven.is_servable());
        assert!(!Verdict::NotFound.is_servable());
        assert!(!Verdict::PointsElsewhere { found: "1.2.3.4".into() }.is_servable());

        // Every verdict has a sentence worth showing a customer.
        for v in [
            Verdict::PointsHere,
            Verdict::OwnershipProven,
            Verdict::NotFound,
            Verdict::PointsElsewhere { found: "1.2.3.4".into() },
        ] {
            assert!(!v.message().trim().is_empty());
        }
        assert!(Verdict::PointsElsewhere { found: "1.2.3.4".into() }
            .message()
            .contains("1.2.3.4"));
    }

    /// A subdomain gets a CNAME, which survives the platform changing address.
    #[test]
    fn a_subdomain_is_told_to_use_a_cname() {
        let i = instructions("blog.customer.com", "ghost-pro-verify=abc", &expect());
        let cname = i.records.iter().find(|r| r.kind == "CNAME").expect("a CNAME");
        assert_eq!(cname.name, "blog");
        assert_eq!(cname.value, "sites.example.com.");
        assert!(!i.records.iter().any(|r| r.kind == "A"), "no A records for a subdomain");
    }

    /// A CNAME at a zone apex is forbidden by the DNS specification, so an
    /// apex domain must be given A records instead.
    #[test]
    fn an_apex_domain_is_told_to_use_a_records() {
        let i = instructions("customer.com", "ghost-pro-verify=abc", &expect());
        assert!(!i.records.iter().any(|r| r.kind == "CNAME"), "a CNAME at an apex is invalid");
        let a = i.records.iter().find(|r| r.kind == "A").expect("an A record");
        assert_eq!(a.name, "@");
        assert_eq!(a.value, "203.0.113.10");
    }

    #[test]
    fn emits_aaaa_for_an_ipv6_platform() {
        let expectation = Expectation {
            addresses: vec!["2001:db8::1".parse().unwrap()],
            cname_target: "sites.example.com".into(),
        };
        let i = instructions("customer.com", "t", &expectation);
        assert!(i.records.iter().any(|r| r.kind == "AAAA" && r.value == "2001:db8::1"));
    }

    /// The ownership record is offered alongside, so a customer can prove
    /// control before cutting traffic over.
    #[test]
    fn always_offers_the_ownership_record() {
        for domain in ["customer.com", "blog.customer.com"] {
            let i = instructions(domain, "ghost-pro-verify=abc", &expect());
            let txt = i.records.iter().find(|r| r.kind == "TXT").expect("a TXT record");
            assert_eq!(txt.value, "ghost-pro-verify=abc");
            // At a dedicated name, so it cannot collide with SPF or DMARC at
            // the apex.
            assert_eq!(txt.name, "_ghost-pro-challenge");
        }
    }

    /// A hostname with a port or a trailing dot must still be understood.
    #[test]
    fn normalises_the_domain_before_deciding() {
        let i = instructions("Blog.Customer.COM.", "t", &expect());
        let cname = i.records.iter().find(|r| r.kind == "CNAME").unwrap();
        assert_eq!(cname.name, "blog");
    }

    /// A domain that resolves nowhere is the normal state of a customer
    /// mid-setup, and must come back as a verdict rather than an error — the
    /// check is retried on a schedule.
    #[tokio::test]
    async fn an_unresolvable_domain_is_a_verdict_not_an_error() {
        let result =
            check("this-domain-does-not-exist-ghost-pro-test.invalid", "token", &expect()).await;
        let verdict = result.expect("a failed lookup is not an error");
        assert!(!verdict.is_servable());
    }
}
