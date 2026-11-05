//! Turning results into responses.
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use pro_core::ProError;

use crate::state::Chrome;
use crate::view::{self, Flash};

/// An HTML page.
pub fn html(body: String) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    harden(response.headers_mut());
    response
}

/// An HTML page with a status other than 200.
pub fn html_status(status: StatusCode, body: String) -> Response {
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    harden(response.headers_mut());
    response
}

/// Headers applied to every page this platform serves.
///
/// The Content-Security-Policy is the one that earns its place. These pages
/// have exactly one stylesheet, inline, and **no scripts at all** — so the
/// policy can be genuinely strict rather than nominally present. With no
/// `script-src` the `default-src 'none'` covers it, which means an injected
/// `<script>` cannot run even if an escaping bug let one through, and turns a
/// cross-site-scripting hole into a cosmetic defect.
///
/// That is also why the design system in [`crate::view`] has no JavaScript in
/// it: this policy blocks an inline `onsubmit` handler as surely as it blocks
/// a script element, so a `confirm()` prompt would simply never appear. Every
/// interaction is a form, a `<details>` or a CSS selector.
///
/// The two Google Fonts hosts are the one exception to "nothing but this
/// document". Material 3's type scale is drawn in Outfit over Fraunces, and
/// `font-src`/`style-src` are the narrowest pair of directives that can carry
/// them. Each face has a local fallback in the stylesheet, so a blocked or
/// offline request costs the typeface and nothing else.
///
/// `frame-ancestors 'none'` is what stops the console being framed by another
/// site and clickjacked — an operator clicking "Suspend" on an invisible
/// overlay is the attack this closes.
fn harden(headers: &mut HeaderMap) {
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; \
             style-src 'unsafe-inline' https://fonts.googleapis.com; \
             font-src https://fonts.gstatic.com; \
             img-src 'self' data:; \
             form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("strict-origin-when-cross-origin"));
}

/// A redirect, as a `303 See Other`.
///
/// `303` rather than `302`, because every redirect here follows a POST: it
/// tells the browser to re-request with `GET`, which is what makes the
/// post-redirect-get pattern work and stops a reload from re-submitting the
/// form.
/// A `303` whose `Location` could not be built is a dead end — the browser
/// stays where it is and the button the visitor pressed looks broken, with
/// nothing in the log to say why. The target can come from outside (a payment
/// provider's checkout url is whatever they hand back), so this is reachable
/// without a bug here, and it is reported rather than swallowed.
pub fn redirect(path: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    match HeaderValue::from_str(path) {
        Ok(value) => {
            response.headers_mut().insert(header::LOCATION, value);
        }
        Err(e) => {
            tracing::error!("cannot redirect to {path:?}: {e}");
            return error_page(
                &Chrome::plain("Ghost Pro"),
                &pro_core::ProError::internal(format!("invalid redirect target: {e}")),
            );
        }
    }
    response
}

/// A redirect carrying a message for the next page.
pub fn redirect_with(path: &str, flash: Flash) -> Response {
    let separator = if path.contains('?') { '&' } else { '?' };
    redirect(&format!("{path}{separator}{}", flash.to_query()))
}

/// A redirect that also sets or clears a cookie.
pub fn redirect_with_cookie(path: &str, cookie: &str) -> Response {
    let mut response = redirect(path);
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// JSON, for the few machine-facing endpoints.
pub fn json(status: StatusCode, body: serde_json::Value) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    response
}

/// A file download, streamed.
///
/// Streams rather than reading into memory because the thing on offer here is
/// a site archive, and a site's uploads can be far larger than the memory a
/// control plane should commit to one click. `nosniff` so a browser cannot
/// be talked into executing it, `attachment` so it cannot be served as a page
/// from the customer's own origin.
pub async fn download(filename: &str, file: tokio::fs::File) -> Response {
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut response = Response::new(axum::body::Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        filename.replace(['"', '\\', '\r', '\n'], "")
    )) {
        response.headers_mut().insert("content-disposition", value);
    }
    response
        .headers_mut()
        .insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    response
}

