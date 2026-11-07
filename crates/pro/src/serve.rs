//! Binding and serving.
//!
//! Mirrors Vuuut's own `serve` module, for the same reason it exists
//! there: this is the one place that has to own the listener, negotiate TLS
//! and hand connections to hyper, and keeping it separate stops that detail
//! leaking into the application.
//!
//! The difference is the certificate set. Vuuut serves one site and knows
//! its hostnames at boot. This serves every tenant subdomain and every custom
//! domain a customer attaches, and that set changes while the process runs —
//! so the ACME state is rebuilt when a domain goes live rather than fixed at
//! startup.
use futures::StreamExt;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use std::io;
use std::path::Path;
use std::sync::Arc;
use tower::Service;

/// Protocols offered over ALPN. HTTP/2 first, which is what lets a browser
/// negotiate it.
const ALPN: &[&[u8]] = &[b"h2", b"http/1.1"];

/// Anything that can serve an axum request — the application, in practice.
pub trait AppService:
    Service<
            axum::http::Request<axum::body::Body>,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
    + Send
    + 'static
{
}

impl<T> AppService for T where
    T: Service<
            axum::http::Request<axum::body::Body>,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static
{
}

/// Install the cryptographic provider rustls needs.
///
/// Idempotent: rustls refuses a second install, and that is not an error here
/// — two call paths can reasonably both want to be sure.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Serve plain HTTP.
///
/// The right choice for local use, and behind a proxy that has already
/// terminated TLS.
pub async fn serve_plain<S>(bind: &str, app: S) -> io::Result<()>
where
    S: AppService,
    S::Future: Send,
{
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("listening on http://{bind}");
    loop {
        let (stream, peer) = listener.accept().await?;
        tokio::spawn(serve_connection(stream, app.clone(), Some(peer)));
    }
}

/// Serve HTTPS with certificates from Let's Encrypt.
///
/// `domains` is every hostname the platform answers for right now: its own,
/// plus every live tenant domain. `reload` is signalled when that set changes
/// — a custom domain passing its DNS check — and the ACME state is rebuilt
/// around the new set.
///
/// Rebuilding rather than mutating is the honest option: `rustls-acme` takes
/// its domain list at construction. Certificates are cached on disk, so a
/// rebuild re-reads what it already has and only orders what is genuinely new
/// — which is why this is cheap enough to do on a domain being added.
pub async fn serve_acme<S>(
    bind: &str,
    app: S,
    initial_domains: Vec<String>,
    email: Option<String>,
    staging: bool,
    cache_dir: &Path,
    reload: Arc<tokio::sync::Notify>,
    current_domains: impl Fn() -> Vec<String> + Send + Sync + 'static,
) -> io::Result<()>
where
    S: AppService,
    S::Future: Send,
{
    use rustls_acme::caches::DirCache;
    use rustls_acme::AcmeConfig;

    install_crypto_provider();
    tokio::fs::create_dir_all(cache_dir).await?;

    if staging {
        tracing::warn!(
            "ssl.staging is on: certificates come from Let's Encrypt's test CA and browsers \
             will not trust them. Remove it once the setup works."
        );
    }

    // Bound once, for the whole run. Each ACME generation gets its own
    // duplicated descriptor, so rebuilding the certificate set never leaves
    // the port unbound — a window in which a request would be refused.
    let std_listener = std::net::TcpListener::bind(bind)?;
    std_listener.set_nonblocking(true)?;
    tracing::info!("listening on https://{bind}");
    let mut domains = initial_domains;

    loop {
        tracing::info!(
            "requesting certificates for {} hostname(s){}",
            domains.len(),
            if staging { " from the staging CA" } else { "" }
        );

        let mut acme = AcmeConfig::new(domains.clone())
            .cache(DirCache::new(cache_dir.to_path_buf()))
            .directory_lets_encrypt(!staging);
        if let Some(address) = &email {
            acme = acme.contact_push(format!("mailto:{address}"));
        }

        // `tokio_incoming` drives the whole certificate lifecycle — the
        // initial order, the TLS-ALPN-01 challenge and renewal weeks later —
        // off the same stream that yields accepted connections.
        // `TcpListenerStream` takes ownership of the listener, so each
        // generation is handed a dup of the descriptor rather than the
        // original.
        let generation = tokio::net::TcpListener::from_std(std_listener.try_clone()?)?;
        let mut incoming = acme.state().tokio_incoming(
            tokio_stream::wrappers::TcpListenerStream::new(generation),
            ALPN.iter().map(|p| p.to_vec()).collect(),
        );

        // Serve until the domain set changes.
        loop {
            tokio::select! {
                connection = incoming.next() => match connection {
                    Some(Ok(stream)) => {
                        let peer = stream.get_ref().get_ref().0.get_ref().peer_addr().ok();
                        tokio::spawn(serve_connection(stream, app.clone(), peer));
                    }
                    Some(Err(e)) => tracing::debug!("TLS connection failed: {e}"),
                    None => return Ok(()),
                },
                _ = reload.notified() => {
                    let next = current_domains();
                    if next != domains {
                        tracing::info!(
                            "certificate hostnames changed ({} → {}); reloading",
                            domains.len(),
                            next.len()
                        );
                        domains = next;
                        break;
                    }
                }
            }
        }
    }
}

