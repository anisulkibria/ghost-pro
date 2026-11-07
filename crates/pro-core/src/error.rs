//! One error type for the whole control plane.
//!
//! The variants are chosen by *what the caller should do about it*, not by
//! where the failure came from: a `Conflict` is a 409 and a retry with
//! different input, an `Unavailable` is a 503 and a retry with the same input,
//! and an `Internal` is neither. The HTTP layer maps them in one place, so a
//! new call site cannot invent a new status code for a failure the platform
//! already has a meaning for.
//!
//! `Db` holds a string rather than a `sqlx::Error`, which is what keeps this
//! crate below the database stack instead of beside it — the same trade
//! Vuuut's own `GhostError` makes. Nothing in the control plane branches
//! on a sqlx variant; it logs the text and returns a 500.
use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum ProError {
    /// The request was understood and is not allowed to happen.
    #[error("{0}")]
    BadRequest(String),
    /// No session, or a session that has expired.
    #[error("{0}")]
    Unauthorized(String),
    /// A valid session that may not do this.
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    /// Something about the current state blocks this: a slug already taken, a
    /// domain already attached to another tenant, a tenant already being
    /// provisioned.
    #[error("{0}")]
    Conflict(String),
    /// The plan does not include this, or the account is at a ceiling.
    /// `upgrade_url` is where the customer would go to change that, which is
    /// the only useful thing to offer alongside the refusal.
    #[error("{message}")]
    PlanLimit { message: String, upgrade_url: Option<String> },
    /// A dependency is down — Stripe, the DNS resolver, a tenant process that
    /// will not start. Worth retrying unchanged.
    #[error("{0}")]
    Unavailable(String),
    #[error("configuration: {0}")]
    Config(String),
    #[error("database: {0}")]
    Db(String),
    #[error("{0}")]
    Internal(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl ProError {
    pub fn internal(m: impl fmt::Display) -> Self {
        ProError::Internal(m.to_string())
    }
    pub fn bad_request(m: impl fmt::Display) -> Self {
        ProError::BadRequest(m.to_string())
    }
    pub fn not_found(m: impl fmt::Display) -> Self {
        ProError::NotFound(m.to_string())
    }
    pub fn conflict(m: impl fmt::Display) -> Self {
        ProError::Conflict(m.to_string())
    }
    pub fn unavailable(m: impl fmt::Display) -> Self {
        ProError::Unavailable(m.to_string())
    }
    pub fn config(m: impl fmt::Display) -> Self {
        ProError::Config(m.to_string())
    }
    pub fn forbidden(m: impl fmt::Display) -> Self {
        ProError::Forbidden(m.to_string())
    }
    pub fn unauthorized(m: impl fmt::Display) -> Self {
        ProError::Unauthorized(m.to_string())
    }
    pub fn db(m: impl fmt::Display) -> Self {
        ProError::Db(m.to_string())
    }
    pub fn plan_limit(m: impl fmt::Display) -> Self {
        ProError::PlanLimit { message: m.to_string(), upgrade_url: None }
    }

    /// The HTTP status this failure should be served as.
    pub fn status(&self) -> u16 {
        match self {
            ProError::BadRequest(_) => 400,
            ProError::Unauthorized(_) => 401,
            ProError::Forbidden(_) => 403,
            ProError::NotFound(_) => 404,
            ProError::Conflict(_) => 409,
            // Ghost answers a limit with 403 and a `host-limit` code; the
            // platform matches it, so a client behaves the same way whichever
            // side refused.
            ProError::PlanLimit { .. } => 403,
            ProError::Unavailable(_) => 503,
            _ => 500,
        }
    }

    /// A short machine-readable code, for clients that branch on the reason.
    pub fn code(&self) -> &'static str {
        match self {
            ProError::BadRequest(_) => "bad_request",
            ProError::Unauthorized(_) => "unauthorized",
            ProError::Forbidden(_) => "forbidden",
            ProError::NotFound(_) => "not_found",
            ProError::Conflict(_) => "conflict",
            ProError::PlanLimit { .. } => "host_limit",
            ProError::Unavailable(_) => "unavailable",
            ProError::Config(_) => "config_error",
            _ => "internal_error",
        }
    }

    /// Whether this is worth saying to the public. Internal, database and
    /// config failures are logged in full and reported generically, so a
    /// message shaped like a stack trace or a file path never reaches a
    /// browser.
    pub fn is_public(&self) -> bool {
        !matches!(
            self,
            ProError::Internal(_)
                | ProError::Db(_)
                | ProError::Io(_)
                | ProError::Json(_)
                | ProError::Config(_)
        )
    }

    /// The message to show a user: the real one when it is safe, a generic one
    /// otherwise.
    pub fn public_message(&self) -> String {
        if self.is_public() {
            self.to_string()
        } else {
            "Something went wrong on our side. The team has been notified.".into()
        }
    }
}

/// `?` on a sqlx call site, without this crate knowing what sqlx is.
///
/// Implemented over the error's *display* form so that any error type can be
/// folded into `Db`; the database layer is the only thing that uses it.
pub trait DbErrorExt<T> {
    fn db_err(self) -> Result<T>;
}

impl<T, E: fmt::Display> DbErrorExt<T> for std::result::Result<T, E> {
    fn db_err(self) -> Result<T> {
        self.map_err(|e| ProError::Db(e.to_string()))
    }
}

pub type Result<T> = std::result::Result<T, ProError>;
