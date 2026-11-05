//! What every handler needs.
use pro_core::{Config, Effective};
use pro_db::{SettingsStore, SqlitePool};
use pro_mail::Mailer;
use pro_proxy::{Forwarder, Router as HostRouter};
use pro_supervisor::Supervisor;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub config: Arc<Config>,
    pub settings: Arc<SettingsStore>,
    pub supervisor: Arc<Supervisor>,
    pub hosts: Arc<HostRouter>,
    pub forwarder: Forwarder,
}

impl AppState {
    /// The live settings — the config file with the operator's console
    /// changes on top.
    ///
    /// Called per request rather than held, so a setting changed in the
    /// console takes effect on the next request without a restart.
    pub fn live(&self) -> Arc<Effective> {
        self.settings.current()
    }

    /// A mailer built from the current settings.
    ///
    /// Built per send rather than once at boot for the same reason: an
    /// operator who fixes their SMTP details in the console expects the next
    /// email to use them.
    pub fn mailer(&self) -> Mailer {
        let live = self.live();
        Mailer::new(live.mail(), live.platform_name())
    }

    pub fn platform_name(&self) -> String {
        self.live().platform_name()
    }

    /// What every page needs to draw its own frame: the platform's name, and
    /// the colour scheme this visitor asked for.
    ///
    /// Taken together because every error path needs both and neither can be
    /// read from `AppState` alone — the scheme is in a cookie. Resolving it
    /// once, at the top of a handler, is also what keeps it out of the
    /// `Result` plumbing in [`crate::reply`].
    pub fn chrome(&self, headers: &axum::http::HeaderMap) -> Chrome {
        Chrome { platform: self.platform_name(), theme: crate::auth::theme(headers) }
    }

    /// Absolute URL for a path on the platform.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.canonical_url(), path)
    }

    /// Whether cookies may be marked `Secure`.
    ///
    /// Drives the cookie *name* as well as the flag: the `__Host-` prefix is
    /// ignored by browsers unless the cookie is secure, so on plain HTTP the
    /// prefixed name would be set and silently dropped, and nobody could sign
    /// in locally.
    pub fn secure_cookies(&self) -> bool {
        self.config.public_tls()
    }
}

/// The page frame: who is serving this, and in which colour scheme.
///
/// Carried as one value because every error path needs both, and neither can
/// be read from [`AppState`] alone — the colour scheme is in a cookie.
#[derive(Clone)]
pub struct Chrome {
    pub platform: String,
    pub theme: crate::view::Theme,
}

impl Chrome {
    /// For the few places with no request to read a cookie from — a background
    /// task rendering a page, or a test.
    pub fn plain(platform: impl Into<String>) -> Self {
        Self { platform: platform.into(), theme: crate::view::Theme::default() }
    }
}
