//! `pro-proxy` — the edge.
//!
//! One listener answers for the whole platform: the dashboard, the operator
//! console, every tenant subdomain and every custom domain a customer has
//! attached. The `Host` header is what tells them apart ([`router`]), and a
//! request for a tenant is streamed through to that tenant's loopback port
//! ([`forward`]).
//!
//! The edge is also where hibernation becomes invisible. A request for a
//! sleeping site starts it and waits, so the visitor sees a slow page rather
//! than an error — which is the whole reason the platform can hold far more
//! sites than it has memory to run at once.
//!
//! TLS and certificate issuance live in the `pro` binary alongside the rest of
//! the serving setup, for the same reason Ghost-rust keeps them in its own
//! `serve` module: it is the one place that has to own the listener.
pub mod dns;
pub mod forward;
pub mod router;

pub use forward::{client_ip, client_scheme, Forwarder};
pub use router::{target, Route, Router, Target};
