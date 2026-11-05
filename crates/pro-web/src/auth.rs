//! Who is making this request.
//!
//! Two populations, kept apart at three levels — see `pro_db::sessions` for
//! why one is not enough. This module is the HTTP end of that: it reads the
//! right cookie, resolves it against the right `subject_type`, and loads the
//! right record.
//!
//! There is deliberately **no** function that answers "whoever is signed in".
//! A handler asks for [`account`] or for [`staff`], and the type it gets back
//! is the proof of which. A permissive helper is how a customer ends up inside
//! the operator console.
use axum::http::HeaderMap;
use pro_core::{Account, ProError, Result, Staff, StaffRole};
use pro_db::sessions::{self, Subject};

use crate::state::AppState;

/// Read one cookie out of a `Cookie` header.
///
/// Written out rather than taken from a crate because the parsing is six lines
/// and the one subtlety is worth being explicit about: a browser sends several
/// cookies separated by `; `, and a value may legitimately contain `=`, so
/// only the *first* `=` separates name from value.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((key, value)) = pair.split_once('=') {
            if key.trim() == name {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Build a `Set-Cookie` value.
///
/// `HttpOnly` keeps the token away from JavaScript, so an XSS anywhere on the
/// platform cannot read a session out. `SameSite=Lax` is the deliberate
/// choice over `Strict`: it still blocks the cross-site POST a CSRF attack
/// needs, while letting a customer who follows a link from a confirmation
/// email arrive already signed in — which `Strict` would break.
pub fn set_cookie(name: &str, value: &str, secure: bool, max_age_days: i64) -> String {
    let mut parts = vec![
        format!("{name}={value}"),
        "Path=/".to_string(),
        "HttpOnly".to_string(),
        "SameSite=Lax".to_string(),
        format!("Max-Age={}", max_age_days * 86_400),
    ];
    if secure {
        parts.push("Secure".into());
    }
    parts.join("; ")
}

/// Build a `Set-Cookie` that clears a session.
pub fn clear_cookie(name: &str, secure: bool) -> String {
    let mut parts = vec![
        format!("{name}="),
        "Path=/".to_string(),
        "HttpOnly".to_string(),
        "SameSite=Lax".to_string(),
        "Max-Age=0".to_string(),
    ];
    if secure {
        parts.push("Secure".into());
    }
    parts.join("; ")
}

/// A `Set-Cookie` for something that is not a session.
///
/// Deliberately **not** `HttpOnly`: there is nothing to protect here, and the
/// distinction keeps the session-cookie helper above the only thing that
/// claims that flag. One year, because a colour scheme a visitor picked should
/// still be theirs on their next visit.
pub fn set_plain_cookie(name: &str, value: &str, secure: bool) -> String {
    let mut parts = vec![
        format!("{name}={value}"),
        "Path=/".to_string(),
        "SameSite=Lax".to_string(),
        format!("Max-Age={}", 365 * 86_400),
    ];
    if secure {
        parts.push("Secure".into());
    }
    parts.join("; ")
}

/// Which colour scheme this request should be rendered in.
///
/// Every page is server-rendered with the resolved class already on `<html>`,
/// because there is no script to put it there after the fact — see
/// `crate::view` for why. An unset or unrecognised cookie is light, which is
/// Ghost's own default rather than the operating system's.
pub fn theme(headers: &HeaderMap) -> crate::view::Theme {
    cookie(headers, crate::view::THEME_COOKIE)
        .as_deref()
        .and_then(crate::view::Theme::parse)
        .unwrap_or_default()
}

/// The signed-in customer, or an error.
pub async fn account(state: &AppState, headers: &HeaderMap) -> Result<Account> {
    let name = sessions::account_cookie_name(state.secure_cookies());
    let token = cookie(headers, name)
        .ok_or_else(|| ProError::Unauthorized("Please sign in.".into()))?;
    let id = sessions::resolve(&state.db, Subject::Account, &token).await?;
    let account = pro_db::accounts::get(&state.db, &id).await?;

    // A blocked account's existing sessions must stop working immediately,
    // not when their cookie happens to expire.
    if !account.is_active() {
        return Err(ProError::Forbidden(
            "This account has been suspended. Please contact support.".into(),
        ));
    }
    Ok(account)
}

/// The signed-in operator, or an error.
pub async fn staff(state: &AppState, headers: &HeaderMap) -> Result<Staff> {
    let name = sessions::staff_cookie_name(state.secure_cookies());
    let token = cookie(headers, name)
        .ok_or_else(|| ProError::Unauthorized("Please sign in.".into()))?;
    let id = sessions::resolve(&state.db, Subject::Staff, &token).await?;
    let staff = pro_db::staff::get(&state.db, &id).await?;
    if !staff.is_active() {
        return Err(ProError::Forbidden("This login has been disabled.".into()));
    }
    Ok(staff)
}

/// The signed-in operator, if they hold at least `needed`.
pub async fn staff_with(
    state: &AppState,
    headers: &HeaderMap,
    needed: StaffRole,
) -> Result<Staff> {
    let member = staff(state, headers).await?;
    if !member.can(needed) {
        return Err(ProError::Forbidden(format!(
            "This needs the {} role.",
            needed.label()
        )));
    }
    Ok(member)
}

/// A tenant this customer owns.
///
/// Every customer-facing handler that touches a site goes through here. The
/// ownership check is the authorisation: without it, a site id in a URL is
/// enough to act on somebody else's publication. It reports `404`, not `403`,
/// so a customer cannot learn which ids exist.
pub async fn owned_tenant(
    state: &AppState,
    account: &Account,
    tenant_id: &str,
) -> Result<pro_core::Tenant> {
    let tenant = pro_db::tenants::get(&state.db, tenant_id)
        .await
        .map_err(|_| ProError::NotFound("No such site.".into()))?;
    if tenant.account_id != account.id {
        return Err(ProError::NotFound("No such site.".into()));
    }
    if tenant.status == pro_core::TenantStatus::Deleted {
        return Err(ProError::NotFound("This site has been deleted.".into()));
    }
    Ok(tenant)
}

/// Whether a request came from the same site.
///
/// Checked on every state-changing POST. `SameSite=Lax` on the session cookie
/// already blocks a cross-site form post from carrying the session, so this is
/// the second layer — and it is the one that still holds if a browser's
/// `SameSite` handling is absent or a future cookie change relaxes it.
///
/// `Origin` is preferred because it is simple and always present on a
/// cross-origin POST; `Referer` is the fallback for browsers that omit
/// `Origin` on same-origin requests. A request with *neither* is allowed, and
/// that is a deliberate judgement rather than an oversight: some privacy
/// tooling strips both, and refusing those would lock real customers out of
/// every form on the platform.
pub fn same_origin(headers: &HeaderMap, expected_host: &str) -> bool {
    let expected = pro_core::normalize_host(expected_host);

    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        // A literal "null" origin is what a sandboxed iframe or a redirect
        // from another scheme sends; it is not this platform.
        if origin == "null" {
            return false;
        }
        return host_of(origin).map(|h| h == expected).unwrap_or(false);
    }
    if let Some(referer) = headers.get("referer").and_then(|v| v.to_str().ok()) {
        return host_of(referer).map(|h| h == expected).unwrap_or(false);
    }
    true
}