/// Serve one accepted connection, letting hyper negotiate HTTP/1.1 or HTTP/2
/// from whatever ALPN settled on.
async fn serve_connection<I, S>(stream: I, app: S, peer: Option<std::net::SocketAddr>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    S: AppService,
    S::Future: Send,
{
    let service = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
        // The peer address, which `pro_web::client_ip` reads back out. Without
        // this a session row has no address and a tenant gets no
        // `X-Forwarded-For`.
        if let Some(peer) = peer {
            req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
        }
        app.clone().call(req.map(axum::body::Body::new))
    });

    if let Err(e) = ConnBuilder::new(TokioExecutor::new())
        // Upgrades keep websockets working through the proxy, which Ghost's
        // admin uses for live previews.
        .serve_connection_with_upgrades(TokioIo::new(stream), service)
        .await
    {
        tracing::debug!("connection error: {e}");
    }
}

/// A plain-HTTP listener that redirects to HTTPS.
///
/// Also the port Let's Encrypt's HTTP-01 challenge would arrive on, which is
/// why the challenge path is forwarded to the application rather than
/// redirected: a redirect would fail the challenge.
pub async fn serve_http_redirect<S>(port: u16, app: S) -> io::Result<()>
where
    S: AppService,
    S::Future: Send,
{
    let bind = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("redirecting http://{bind} to https");

    loop {
        let (stream, peer) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            let service =
                hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let app = app.clone();
                    async move {
                        let path = req.uri().path();
                        // The ACME challenge must be answered over plain HTTP.
                        if path.starts_with("/.well-known/acme-challenge/") {
                            let mut req = req.map(axum::body::Body::new);
                            req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
                            return app.clone().call(req).await;
                        }

                        let host = req
                            .headers()
                            .get(axum::http::header::HOST)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .split(':')
                            .next()
                            .unwrap_or("")
                            .to_string();
                        let target = format!(
                            "https://{host}{}",
                            req.uri()
                                .path_and_query()
                                .map(|p| p.as_str())
                                .unwrap_or("/")
                        );

                        let mut response = axum::response::Response::builder()
                            .status(axum::http::StatusCode::MOVED_PERMANENTLY);
                        if let Ok(value) = axum::http::HeaderValue::from_str(&target) {
                            response = response.header(axum::http::header::LOCATION, value);
                        }
                        Ok(response
                            .body(axum::body::Body::empty())
                            .unwrap_or_default())
                    }
                });

            if let Err(e) = ConnBuilder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!("redirect connection error: {e}");
            }
        });
    }
}
