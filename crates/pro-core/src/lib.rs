//! `pro-core` — the vocabulary of the hosting platform.
//!
//! No I/O, no database, no HTTP. Everything here is a type, a rule, or a pure
//! function, which is what lets the interesting decisions be unit-tested
//! without a server: which limits a plan grants, whether a slug may be
//! registered, who may change the payment provider.
//!
//! ## How this platform relates to the CMS
//!
//! Vuuut is a single-site server: one process, one SQLite database, one
//! `url`. It already knows how to be hosted — `ghost_db::limits` reads a
//! `hostSettings` block out of its config and enforces plan ceilings from the
//! inside, exactly as Ghost does on Ghost Pro.
//!
//! So this platform does not wrap the CMS or modify it. It runs one CMS
//! process per tenant and writes that `hostSettings` block. The division:
//!
//! | The platform decides | The site decides |
//! |---|---|
//! | that a site exists, and on what plan | what is published |
//! | which hostnames route to it | its own theme, members, newsletters |
//! | when it starts, stops, is suspended | whether a ceiling has been hit |
//!
//! The control plane never reads a tenant's content or its users. That is not
//! politeness; it is what keeps the blast radius of a bug here small.
//!
//! ## Where configuration comes from
//!
//! [`config::Config`] is the seed — a file and the environment, read once at
//! boot. [`settings::Effective`] is what the platform actually reads, and it
//! layers the operator's console changes on top, so switching payment
//! provider does not mean a restart. Only [`models::StaffRole::Owner`] may
//! change the settings that decide how money is taken.
pub mod checkout;
pub mod config;
pub mod database;
pub mod error;
pub mod ids;
pub mod models;
pub mod plans;
pub mod settings;

pub use config::Config;
pub use database::{Engine, Target as DbTarget, TenantStore};
pub use error::{DbErrorExt, ProError, Result};
pub use models::{
    suspension, Account, AuditEvent, Domain, DomainKind, DomainStatus, Staff, StaffRole,
    Subscription, Tenant, TenantStatus,
};
pub use plans::Plan;
pub use settings::Effective;

/// Now, as the ISO-8601 string every timestamp in the control plane is stored
/// as.
///
/// Text rather than an integer so a database dump is readable and sorts
/// correctly, and always UTC — a platform with tenants in several timezones
/// has no business storing a local one.
pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `now_iso()` shifted by a number of days, for trial ends and deletion grace
/// periods.
pub fn days_from_now(days: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::days(days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Parse one of our own timestamps back. `None` for anything unparseable,
/// which callers treat as "unknown" rather than as an error — a malformed
/// date in one row must not take a page down.
pub fn parse_iso(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|d| d.and_utc())
        })
}

/// Whether `deadline` is in the past. An unparseable or absent deadline has
/// not passed, which is the safe direction for every caller here: it delays a
/// suspension or a deletion rather than performing one early.
pub fn has_passed(deadline: Option<&str>) -> bool {
    match deadline.and_then(parse_iso) {
        Some(t) => t <= chrono::Utc::now(),
        None => false,
    }
}

/// Normalise a hostname for storage and comparison.
///
/// Lowercased, trailing dot and port removed, and `www.` left alone — `www` is
/// a different hostname and whether it should redirect is the customer's
/// choice, not something to silently collapse here.
pub fn normalize_host(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    let h = h.split('/').next().unwrap_or(&h);
    // Strip a port, but not part of an IPv6 literal.
    let h = match h.rsplit_once(':') {
        Some((head, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => head,
        _ => h,
    };
    h.trim_end_matches('.').to_string()
}

/// Whether a string is a plausible email address.
///
/// Deliberately loose. The only reliable test of an address is sending to it,
/// which the platform does anyway on signup — so this rejects what is
/// obviously not an address and lets the verification email settle the rest.
/// A stricter regex here would reject valid addresses, which is the worse
/// error.
pub fn looks_like_email(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 3 || s.len() > 254 || s.contains(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = s.rsplit_once('@') else { return false };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip() {
        let now = now_iso();
        assert!(now.ends_with('Z'));
        assert!(parse_iso(&now).is_some());
        assert!(parse_iso("not a date").is_none());
    }

    #[test]
    fn deadlines_in_the_past_have_passed() {
        assert!(has_passed(Some(&days_from_now(-1))));
        assert!(!has_passed(Some(&days_from_now(1))));
    }

    /// An absent or broken deadline must not trigger a deletion.
    #[test]
    fn an_unknown_deadline_never_passes() {
        assert!(!has_passed(None));
        assert!(!has_passed(Some("")));
        assert!(!has_passed(Some("whenever")));
    }

    #[test]
    fn normalizes_hostnames() {
        assert_eq!(normalize_host("Example.COM."), "example.com");
        assert_eq!(normalize_host(" blog.example.com:8080 "), "blog.example.com");
        assert_eq!(normalize_host("example.com/path"), "example.com");
        // www is a distinct hostname and is not collapsed
        assert_eq!(normalize_host("www.example.com"), "www.example.com");
    }

    #[test]
    fn accepts_plausible_emails() {
        assert!(looks_like_email("a@b.co"));
        assert!(looks_like_email("first.last+tag@sub.example.com"));
        assert!(!looks_like_email("no-at-sign"));
        assert!(!looks_like_email("a@b"), "no dot in the domain");
        assert!(!looks_like_email("a b@c.com"), "whitespace");
        assert!(!looks_like_email("@b.com"), "empty local part");
        assert!(!looks_like_email("a@.com"));
    }
}