/// The host out of an absolute URL.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let authority = after_scheme.split('/').next()?;
    Some(pro_core::normalize_host(authority))
}

/// The path the request came from, when that was a page on this platform.
///
/// Only for coming back to where somebody already was — the appearance
/// switcher, which has no session to protect and no state of its own. Never
/// trust it for anything a cross-site page could exploit by setting it.
///
/// `Referrer-Policy: strict-origin-when-cross-origin` (see [`crate::reply`])
/// sends the full URL on a same-origin request and only the bare origin
/// cross-site, so a path arrives here only from one of our own pages. The host
/// is checked regardless, and the fragment and the scheme-relative `//host`
/// form are both dropped, so what comes back is always a path on this site.
pub fn local_path_from_referer(headers: &HeaderMap, expected_host: &str) -> Option<String> {
    let referer = headers.get("referer").and_then(|v| v.to_str().ok())?;
    if host_of(referer)? != pro_core::normalize_host(expected_host) {
        return None;
    }
    // Everything from the authority's first `/` on — the path and the query,
    // with any fragment cut off.
    let after_scheme = referer.split("://").nth(1)?;
    let path = &after_scheme[after_scheme.find('/')?..];
    let path = path.split('#').next()?;
    if path.starts_with('/') && !path.starts_with("//") {
        Some(path.to_string())
    } else {
        None
    }
}

