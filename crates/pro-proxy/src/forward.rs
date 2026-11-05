//! Forwarding a request to a tenant process.
//!
//! A reverse proxy, with four things it must get right:
//!
//! * **Stream bodies.** A tenant serves images and accepts uploads; buffering
//!   either in the edge would make memory use a function of the largest file
//!   any customer touches.
//! * **Strip hop-by-hop headers.** `Connection`, `Keep-Alive`,
//!   `Transfer-Encoding` and friends describe *this* connection and must not
//!   be relayed onto the next one.
//! * **Set `X-Forwarded-*` authoritatively.** The tenant is behind this proxy
//!   and has no other way to know the visitor's address or that the browser
//!   was on HTTPS. Equally, whatever the client *sent* in those headers must
//!   be discarded, or a visitor can forge their own IP in a site's analytics.
//! * **Wake a sleeping tenant.** Hibernation is invisible only if the first
//!   request waits for the start instead of failing.
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use pro_core::{ProError, Result};

/// Headers that describe a single connection and must not be forwarded.
///
/// `Host` is handled separately — it is rewritten rather than dropped.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers a client must never be able to dictate.
///
/// All of these are set by this proxy from what it actually observed. If an
/// inbound value survived, a visitor could claim any IP address — poisoning a
/// site's analytics and any rate limiting keyed on it — or claim HTTPS on a
/// plain connection.
const CLIENT_CONTROLLED: &[&str] = &[
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-real-ip",
    "forwarded",
];

/// A pooled HTTP client for talking to tenant processes.
#[derive(Clone)]
pub struct Forwarder {
    client: Client<HttpConnector, axum::body::Body>,
}

impl Default for Forwarder {
    fn default() -> Self {
        Self::new()
    }
}

impl Forwarder {
    pub fn new() -> Self {
        let mut connector = HttpConnector::new();
        // Tenants are on loopback, so a connect should be instant; a long
        // timeout here would only delay noticing a dead process.
        connector.set_connect_timeout(Some(std::time::Duration::from_secs(5)));
        connector.set_nodelay(true);

        let client = Client::builder(TokioExecutor::new())
            // Keeping connections alive matters: a site serving a page plus
            // twenty assets would otherwise pay a fresh TCP handshake for each.
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(16)
            .build(connector);

        Self { client }
    }

    /// Send `request` to the tenant on `port` and return its response.
    pub async fn send(
        &self,
        mut request: axum::extract::Request,
        port: u16,
        client_ip: Option<std::net::IpAddr>,
        public_host: &str,
        client_scheme: &str,
    ) -> Result<axum::response::Response> {
        let path_and_query = request
            .uri()
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "/".into());

        let headers = request.headers_mut();

        for name in HOP_BY_HOP.iter().chain(CLIENT_CONTROLLED) {
            headers.remove(*name);
        }

        // The tenant's Ghost builds absolute URLs from its configured `url`,
        // not from this header, so `Host` is mostly informational — but it is
        // what a `Host`-based check inside the site would see, so it carries
        // the hostname the visitor actually used.
        use axum::http::HeaderValue;
        if let Ok(value) = HeaderValue::from_str(public_host) {
            headers.insert(axum::http::header::HOST, value.clone());
            headers.insert("x-forwarded-host", value);
        }
        if let Some(ip) = client_ip {
            if let Ok(value) = HeaderValue::from_str(&ip.to_string()) {
                headers.insert("x-forwarded-for", value.clone());
                headers.insert("x-real-ip", value);
            }
        }
        if let Ok(value) = HeaderValue::from_str(client_scheme) {
            headers.insert("x-forwarded-proto", value);
        }

        *request.uri_mut() = format!("http://127.0.0.1:{port}{path_and_query}")
            .parse()
            .map_err(|e| ProError::internal(format!("could not build tenant uri: {e}")))?;

        let response = self.client.request(request).await.map_err(|e| {
            // A tenant that was up a moment ago and is not now: the sweeper
            // will notice, and the visitor gets a retryable error rather than
            // a stack trace.
            ProError::Unavailable(format!("tenant on port {port} did not respond: {e}"))
        })?;

        let (mut parts, body) = response.into_parts();
        for name in HOP_BY_HOP {
            parts.headers.remove(*name);
        }
        Ok(axum::response::Response::from_parts(
            parts,
            axum::body::Body::new(body),
        ))
    }
}

/// The client's IP, honouring a trusted proxy in front when configured.
///
/// `behind_proxy` is the switch that decides whether `X-Forwarded-For` may be
/// believed. Off, the socket address is the only truth. On, the *left-most*
/// entry is taken, which is the original client as each hop appends itself.
///
/// This is only safe when something really is in front, which is why it is an
/// explicit setting and not a guess: trusting the header unconditionally lets
/// any visitor claim any address.
pub fn client_ip(
    headers: &axum::http::HeaderMap,
    socket_addr: Option<std::net::SocketAddr>,
    behind_proxy: bool,
) -> Option<std::net::IpAddr> {
    if behind_proxy {
        if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = forwarded.split(',').next() {
                if let Ok(ip) = first.trim().parse() {
                    return Some(ip);
                }
            }
        }
    }
    socket_addr.map(|a| a.ip())
}