/// An error as JSON, in the shape Ghost's own API uses.
pub fn json_error(error: &ProError) -> Response {
    log_if_internal(error);
    json(
        StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        serde_json::json!({
            "errors": [{
                "message": error.public_message(),
                "type": error.code(),
            }]
        }),
    )
}

/// An error as a page.
///
/// Internal failures are logged in full here and reported generically to the
/// browser, so a database error's text — which can carry a query, a path, or a
/// column name — never reaches a customer.
///
/// Drawn on the same centred, chrome-free surface as the sign-in screens:
/// whatever failed, the rail's navigation would be either unreachable (a
/// signed-out visitor) or beside the point.
pub fn error_page(chrome: &Chrome, error: &ProError) -> Response {
    log_if_internal(error);
    let status =
        StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let heading = match error.status() {
        401 => "Please sign in",
        403 => "Not allowed",
        404 => "Not found",
        409 => "That did not work",
        503 => "Temporarily unavailable",
        _ => "Something went wrong",
    };

    // A plan limit is the one error with a way out of it, so it offers one.
    let card = match error {
        ProError::PlanLimit { upgrade_url: Some(url), .. } => format!(
            r#"<a class="btn block" href="{}">See plans</a>
<p class="hint" style="text-align:center">Or <a href="/">go back</a>.</p>"#,
            view::attr(url)
        ),
        _ => r#"<a class="btn tonal block" href="/">Back to the start</a>"#.to_string(),
    };

    html_status(
        status,
        view::auth_page(
            heading,
            &chrome.platform,
            chrome.theme,
            heading,
            &error.public_message(),
            None,
            &card,
            "",
        ),
    )
}

/// Log the failures a user is not told the detail of.
///
/// The asymmetry is the point: what the browser sees is generic, and what the
/// operator's log sees is everything. Without this, a 500 would be
/// undiagnosable.
fn log_if_internal(error: &ProError) {
    if !error.is_public() {
        tracing::error!("{error}");
    } else {
        tracing::debug!("{error}");
    }
}

/// A `Result` handler's tail: render the value, or an error page.
pub fn or_error_page(chrome: &Chrome, result: pro_core::Result<Response>) -> Response {
    match result {
        Ok(response) => response,
        Err(e) => error_page(chrome, &e),
    }
}

/// A signed-in-only page's tail: like [`or_error_page`], but a visitor with
/// no session is sent to `signin` rather than shown a 401.
///
/// A 401 page that says "Please sign in" and offers only a Back link is a
/// dead end: it tells the visitor what to do without offering anywhere to
/// do it. Nobody bookmarks the sign-in form; they bookmark (or are emailed)
/// the page behind it, so that page is where the detour has to start. A
/// redirect also means the sign-in form can send them onward with a `next`
/// parameter later, which an error page never could.
pub fn or_page_or_signin(
    signin: &str,
    chrome: &Chrome,
    result: pro_core::Result<Response>,
) -> Response {
    match result {
        Ok(response) => response,
        Err(ProError::Unauthorized(_)) => redirect(signin),
        Err(e) => error_page(chrome, &e),
    }
}

