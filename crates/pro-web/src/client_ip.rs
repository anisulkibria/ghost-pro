//! Knowing who is connecting, without it ever being able to fail.
//!
//! `ConnectInfo<SocketAddr>` is axum's way of exposing the peer address, but
//! it is a *fallible* extractor: when the serving layer has not inserted it,
//! a handler that asks for it is rejected with a 500. That is the wrong
//! trade here. The peer address is used for two things — stamping a session
//! row, and resolving the visitor's IP for a tenant — and neither is worth
//! failing a signup over.
//!
//! So this extractor cannot fail. It reports `None` when the address is
//! genuinely unknown, and every caller is written to cope with that.
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

/// The peer address of this connection, if the serving layer recorded one.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientAddr(pub Option<SocketAddr>);

impl ClientAddr {
    pub fn ip(&self) -> Option<IpAddr> {
        self.0.map(|a| a.ip())
    }

    pub fn ip_string(&self) -> Option<String> {
        self.ip().map(|ip| ip.to_string())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ClientAddr {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(ClientAddr(
            parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_the_peer_address_when_present() {
        let mut request = axum::http::Request::builder().body(()).unwrap();
        let addr: SocketAddr = "203.0.113.7:51234".parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(addr));

        let (mut parts, _) = request.into_parts();
        let extracted = ClientAddr::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(extracted.ip().unwrap().to_string(), "203.0.113.7");
        assert_eq!(extracted.ip_string().as_deref(), Some("203.0.113.7"));
    }

    /// The point of the whole module: a missing address is not an error, so a
    /// signup does not fail because the serving layer did not record one.
    #[tokio::test]
    async fn an_absent_address_is_not_a_failure() {
        let request = axum::http::Request::builder().body(()).unwrap();
        let (mut parts, _) = request.into_parts();
        let extracted = ClientAddr::from_request_parts(&mut parts, &()).await.unwrap();
        assert!(extracted.ip().is_none());
        assert!(extracted.ip_string().is_none());
    }
}