/// Refuse a cross-site POST.
pub fn require_same_origin(headers: &HeaderMap, expected_host: &str) -> Result<()> {
    if same_origin(headers, expected_host) {
        Ok(())
    } else {
        Err(ProError::Forbidden(
            "That request did not come from this site. Please reload and try again.".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn with_cookie(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::COOKIE, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn reads_one_cookie_from_many() {
        let h = with_cookie("a=1; pro_session=abc123; b=2");
        assert_eq!(cookie(&h, "pro_session").as_deref(), Some("abc123"));
        assert_eq!(cookie(&h, "a").as_deref(), Some("1"));
        assert_eq!(cookie(&h, "missing"), None);
        assert_eq!(cookie(&HeaderMap::new(), "pro_session"), None);
    }

    /// A session token is hex, but a cookie value may generally contain `=`
    /// — only the first one separates name from value.
    #[test]
    fn a_value_may_contain_equals() {
        let h = with_cookie("token=abc=def==");
        assert_eq!(cookie(&h, "token").as_deref(), Some("abc=def=="));
    }

    /// The two panels use different cookie names, so a browser signed into
    /// both never presents one where the other is read.
    #[test]
    fn the_two_populations_use_different_cookies() {
        let h = with_cookie("pro_session=customer; pro_staff=operator");
        assert_eq!(cookie(&h, "pro_session").as_deref(), Some("customer"));
        assert_eq!(cookie(&h, "pro_staff").as_deref(), Some("operator"));
    }

    /// HttpOnly keeps a token away from JavaScript, so an XSS cannot read a
    /// session out.
    #[test]
    fn session_cookies_are_httponly_and_lax() {
        let c = set_cookie("pro_session", "tok", true, 30);
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Secure"));
        assert!(c.contains("Path=/"));
        assert!(c.contains("Max-Age=2592000"));
    }

    /// `Secure` cannot be set on plain HTTP, or the browser drops the cookie
    /// and nobody can sign in locally.
    #[test]
    fn no_secure_flag_on_plain_http() {
        let c = set_cookie("pro_session", "tok", false, 30);
        assert!(!c.contains("Secure"));
        assert!(c.contains("HttpOnly"));
    }

    #[test]
    fn clearing_expires_immediately() {
        let c = clear_cookie("pro_session", true);
        assert!(c.contains("Max-Age=0"));
        assert!(c.starts_with("pro_session="));
    }

    /// The CSRF check: a form posted from another site must not act.
    #[test]
    fn rejects_a_cross_site_origin() {
        let mut h = HeaderMap::new();
        h.insert("origin", HeaderValue::from_static("https://evil.example"));
        assert!(!same_origin(&h, "pro.example.com"));
        assert!(require_same_origin(&h, "pro.example.com").is_err());
    }

    #[test]
    fn accepts_a_same_site_origin() {
        let mut h = HeaderMap::new();
        h.insert("origin", HeaderValue::from_static("https://pro.example.com"));
        assert!(same_origin(&h, "pro.example.com"));

        // A port on either side must not change the decision.
        let mut ported = HeaderMap::new();
        ported.insert("origin", HeaderValue::from_static("http://pro.localhost:8080"));
        assert!(same_origin(&ported, "pro.localhost"));
        assert!(same_origin(&ported, "pro.localhost:8080"));
    }

    /// A sandboxed iframe sends `Origin: null`, which is not this platform.
    #[test]
    fn rejects_a_null_origin() {
        let mut h = HeaderMap::new();
        h.insert("origin", HeaderValue::from_static("null"));
        assert!(!same_origin(&h, "pro.example.com"));
    }

    #[test]
    fn falls_back_to_the_referer() {
        let mut same = HeaderMap::new();
        same.insert("referer", HeaderValue::from_static("https://pro.example.com/dashboard"));
        assert!(same_origin(&same, "pro.example.com"));

        let mut other = HeaderMap::new();
        other.insert("referer", HeaderValue::from_static("https://evil.example/page"));
        assert!(!same_origin(&other, "pro.example.com"));
    }

    /// Origin wins over Referer when both are present, since Referer can be
    /// truncated to an origin by a referrer policy.
    #[test]
    fn origin_takes_precedence_over_referer() {
        let mut h = HeaderMap::new();
        h.insert("origin", HeaderValue::from_static("https://evil.example"));
        h.insert("referer", HeaderValue::from_static("https://pro.example.com/x"));
        assert!(!same_origin(&h, "pro.example.com"));
    }

    /// Neither header is allowed through on purpose: some privacy tooling
    /// strips both, and the session cookie's SameSite=Lax is what carries the
    /// protection in that case.
    #[test]
    fn allows_a_request_with_no_origin_information() {
        assert!(same_origin(&HeaderMap::new(), "pro.example.com"));
    }

    #[test]
    fn a_malformed_origin_is_not_trusted() {
        let mut h = HeaderMap::new();
        h.insert("origin", HeaderValue::from_static("not-a-url"));
        assert!(!same_origin(&h, "pro.example.com"));
    }

    /// A site id in a URL must not be enough to act on somebody else's
    /// publication, and the refusal must not reveal that the id exists.
    #[tokio::test]
    async fn ownership_is_enforced_and_does_not_leak() {
        let pool = pro_db::open_in_memory().await.unwrap();
        let mine = pro_db::accounts::create(&pool, "me@example.com", "Me", "h").await.unwrap();
        let theirs = pro_db::accounts::create(&pool, "them@example.com", "Them", "h").await.unwrap();
        let their_site = pro_db::tenants::create(
            &pool, &theirs.id, "theirs", "Theirs", "starter", "2026-01-01T00:00:00Z", 21000, 21010,
        )
        .await
        .unwrap();

        let state = AppState {
            db: pool.clone(),
            config: {
                let mut c: pro_core::Config = serde_json::from_value(serde_json::json!({
                    "url": "http://pro.localhost:8080",
                    "subdomainBase": "localhost",
                    "paths": { "ghostBinary": "/bin/sh" },
                }))
                .unwrap();
                c.plans = pro_core::plans::default_plans();
                std::sync::Arc::new(c)
            },
            settings: {
                let mut c: pro_core::Config = serde_json::from_value(serde_json::json!({
                    "url": "http://pro.localhost:8080",
                    "subdomainBase": "localhost",
                }))
                .unwrap();
                c.plans = pro_core::plans::default_plans();
                std::sync::Arc::new(
                    pro_db::SettingsStore::load(&pool, std::sync::Arc::new(c)).await.unwrap(),
                )
            },
            supervisor: {
                let mut c: pro_core::Config = serde_json::from_value(serde_json::json!({
                    "url": "http://pro.localhost:8080",
                    "subdomainBase": "localhost",
                    "paths": { "ghostBinary": "/bin/sh" },
                }))
                .unwrap();
                c.plans = pro_core::plans::default_plans();
                std::sync::Arc::new(
                    pro_supervisor::Supervisor::new(std::sync::Arc::new(c)).unwrap(),
                )
            },
            hosts: std::sync::Arc::new(pro_proxy::Router::new(
                "pro.localhost".into(),
                "localhost".into(),
            )),
            forwarder: pro_proxy::Forwarder::new(),
        };

        let err = owned_tenant(&state, &mine, &their_site.id).await.unwrap_err();
        assert_eq!(err.status(), 404, "must not be 403, which would confirm it exists");

        // And the owner can reach their own.
        let my_site = pro_db::tenants::create(
            &pool, &mine.id, "mine", "Mine", "starter", "2026-01-01T00:00:00Z", 21000, 21010,
        )
        .await
        .unwrap();
        assert_eq!(owned_tenant(&state, &mine, &my_site.id).await.unwrap().id, my_site.id);

        // A deleted site is gone from the customer's view.
        pro_db::tenants::mark_deleted(&pool, &my_site.id).await.unwrap();
        assert_eq!(owned_tenant(&state, &mine, &my_site.id).await.unwrap_err().status(), 404);
    }
}