/// The scheme the *browser* used.
///
/// A tenant needs this to decide whether to mark its cookies secure, and it
/// cannot tell from its own connection, which is always plain HTTP.
pub fn client_scheme(
    headers: &axum::http::HeaderMap,
    edge_is_tls: bool,
    behind_proxy: bool,
) -> &'static str {
    if edge_is_tls {
        return "https";
    }
    if behind_proxy {
        if let Some(proto) = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) {
            // Several hops each append; the first is what the browser used.
            if proto.split(',').next().map(str::trim) == Some("https") {
                return "https";
            }
        }
    }
    "http"
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        use axum::http::HeaderName;
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(HeaderName::try_from(*k).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn socket(ip: &str) -> Option<std::net::SocketAddr> {
        Some(format!("{ip}:12345").parse().unwrap())
    }

    /// Without a proxy in front, the socket is the only truth — a header
    /// claiming otherwise is a forgery attempt.
    #[test]
    fn ignores_forwarded_for_when_not_behind_a_proxy() {
        let h = headers(&[("x-forwarded-for", "1.2.3.4")]);
        let ip = client_ip(&h, socket("10.0.0.9"), false);
        assert_eq!(ip.unwrap().to_string(), "10.0.0.9");
    }

    /// With a proxy in front, the left-most entry is the original client.
    #[test]
    fn trusts_forwarded_for_when_configured() {
        let h = headers(&[("x-forwarded-for", "1.2.3.4, 10.0.0.1, 10.0.0.2")]);
        let ip = client_ip(&h, socket("10.0.0.9"), true);
        assert_eq!(ip.unwrap().to_string(), "1.2.3.4");
    }

    #[test]
    fn falls_back_to_the_socket_when_the_header_is_unusable() {
        let h = headers(&[("x-forwarded-for", "not-an-ip")]);
        assert_eq!(client_ip(&h, socket("10.0.0.9"), true).unwrap().to_string(), "10.0.0.9");
        assert_eq!(client_ip(&HeaderMap::new(), socket("10.0.0.9"), true).unwrap().to_string(), "10.0.0.9");
        assert!(client_ip(&HeaderMap::new(), None, true).is_none());
    }

    #[test]
    fn handles_ipv6() {
        let h = headers(&[("x-forwarded-for", "2001:db8::1")]);
        assert_eq!(client_ip(&h, None, true).unwrap().to_string(), "2001:db8::1");
    }

    /// The tenant always speaks plain HTTP, so the scheme it is told has to
    /// come from the edge — otherwise it emits http:// links and non-secure
    /// cookies on an HTTPS site.
    #[test]
    fn reports_https_when_the_edge_terminates_tls() {
        assert_eq!(client_scheme(&HeaderMap::new(), true, false), "https");
    }

    #[test]
    fn reports_https_from_a_trusted_proxy_header() {
        let h = headers(&[("x-forwarded-proto", "https")]);
        assert_eq!(client_scheme(&h, false, true), "https");
        // Several hops: the first is the browser's.
        let chained = headers(&[("x-forwarded-proto", "https, http")]);
        assert_eq!(client_scheme(&chained, false, true), "https");
    }

    /// A client must not be able to claim HTTPS on a plain connection.
    #[test]
    fn ignores_a_forged_proto_header() {
        let h = headers(&[("x-forwarded-proto", "https")]);
        assert_eq!(client_scheme(&h, false, false), "http");
    }

    #[test]
    fn plain_http_is_plain_http() {
        assert_eq!(client_scheme(&HeaderMap::new(), false, false), "http");
        let h = headers(&[("x-forwarded-proto", "http")]);
        assert_eq!(client_scheme(&h, false, true), "http");
    }

    /// Every header a client could use to lie about itself is in the strip
    /// list. A new one added to the proxy without being stripped would be
    /// forgeable.
    #[test]
    fn every_forwarded_header_we_set_is_also_stripped() {
        for set_by_us in ["x-forwarded-for", "x-real-ip", "x-forwarded-proto", "x-forwarded-host"] {
            assert!(
                CLIENT_CONTROLLED.contains(&set_by_us),
                "{set_by_us} is set by the proxy but not stripped from the request"
            );
        }
    }

    /// Hop-by-hop headers describe one connection and must not be relayed.
    #[test]
    fn strips_the_hop_by_hop_set() {
        for name in ["connection", "keep-alive", "transfer-encoding", "upgrade", "te"] {
            assert!(HOP_BY_HOP.contains(&name), "{name} must be stripped");
        }
        // Host is rewritten rather than dropped, so it must not be in the list.
        assert!(!HOP_BY_HOP.contains(&"host"));
    }
}