/// A `Result` handler's tail for a form POST: redirect back with the message.
///
/// A failed form submission belongs back on the form with an explanation, not
/// on an error page — the customer has something to correct, and an error page
/// loses what they typed and where they were.
pub fn or_redirect(back: &str, chrome: &Chrome, result: pro_core::Result<Response>) -> Response {
    match result {
        Ok(response) => response,
        Err(e) if e.is_public() => redirect_with(back, Flash::err(e.public_message())),
        Err(e) => {
            log_if_internal(&e);
            error_page(chrome, &e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chrome() -> Chrome {
        Chrome::plain("Ghost Pro")
    }

    #[test]
    fn pages_are_hardened() {
        let response = html("<p>hi</p>".into());
        let headers = response.headers();
        let csp = headers.get("content-security-policy").unwrap().to_str().unwrap();
        // An injected script must not be able to run even if escaping failed.
        // There is no `script-src`, so `default-src 'none'` is what covers it —
        // and the design system has no script for it to have to allow.
        assert!(csp.contains("default-src 'none'"));
        assert!(!csp.contains("script-src"), "nothing may relax the script rule: {csp}");
        // Clickjacking the console's Suspend button is the attack this closes.
        assert!(csp.contains("frame-ancestors 'none'"));
        // Material 3's type scale is the one thing fetched from elsewhere, and
        // only as a stylesheet and its fonts.
        assert!(csp.contains("https://fonts.googleapis.com"));
        assert!(csp.contains("font-src https://fonts.gstatic.com"));
        assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        assert!(headers.get("referrer-policy").is_some());
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
    }

    /// 303 is what stops a reload re-submitting a form.
    #[test]
    fn redirects_are_see_other() {
        let response = redirect("/dashboard");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/dashboard");
    }

    #[test]
    fn flashes_attach_to_a_redirect() {
        let response = redirect_with("/dashboard", Flash::ok("Site created"));
        let location = response.headers().get(header::LOCATION).unwrap().to_str().unwrap();
        assert!(location.starts_with("/dashboard?ok="));

        // An existing query string must be preserved.
        let with_query = redirect_with("/dashboard?tab=sites", Flash::err("No"));
        let location = with_query.headers().get(header::LOCATION).unwrap().to_str().unwrap();
        assert!(location.contains("tab=sites&err="), "got {location}");
    }

    /// A header value cannot contain a newline; a hostile flash must not be
    /// able to inject one and split the response.
    #[test]
    fn a_hostile_flash_cannot_split_the_response() {
        let response = redirect_with("/x", Flash::err("a\r\nSet-Cookie: admin=1"));
        let location = response.headers().get(header::LOCATION).unwrap().to_str().unwrap();
        assert!(!location.contains('\n'));
        assert!(!location.contains('\r'));
    }

    /// Internal detail must never reach a browser.
    #[test]
    fn internal_errors_are_reported_generically() {
        let error = ProError::Db("no such column: secret_internal_name".into());
        let response = error_page(&chrome(), &error);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!error.public_message().contains("secret_internal_name"));
    }

    /// A message meant for the user is shown as-is.
    #[test]
    fn public_errors_keep_their_message() {
        let error = ProError::Conflict("“alice” is taken.".into());
        assert_eq!(error.public_message(), "“alice” is taken.");
        let response = error_page(&chrome(), &error);
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn json_errors_use_ghosts_shape() {
        let response = json_error(&ProError::NotFound("no site".into()));
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// A plan limit offers the way out of it.
    #[test]
    fn a_plan_limit_links_to_the_plans() {
        let error = ProError::PlanLimit {
            message: "Your plan includes 1 custom domain.".into(),
            upgrade_url: Some("/dashboard/billing".into()),
        };
        let response = error_page(&chrome(), &error);
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(error.code(), "host_limit");
    }

    /// A failed form goes back to the form with an explanation, not to an
    /// error page that loses what the customer typed.
    #[test]
    fn a_failed_form_returns_to_the_form() {
        let result = Err(ProError::BadRequest("Site address is too short.".into()));
        let response = or_redirect("/dashboard/new", &chrome(), result);
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers().get(header::LOCATION).unwrap().to_str().unwrap();
        assert!(location.starts_with("/dashboard/new?err="));
    }

    /// An internal failure on a form is not a message to put in a URL.
    #[test]
    fn an_internal_form_failure_shows_a_page() {
        let result = Err(ProError::Db("constraint x".into()));
        let response = or_redirect("/dashboard/new", &chrome(), result);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// A signed-out visitor to a signed-in-only page is detoured to the
    /// sign-in form, not parked on a page whose only exit is Back.
    #[test]
    fn a_signed_out_visitor_is_sent_to_the_signin_form() {
        let result = Err(ProError::Unauthorized("Please sign in.".into()));
        let response = or_page_or_signin("/console/signin", &chrome(), result);
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/console/signin"
        );
    }

    /// Anything else a page can fail with still renders as a page.
    #[test]
    fn other_page_failures_still_render() {
        let result = Err(ProError::Forbidden("This needs the Owner role.".into()));
        let response = or_page_or_signin("/console/signin", &chrome(), result);
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
