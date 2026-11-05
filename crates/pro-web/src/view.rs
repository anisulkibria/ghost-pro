//! HTML, written as functions — in the Material 3 language the Ghost CMS port
//! at `Ghost-rust` uses for its admin.
//!
//! ## Why functions and not templates
//!
//! No template engine. Plain functions returning `String` buy two things worth
//! more than template syntax here: every value that reaches the page passes
//! through [`esc`] on its way, visibly, at the call site; and a missing or
//! renamed field is a compile error rather than a blank space in a rendered
//! page.
//!
//! The one rule that must never be broken: **every** interpolation of data
//! that did not come from this file goes through [`esc`], or through [`attr`]
//! inside an attribute. A customer controls their site's title, their name,
//! their domain; an operator reading the console must not be able to be
//! attacked by a customer's choice of site title.
//!
//! ## Why there is no JavaScript
//!
//! Every page this crate serves is sent with `default-src 'none'` (see
//! [`crate::reply`]), which blocks inline event handlers as surely as it
//! blocks a `<script>` element. That is a security property worth keeping, so
//! the design system is built to need no script at all:
//!
//! * the appearance switcher is three one-button forms posting to
//!   `/appearance`, and the chosen scheme is rendered into the `<html>` class
//!   server-side — `system` resolves in CSS, through a media query;
//! * a destructive action's confirmation is a `<details>` popover holding the
//!   real submit button, so nothing happens on the first click;
//! * menus, tabs and switches are `<details>`, `:checked` and `:has()`.
//!
//! The visual language — the tonal palette, 999px buttons, 24px cards, 40px
//! tinted leading icons, the outlined floating-label field, the nav rail — is
//! the one in `crates/ghost-admin-ui/src/lib.rs`, so moving between the
//! platform dashboard and a site's own admin does not feel like two products.

// ---------------------------------------------------------------------------
// Escaping
// ---------------------------------------------------------------------------

/// Escape text for HTML body content.
///
/// All five of these matter. `<` and `>` end and open tags; `&` would
/// otherwise let a customer write their own entities; and both quote
/// characters escape an attribute value, which is why this is also safe to use
/// inside one.
pub fn esc(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape for an attribute value. Same rules; named separately so a reader can
/// see which context a call site is in.
pub fn attr(input: &str) -> String {
    esc(input)
}

/// `checked`/`selected`-style attribute, or nothing.
pub fn flag(name: &str, on: bool) -> String {
    if on {
        format!(" {name}")
    } else {
        String::new()
    }
}

/// Percent-encode for a query string value.
pub fn urlencode(input: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(input, NON_ALPHANUMERIC).to_string()
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/// Inline 24dp Material Design icons (outlined style, strokeless fill paths).
///
/// Inline rather than a sprite or a font: the pages carry one stylesheet and
/// no other sub-resource, which is what lets the Content-Security-Policy stay
/// at `default-src 'none'`.
fn icon(name: &str) -> &'static str {
    match name {
        "sites" => r#"<path d="M4 6h16v2H4V6zm0 5h16v2H4v-2zm0 5h10v2H4v-2z"/><path d="M19 3H5c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h14c1.1 0 2-.9 2-2V5c0-1.1-.9-2-2-2zm0 16H5V5h14v14z"/>"#,
        "globe" => r#"<path d="M11.99 2C6.47 2 2 6.48 2 12s4.47 10 9.99 10C17.52 22 22 17.52 22 12S17.52 2 11.99 2zm6.93 6h-2.95c-.32-1.25-.78-2.45-1.38-3.56 1.84.63 3.37 1.91 4.33 3.56zM12 4.04c.83 1.2 1.48 2.53 1.91 3.96h-3.82c.43-1.43 1.08-2.76 1.91-3.96zM4.26 14C4.1 13.36 4 12.69 4 12s.1-1.36.26-2h3.38c-.08.66-.14 1.32-.14 2 0 .68.06 1.34.14 2H4.26zm.82 2h2.95c.32 1.25.78 2.45 1.38 3.56-1.84-.63-3.37-1.9-4.33-3.56zm2.95-8H5.08c.96-1.66 2.49-2.93 4.33-3.56C8.81 5.55 8.35 6.75 8.03 8zM12 19.96c-.83-1.2-1.48-2.53-1.91-3.96h3.82c-.43 1.43-1.08 2.76-1.91 3.96zM14.34 14H9.66c-.09-.66-.16-1.32-.16-2 0-.68.07-1.35.16-2h4.68c.09.65.16 1.32.16 2 0 .68-.07 1.34-.16 2zm.25 5.56c.6-1.11 1.06-2.31 1.38-3.56h2.95c-.96 1.65-2.49 2.93-4.33 3.56zM16.36 14c.08-.66.14-1.32.14-2 0-.68-.06-1.34-.14-2h3.38c.16.64.26 1.31.26 2s-.1 1.36-.26 2h-3.38z"/>"#,
        "billing" => r#"<path d="M20 4H4c-1.11 0-2 .89-2 2v12c0 1.11.89 2 2 2h16c1.11 0 2-.89 2-2V6c0-1.11-.89-2-2-2zm0 14H4v-6h16v6zm0-10H4V6h16v2z"/>"#,
        "person" => r#"<path d="M12 12c2.21 0 4-1.79 4-4s-1.79-4-4-4-4 1.79-4 4 1.79 4 4 4zm0 2c-2.67 0-8 1.34-8 4v2h16v-2c0-2.66-5.33-4-8-4z"/>"#,
        "chart" => r#"<path d="M5 9.2h3V19H5V9.2zm5.6-4.2h2.8v14h-2.8V5zm5.6 8H19v6h-2.8v-6z"/>"#,
        "people" => r#"<path d="M16 11c1.66 0 2.99-1.34 2.99-3S17.66 5 16 5c-1.66 0-3 1.34-3 3s1.34 3 3 3zm-8 0c1.66 0 2.99-1.34 2.99-3S9.66 5 8 5C6.34 5 5 6.34 5 8s1.34 3 3 3zm0 2c-2.33 0-7 1.17-7 3.5V19h14v-2.5c0-2.33-4.67-3.5-7-3.5zm8 0c-.29 0-.62.02-.97.05 1.16.84 1.97 1.97 1.97 3.45V19h6v-2.5c0-2.33-4.67-3.5-7-3.5z"/>"#,
        "settings" => r#"<path d="M19.14 12.94c.04-.3.06-.61.06-.94 0-.32-.02-.64-.07-.94l2.03-1.58c.18-.14.23-.41.12-.61l-1.92-3.32c-.12-.22-.37-.29-.59-.22l-2.39.96c-.5-.38-1.03-.7-1.62-.94l-.36-2.54c-.04-.24-.24-.41-.48-.41h-3.84c-.24 0-.43.17-.47.41l-.36 2.54c-.59.24-1.13.57-1.62.94l-2.39-.96c-.22-.08-.47 0-.59.22L2.74 8.87c-.12.21-.08.47.12.61l2.03 1.58c-.05.3-.09.63-.09.94s.02.64.07.94l-2.03 1.58c-.18.14-.23.41-.12.61l1.92 3.32c.12.22.37.29.59.22l2.39-.96c.5.38 1.03.7 1.62.94l.36 2.54c.05.24.24.41.48.41h3.84c.24 0 .44-.17.47-.41l.36-2.54c.59-.24 1.13-.56 1.62-.94l2.39.96c.22.08.47 0 .59-.22l1.92-3.32c.12-.22.07-.47-.12-.61l-2.01-1.58zM12 15.6c-1.98 0-3.6-1.62-3.6-3.6s1.62-3.6 3.6-3.6 3.6 1.62 3.6 3.6-1.62 3.6-3.6 3.6z"/>"#,
        "shield" => r#"<path d="M12 1 3 5v6c0 5.55 3.84 10.74 9 12 5.16-1.26 9-6.45 9-12V5l-9-4zm0 10.99h7c-.53 4.12-3.28 7.79-7 8.94V12H5V6.3l7-3.11v8.8z"/>"#,
        "history" => r#"<path d="M13 3a9 9 0 0 0-9 9H1l3.89 3.89.07.14L8.99 12H6c0-3.87 3.13-7 7-7s7 3.13 7 7-3.13 7-7 7c-1.93 0-3.68-.79-4.94-2.06l-1.42 1.42A8.954 8.954 0 0 0 13 21a9 9 0 0 0 0-18zm-1 5v5l4.28 2.54.72-1.21-3.5-2.08V8H12z"/>"#,
        "add" => r#"<path d="M19 13h-6v6h-2v-6H5v-2h6V5h2v6h6v2z"/>"#,
        "open" => r#"<path d="M19 19H5V5h7V3H5c-1.11 0-2 .9-2 2v14c0 1.1.89 2 2 2h14c1.1 0 2-.9 2-2v-7h-2v7zM14 3v2h3.59l-9.83 9.83 1.41 1.41L19 6.41V10h2V3h-7z"/>"#,
        "logout" => r#"<path d="M17 7l-1.41 1.41L18.17 11H8v2h10.17l-2.58 2.58L17 17l5-5zM4 5h8V3H4c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h8v-2H4V5z"/>"#,
        "delete" => r#"<path d="M6 19c0 1.1.9 2 2 2h8c1.1 0 2-.9 2-2V7H6v12zM19 4h-3.5l-1-1h-5l-1 1H5v2h14V4z"/>"#,
        "restart" => r#"<path d="M17.65 6.35A7.958 7.958 0 0 0 12 4c-4.42 0-7.99 3.58-7.99 8s3.57 8 7.99 8c3.73 0 6.84-2.55 7.73-6h-2.08A5.99 5.99 0 0 1 12 18c-3.31 0-6-2.69-6-6s2.69-6 6-6c1.66 0 3.14.69 4.22 1.78L13 11h7V4l-2.35 2.35z"/>"#,
        "stop" => r#"<path d="M6 6h12v12H6z"/>"#,
        "archive" => r#"<path d="M20.54 5.23l-1.39-1.68C18.88 3.21 18.47 3 18 3H6c-.47 0-.88.21-1.16.55L3.46 5.23C3.17 5.57 3 6.02 3 6.5V19c0 1.1.9 2 2 2h14c1.1 0 2-.9 2-2V6.5c0-.48-.17-.93-.46-1.27zM12 17.5L6.5 12H10v-2h4v2h3.5L12 17.5zM5.12 5l.81-1h12l.94 1H5.12z"/>"#,
        "download" => r#"<path d="M19 9h-4V3H9v6H5l7 7 7-7zM5 18v2h14v-2H5z"/>"#,
        "upload" => r#"<path d="M9 16h6v-6h4l-7-7-7 7h4v6zm-4 2h14v2H5v-2z"/>"#,
        "check" => r#"<path d="M9 16.17L4.83 12l-1.42 1.41L9 19 21 7l-1.41-1.41z"/>"#,
        "chevron" => r#"<path d="M10 6L8.59 7.41 13.17 12l-4.58 4.59L10 18l6-6z"/>"#,
        "down" => r#"<path d="M7.41 8.59L12 13.17l4.59-4.58L18 10l-6 6-6-6z"/>"#,
        "search" => r#"<path d="M15.5 14h-.79l-.28-.27C15.41 12.59 16 11.11 16 9.5 16 5.91 13.09 3 9.5 3S3 5.91 3 9.5 5.91 16 9.5 16c1.61 0 3.09-.59 4.23-1.57l.27.28v.79l5 4.99L20.49 19l-4.99-5zm-6 0C7.01 14 5 11.99 5 9.5S7.01 5 9.5 5 14 7.01 14 9.5 11.99 14 9.5 14z"/>"#,
        "mail" => r#"<path d="M20 4H4c-1.1 0-1.99.9-1.99 2L2 18c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V6c0-1.1-.9-2-2-2zm0 4l-8 5-8-5V6l8 5 8-5v2z"/>"#,
        "lock" => r#"<path d="M18 8h-1V6c0-2.76-2.24-5-5-5S7 3.24 7 6v2H6c-1.1 0-2 .9-2 2v10c0 1.1.9 2 2 2h12c1.1 0 2-.9 2-2V10c0-1.1-.9-2-2-2zm-6 9c-1.1 0-2-.9-2-2s.9-2 2-2 2 .9 2 2-.9 2-2 2zm3.1-9H8.9V6c0-1.71 1.39-3.1 3.1-3.1s3.1 1.39 3.1 3.1v2z"/>"#,
        "warn" => r#"<path d="M1 21h22L12 2 1 21zm12-3h-2v-2h2v2zm0-4h-2v-4h2v4z"/>"#,
        "info" => r#"<path d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm1 15h-2v-6h2v6zm0-8h-2V7h2v2z"/>"#,
        "ghost" => r#"<path d="M12 2a8 8 0 0 0-8 8v10.6c0 .7.85 1.05 1.35.55l1.9-1.9 2.2 2.2c.3.3.78.3 1.08 0L12 20l1.47 1.45c.3.3.78.3 1.08 0l2.2-2.2 1.9 1.9c.5.5 1.35.15 1.35-.55V10a8 8 0 0 0-8-8zm-3 8.5A1.5 1.5 0 1 1 10.5 9 1.5 1.5 0 0 1 9 10.5zm6 0A1.5 1.5 0 1 1 16.5 9 1.5 1.5 0 0 1 15 10.5z"/>"#,
        "dns" => r#"<path d="M20 13H4c-.55 0-1 .45-1 1v6c0 .55.45 1 1 1h16c.55 0 1-.45 1-1v-6c0-.55-.45-1-1-1zM7 19c-1.1 0-2-.9-2-2s.9-2 2-2 2 .9 2 2-.9 2-2 2zM20 3H4c-.55 0-1 .45-1 1v6c0 .55.45 1 1 1h16c.55 0 1-.45 1-1V4c0-.55-.45-1-1-1zM7 9c-1.1 0-2-.9-2-2s.9-2 2-2 2 .9 2 2-.9 2-2 2z"/>"#,
        "key" => r#"<path d="M12.65 10A5.99 5.99 0 0 0 7 6c-3.31 0-6 2.69-6 6s2.69 6 6 6a5.99 5.99 0 0 0 5.65-4H17v4h4v-4h2v-4H12.65zM7 14c-1.1 0-2-.9-2-2s.9-2 2-2 2 .9 2 2-.9 2-2 2z"/>"#,
        "rocket" => r#"<path d="M12 2.5c3.5 2.1 5.5 5.9 5.5 9.8 0 1.6-.3 3.1-.9 4.5l-1.8-1.1c.4-1.1.7-2.2.7-3.4 0-2.9-1.3-5.7-3.5-7.5C9.8 6.6 8.5 9.4 8.5 12.3c0 1.2.2 2.3.7 3.4l-1.8 1.1c-.6-1.4-.9-2.9-.9-4.5 0-3.9 2-7.7 5.5-9.8zM12 10a2 2 0 1 0 0 4 2 2 0 0 0 0-4zm-3.4 9.1 1.6-1v2.6c0 .5-.6.8-1 .5l-1.3-1c-.3-.3-.3-.8 0-1.1l.7-.7v.7zm6.8 0 .7.7c.3.3.3.8 0 1.1l-1.3 1c-.4.3-1 0-1-.5v-2.6l1.6 1v-.7z"/>"#,
        "clock" => r#"<path d="M11.99 2C6.47 2 2 6.48 2 12s4.47 10 9.99 10C17.52 22 22 17.52 22 12S17.52 2 11.99 2zM12 20c-4.42 0-8-3.58-8-8s3.58-8 8-8 8 3.58 8 8-3.58 8-8 8zm.5-13H11v6l5.25 3.15.75-1.23-4.5-2.67z"/>"#,
        "replay" => r#"<path d="M12 5V1L7 6l5 5V7c3.31 0 6 2.69 6 6s-2.69 6-6 6-6-2.69-6-6H4c0 4.42 3.58 8 8 8s8-3.58 8-8-3.58-8-8-8z"/>"#,
        "menu" => r#"<path d="M3 18h18v-2H3v2zm0-5h18v-2H3v2zm0-7v2h18V6H3z"/>"#,
        _ => "",
    }
}

/// An inline icon, sized by the `i` class the stylesheet owns.
pub fn svg(name: &str) -> String {
    format!(
        r#"<svg class="i" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">{}</svg>"#,
        icon(name)
    )
}

/// The one place the admin borrows another set: the appearance menu uses
/// lucide's stroked sun, moon and monitor, because no filled Material glyph
/// reads as cleanly for it. Same `class="i"` contract as [`svg`].
pub fn svg_stroke(name: &str) -> String {
    let body = match name {
        "sun" => r#"<circle cx="12" cy="12" r="4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.93 4.93 1.41 1.41"/><path d="m17.66 17.66 1.41 1.41"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.34 17.66-1.41 1.41"/><path d="m19.07 4.93-1.41 1.41"/>"#,
        "moon" => r#"<path d="M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"/>"#,
        "monitor" => r#"<rect width="20" height="14" x="2" y="3" rx="2"/><line x1="8" x2="16" y1="21" y2="21"/><line x1="12" x2="12" y1="17" y2="21"/>"#,
        _ => "",
    };
    format!(
        r#"<svg class="i t-{name}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{body}</svg>"#
    )
}

// ---------------------------------------------------------------------------
// Appearance
// ---------------------------------------------------------------------------

/// Which colour scheme a visitor has asked for.
///
/// Ghost calls this "night shift" and keeps it per user; there is no user here
/// on the public pages, so it rides in a cookie and is resolved server-side.
/// `System` is the one choice the server cannot settle, and it is the only one
/// that needs the media query in the stylesheet.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Theme {
    #[default]
    Light,
    Dark,
    System,
}

/// The cookie the appearance choice lives in. Not `Secure`-only: the platform
/// runs over plain HTTP in development, and a colour scheme is not a secret.
pub const THEME_COOKIE: &str = "pro_theme";

impl Theme {
    pub fn parse(raw: &str) -> Option<Theme> {
        match raw.trim() {
            "light" => Some(Theme::Light),
            "dark" => Some(Theme::Dark),
            "system" => Some(Theme::System),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
            Theme::System => "system",
        }
    }

    /// The class that goes on `<html>`. `light` needs none — it is the
    /// stylesheet's own `:root`.
    fn html_class(self) -> &'static str {
        match self {
            Theme::Light => "",
            Theme::Dark => " class=\"dark\"",
            Theme::System => " class=\"system\"",
        }
    }
}

// ---------------------------------------------------------------------------
// The stylesheet
// ---------------------------------------------------------------------------

/// The dark scheme's tokens, as a declaration list.
///
/// Held apart from the rest because it is emitted twice: once for the class a
/// `dark` choice puts on `<html>`, and once inside a `prefers-color-scheme`
/// query for the `system` choice the server cannot resolve itself.
const DARK: &str = r#"
  color-scheme: dark;
  --primary: #FFB1C8; --on-primary: #5E1133;
  --primary-container: #7B294B; --on-primary-container: #FFD9E0;
  --secondary-container: #5B4149; --on-secondary-container: #FFD9DE;
  --tertiary-container: #6B5300; --on-tertiary-container: #FFE396;
  --error: #FFB4AB; --error-container: #93000A; --on-error-container: #FFDAD6;
  --surface: #171214; --on-surface: #ECE0E3; --on-surface-variant: #D0C2C6;
  --sc-lowest: #120E10; --sc-low: #1F1A1C; --sc: #241F21; --sc-high: #2F292B; --sc-highest: #3A3436;
  --outline: #998D91; --outline-variant: #4B3F42;
  --inverse-surface: #ECE0E3; --inverse-on-surface: #352F30;
  --blue-bg: #1F3E75; --blue-fg: #AECBFF;
  --green-bg: #173B23; --green-fg: #9CD8AA;
  --amber-bg: #54420A; --amber-fg: #F1CE7F;
  --purple-bg: #4A3085; --purple-fg: #D0BCFF;
  --pink-bg: #672739; --pink-fg: #FFB1C8;
  --teal-bg: #0C4A58; --teal-fg: #8FD8EA;
  --red-bg: #6A1B16; --red-fg: #FFB4AB;
  --slate-bg: #2C2F36; --slate-fg: #C6CAD8;
  --shadow-1: 0 1px 2px rgba(0,0,0,.5); --shadow-2: 0 2px 8px rgba(0,0,0,.55);
"#;

/// Everything but the dark tokens.
const BASE: &str = r#"
/* ---------------------------------------------------------------------------
   Material 3 — a tonal palette generated from Ghost's brand pink (#FF1A75),
   light and dark schemes, and M3 component styles. The same system the Ghost
   CMS port's admin uses, so the two surfaces read as one product.
--------------------------------------------------------------------------- */
:root {
  color-scheme: light;
  --primary: #984059; --on-primary: #fff;
  --primary-container: #FFD9E0; --on-primary-container: #3E071D;
  --secondary-container: #FADDE1; --on-secondary-container: #2B151A;
  --tertiary-container: #FBDEA7; --on-tertiary-container: #291800;
  --error: #BA1A1A; --error-container: #FFDAD6; --on-error-container: #410002;
  --surface: #FFF8F8; --on-surface: #201A1B; --on-surface-variant: #514347;
  --sc-lowest: #FFFFFF; --sc-low: #FAEFF1; --sc: #F4EAEC; --sc-high: #EFDFE3; --sc-highest: #E9DBDF;
  --outline: #847375; --outline-variant: #D7C3C7;
  --inverse-surface: #352F30; --inverse-on-surface: #F9EDEF;
  --blue-bg: #D8E2FF; --blue-fg: #001A41;
  --green-bg: #C4EED0; --green-fg: #0C3D24;
  --amber-bg: #FFDF9E; --amber-fg: #483000;
  --purple-bg: #EADDFF; --purple-fg: #21005D;
  --pink-bg: #FFD9E0; --pink-fg: #3E071D;
  --teal-bg: #BEE9F2; --teal-fg: #0A495B;
  --red-bg: #FFDAD6; --red-fg: #410002;
  --slate-bg: #E0E2EC; --slate-fg: #1B1C22;
  --font: 'Outfit', 'Google Sans', 'Product Sans', system-ui, -apple-system, sans-serif;
  /* Editorial serif for headlines. Fraunces is a variable face with an
     optical-size axis, so a 15px card title and a 30px hero title are drawn
     with different contrast rather than the same outline scaled. */
  --display: 'Fraunces', 'Iowan Old Style', Georgia, 'Times New Roman', serif;
  --mono: 'Roboto Mono', ui-monospace, 'SF Mono', Menlo, monospace;
  --ease: cubic-bezier(.2,0,0,1);
  --dur: .18s;
  --shadow-1: 0 1px 2px rgba(0,0,0,.08), 0 1px 3px 1px rgba(0,0,0,.04);
  --shadow-2: 0 1px 2px rgba(0,0,0,.1), 0 2px 6px 2px rgba(0,0,0,.05);
}
*, *::before, *::after { box-sizing: border-box; }
html { scrollbar-color: var(--outline-variant) transparent; }
body { font-family: var(--font); margin: 0; background: var(--surface); color: var(--on-surface);
  font-size: 15px; line-height: 1.5; -webkit-font-smoothing: antialiased; }
::selection { background: var(--primary-container); color: var(--on-primary-container); }
a { color: inherit; }
svg.i { width: 22px; height: 22px; flex: none; display: block; }
code, .mono { font-family: var(--mono); }
.muted { color: var(--on-surface-variant); }
.small { font-size: 13px; }
.hint, .help { color: var(--on-surface-variant); font-size: 13px; margin: 6px 0 0; }
.sr-only { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
  overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; border: 0; }
[hidden] { display: none !important; }

/* ---- shell: nav rail + content ---- */
.shell { display: flex; min-height: 100vh; }
.rail { width: 240px; flex: none; padding: 8px 10px 12px; display: flex; flex-direction: column; gap: 2px;
  background: var(--sc-low); position: sticky; top: 0; height: 100vh; overflow-y: auto;
  scrollbar-width: thin; scrollbar-color: var(--outline-variant) transparent;
  border-right: 1px solid color-mix(in srgb, var(--outline-variant) 55%, transparent); }
/* One mark only, given the whole slot so it can run larger than it could
   beside a label. */
.brand { display: flex; align-items: center; gap: 10px; min-height: 64px;
  padding: 12px 10px 14px; color: inherit; text-decoration: none; border-radius: 12px; }
.brand:hover { background: color-mix(in srgb, var(--on-surface) 6%, transparent); }
.brand:focus-visible { outline: 2px solid var(--primary); outline-offset: 2px; }
.brand .mark { color: var(--primary); display: grid; place-items: center; flex: none; }
.brand .mark svg { width: 34px; height: 34px; }
.brand .brand-name { min-width: 0; overflow-wrap: anywhere; }
.brand b { font-size: 18px; font-weight: 600; letter-spacing: -.1px; display: block; line-height: 1.2; }
.brand .tag { display: block; font-size: 11px; font-weight: 600; text-transform: uppercase;
  letter-spacing: .08em; color: var(--primary); }
.nav-item { display: flex; align-items: center; gap: 10px; padding: 6px 10px; border-radius: 10px;
  text-decoration: none; color: var(--on-surface-variant); font-size: 13.5px; font-weight: 500;
  transition: background var(--dur) var(--ease), color var(--dur) var(--ease); position: relative; }
.nav-item:hover { background: color-mix(in srgb, var(--on-surface) 6%, transparent); }
.nav-item.active { background: var(--secondary-container); color: var(--on-secondary-container); font-weight: 600; }
.nav-item:focus-visible { outline: 2px solid var(--primary); outline-offset: 2px; }
.nav-item .nav-label { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
/* The rail runs at its own, tighter scale: the tinted squares elsewhere lead
   56px list rows, where 40px is right; as navigation they only have to read
   at a glance. */
.rail .ic { width: 28px; height: 28px; border-radius: 8px; }
.rail .ic svg { width: 17px; height: 17px; }
.ic { width: 40px; height: 40px; border-radius: 14px; display: grid; place-items: center; flex: none; }
.ic svg { width: 21px; height: 21px; }
.tint-blue { background: var(--blue-bg); color: var(--blue-fg); }
.tint-green { background: var(--green-bg); color: var(--green-fg); }
.tint-amber { background: var(--amber-bg); color: var(--amber-fg); }
.tint-purple { background: var(--purple-bg); color: var(--purple-fg); }
.tint-pink { background: var(--pink-bg); color: var(--pink-fg); }
.tint-teal { background: var(--teal-bg); color: var(--teal-fg); }
.tint-red { background: var(--red-bg); color: var(--red-fg); }
.tint-slate { background: var(--slate-bg); color: var(--slate-fg); }
.rail-foot { margin-top: auto; padding-top: 10px; position: relative;
  border-top: 1px solid color-mix(in srgb, var(--outline-variant) 55%, transparent); }
.who { display: flex; align-items: center; gap: 8px; padding: 8px 6px 2px; }
.avatar { width: 30px; height: 30px; border-radius: 50%; background: var(--primary); color: var(--on-primary);
  display: grid; place-items: center; font-weight: 600; font-size: 13px; flex: none; }
.who b { font-size: 13px; font-weight: 600; display: block; line-height: 1.2; }
.who span { font-size: 11px; color: var(--on-surface-variant); }
.rail .icon-btn { width: 32px; height: 32px; }
.rail .icon-btn svg.i { width: 18px; height: 18px; }
/* The account block is a link to your own profile; long names and addresses
   must not push out the rail. */
.who-link { display: flex; align-items: center; gap: 10px; flex: 1; min-width: 0;
  color: inherit; text-decoration: none; border-radius: 999px; padding: 4px; margin: -4px;
  transition: background var(--dur) var(--ease); }
.who-link:hover { background: color-mix(in srgb, var(--on-surface) 6%, transparent); }
.who-link:focus-visible { outline: 2px solid var(--primary); outline-offset: 2px; }
.who-link > div { min-width: 0; }
.who-link b, .who-link span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; display: block; }
.main { flex: 1; min-width: 0; }
.content { max-width: 1120px; margin: 0 auto; padding: 0 24px 64px; }
.content.narrow { max-width: 640px; }

/* ---- appearance menu ----
   Ghost's own switcher offers Dark, Light and System from the account menu.
   Same trio here — in the rail foot beside sign-out on the panels, in the top
   bar on the public pages, and in the corner of the chrome-free auth and error
   pages — and with no script, so each row is a one-button form that posts the
   choice and comes back. */
.theme-menu > summary { list-style: none; }
.theme-menu > summary::-webkit-details-marker { display: none; }
/* The button shows the *resolved* scheme, not the preference: System on a
   dark OS is a moon, and only the class on <html> flips the glyph. */
.theme-menu > summary .t-moon { display: none; }
html.dark .theme-menu > summary .t-moon { display: block; }
html.dark .theme-menu > summary .t-sun { display: none; }
/* The popover anchors to the switcher itself, so it opens beside the button
   wherever the button is put. That default matters: an absolutely positioned
   box with no positioned ancestor falls back to the initial containing block,
   and `bottom: 100%` against *that* lands the menu off the top of the page —
   which is what made the public pages' switcher unclickable. Opening downward
   is likewise the safe default, because every placement but the rail's sits at
   the top of the viewport. The rail is the one exception, right below. */
.theme-menu { position: relative; }
.theme-menu .pop { position: absolute; top: calc(100% + 10px); bottom: auto; left: auto; right: 0;
  width: 220px; z-index: 45;
  background: var(--sc-lowest); border: 1px solid var(--outline-variant); border-radius: 14px;
  box-shadow: var(--shadow-2); padding: 6px; }
/* In the rail the popover anchors to the whole rail-foot row rather than the
   32px button, so it can span the rail's width and still open inside it — the
   rail scrolls, and a box hanging past its edge would grow a scrollbar. The
   foot sits at the bottom of a tall column, so there it opens upward. */
.rail .theme-menu { position: static; }
.rail .theme-menu .pop { top: auto; bottom: calc(100% + 10px); left: 0; right: 0; width: auto; }
.pop-title { margin: 6px 12px 8px; font-size: 11px; font-weight: 600; letter-spacing: .06em;
  text-transform: uppercase; color: var(--on-surface-variant); }
.menu-item { display: flex; align-items: center; gap: 10px; padding: 9px 12px; border-radius: 9px;
  font-size: 13.5px; font-weight: 500; color: var(--on-surface); text-decoration: none;
  width: 100%; border: none; background: none; font-family: inherit; text-align: left; cursor: pointer; }
.menu-item:hover { background: color-mix(in srgb, var(--on-surface) 7%, transparent); }
.menu-item:focus-visible { outline: 2px solid var(--primary); outline-offset: -2px; }
.menu-item .ck { width: 18px; height: 18px; flex: none; display: grid; place-items: center;
  color: transparent; margin-left: auto; }
.menu-item .ck svg { width: 16px; height: 16px; }
.menu-item.on { font-weight: 600; }
.menu-item.on .ck { color: var(--primary); }
/* The leading glyph only — the check rides in .ck, whose transparent colour
   is what hides it on the rows that are not chosen. */
.theme-menu .menu-item > svg.i { width: 17px; height: 17px; color: var(--on-surface-variant); }
.theme-menu .menu-item.on > svg.i { color: inherit; }
.theme-menu form { display: contents; }

/* ---- page head (sticky top app bar per page) ---- */
.page-head { position: sticky; top: 0; z-index: 20; display: flex; align-items: center; gap: 16px;
  padding: 22px 4px 16px; margin-bottom: 8px;
  background: color-mix(in srgb, var(--surface) 82%, transparent); backdrop-filter: blur(16px);
  -webkit-backdrop-filter: blur(16px); }
.page-head h1 { margin: 0; font-size: 24px; font-weight: 600; letter-spacing: 0; }
.page-head .sub { margin: 2px 0 0; font-size: 13.5px; color: var(--on-surface-variant); }
.page-head .sub a { text-decoration: none; border-bottom: 1px solid var(--outline-variant); }
.page-head .heading { min-width: 0; }
.page-head .actions { margin-left: auto; display: flex; gap: 10px; align-items: center;
  flex-wrap: wrap; justify-content: flex-end; }
/* List-shaped pages borrow the editorial face, so the page has one voice. */
.page-head.editorial h1 { font-family: var(--display); font-optical-sizing: auto;
  font-weight: 600; letter-spacing: -.02em; font-size: 27px; }

/* ---- groups, cards, rows ---- */
.section { font-size: 13px; font-weight: 600; color: var(--on-surface-variant);
  padding: 20px 16px 10px; letter-spacing: .2px; }
.card { background: var(--sc-lowest); border-radius: 24px; margin-bottom: 12px; overflow: hidden;
  box-shadow: var(--shadow-1); }
.card.padded { padding: 20px; }
.card.flush { padding: 0; }
.rows .row { display: flex; align-items: center; gap: 16px; padding: 13px 20px; position: relative;
  text-decoration: none; color: inherit; transition: background var(--dur) var(--ease); }
.rows .row + .row::before { content: ""; position: absolute; top: 0; left: 76px; right: 0; height: 1px;
  background: var(--outline-variant); }
a.row:hover, .rows .row.hoverable:hover { background: color-mix(in srgb, var(--on-surface) 4%, transparent); }
.rows .row .body { flex: 1; min-width: 0; }
/* Title over detail, each clipped to the body's width. Plain inline spans
   would let a long value run through the chips and buttons to the right:
   ellipsis only applies to a block-level box with a width to overflow. */
.rows .row .title { display: block; font-size: 15.5px; font-weight: 500;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.rows .row .detail { display: block; font-size: 13px; color: var(--on-surface-variant);
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.rows .row .detail code { font-family: var(--mono); font-size: 12px; }
.rows.settings .row .title, .rows.settings .row .detail { white-space: normal; overflow: visible; text-overflow: clip; }
.trailing { margin-left: auto; display: flex; align-items: center; gap: 8px; flex: none; flex-wrap: wrap;
  justify-content: flex-end; }
.chev { color: var(--on-surface-variant); opacity: .7; }
.divider { height: 1px; background: var(--outline-variant); margin: 0; border: 0; }

/* ---- chips ---- */
.chip { display: inline-flex; align-items: center; gap: 6px; height: 26px; padding: 0 12px;
  border-radius: 8px; font-size: 12px; font-weight: 600; letter-spacing: .2px; white-space: nowrap;
  background: var(--sc-high); color: var(--on-surface-variant); }
.chip.ok { background: var(--green-bg); color: var(--green-fg); }
.chip.warn { background: var(--amber-bg); color: var(--amber-fg); }
.chip.bad { background: var(--red-bg); color: var(--red-fg); }
.chip.idle { background: var(--slate-bg); color: var(--slate-fg); }
.chip.info { background: var(--blue-bg); color: var(--blue-fg); }
.chip.tonal { background: var(--secondary-container); color: var(--on-secondary-container); }
.chip.outline { background: transparent; border: 1px solid var(--outline); }

/* ---- segmented filter tabs (a row of links) ---- */
.seg-bar { display: flex; flex-wrap: wrap; gap: 8px; margin: 2px 0 16px; }
.seg { display: inline-flex; align-items: center; gap: 8px; height: 36px; padding: 0 16px;
  border-radius: 999px; font-size: 13.5px; font-weight: 500; text-decoration: none;
  color: var(--on-surface-variant); background: transparent;
  border: 1px solid var(--outline-variant); transition: all var(--dur) var(--ease); }
.seg:hover { background: color-mix(in srgb, var(--on-surface) 5%, transparent); color: var(--on-surface); }
.seg.on { background: var(--secondary-container); color: var(--on-secondary-container);
  border-color: transparent; font-weight: 600; }
.seg:focus-visible { outline: 2px solid var(--primary); outline-offset: 2px; }
.seg-count { display: inline-grid; place-items: center; min-width: 22px; height: 22px; padding: 0 6px;
  border-radius: 999px; font-size: 11.5px; font-weight: 600; font-variant-numeric: tabular-nums;
  background: color-mix(in srgb, var(--on-surface) 8%, transparent); color: inherit; }
.seg.on .seg-count { background: color-mix(in srgb, var(--on-secondary-container) 14%, transparent); }

/* ---- buttons ---- */
.btn { display: inline-flex; align-items: center; justify-content: center; gap: 8px; height: 42px;
  padding: 0 24px; border-radius: 999px; border: none; background: var(--primary); color: var(--on-primary);
  font-family: inherit; font-size: 14px; font-weight: 600; letter-spacing: .1px; text-decoration: none;
  cursor: pointer; white-space: nowrap;
  transition: box-shadow var(--dur) var(--ease), transform .1s var(--ease), background var(--dur) var(--ease); }
.btn:hover { box-shadow: var(--shadow-1); }
.btn:active { transform: scale(.97); }
.btn svg.i { width: 18px; height: 18px; }
.btn.tonal { background: var(--secondary-container); color: var(--on-secondary-container); }
.btn.outlined { background: transparent; border: 1px solid var(--outline); color: var(--primary); }
.btn.outlined:hover { background: color-mix(in srgb, var(--primary) 8%, transparent); }
.btn.text { background: transparent; color: var(--primary); padding: 0 14px; }
.btn.text:hover { background: color-mix(in srgb, var(--primary) 10%, transparent); box-shadow: none; }
.btn.danger { background: var(--error); color: var(--sc-lowest); }
html.dark .btn.danger { color: var(--inverse-on-surface); }
.btn.outlined.danger, .btn.text.danger { background: transparent; color: var(--error); border-color: var(--error); }
.btn.outlined.danger:hover, .btn.text.danger:hover { background: color-mix(in srgb, var(--error) 10%, transparent); }
.btn.small { height: 34px; padding: 0 16px; font-size: 13px; }
.btn.block { width: 100%; }
.btn[disabled] { opacity: .45; cursor: not-allowed; box-shadow: none; }
.btn:focus-visible, .icon-btn:focus-visible { outline: 2px solid var(--primary); outline-offset: 2px; }
.icon-btn { width: 40px; height: 40px; border-radius: 50%; border: none; background: transparent;
  color: var(--on-surface-variant); display: inline-grid; place-items: center; cursor: pointer; flex: none;
  transition: background var(--dur) var(--ease), color var(--dur) var(--ease); text-decoration: none; }
.icon-btn:hover { background: color-mix(in srgb, var(--on-surface) 8%, transparent); color: var(--on-surface); }
.icon-btn.danger:hover { background: var(--error-container); color: var(--on-error-container); }
.icon-btn svg.i { width: 20px; height: 20px; }
form.inline { display: inline-flex; margin: 0; }

/* ---- confirmation, without script ----
   Every page is served under `default-src 'none'`, which blocks an inline
   submit handler as surely as it blocks a script element — so a `confirm()`
   prompt would simply never appear. A <details> holding the real submit
   button gives the same two-step with no script at all: the first click
   opens the question, and only the button inside it posts. */
.confirm { position: relative; display: inline-flex; }
.confirm > summary { list-style: none; }
.confirm > summary::-webkit-details-marker { display: none; }
.confirm > summary:focus-visible { outline: 2px solid var(--primary); outline-offset: 2px; }
.confirm .pop { position: absolute; right: 0; top: calc(100% + 8px); z-index: 50;
  width: min(320px, calc(100vw - 32px)); padding: 16px; text-align: left;
  background: var(--sc-lowest); border: 1px solid var(--outline-variant); border-radius: 18px;
  box-shadow: var(--shadow-2); }
.confirm .pop p { margin: 0 0 14px; font-size: 13.5px; color: var(--on-surface-variant); line-height: 1.5; }
.confirm .pop .pop-title { margin: 0 0 6px; font-size: 14.5px; font-weight: 600;
  color: var(--on-surface); text-transform: none; letter-spacing: 0; padding: 0; }
.confirm .pop .btn { width: 100%; }

/* ---- text fields (M3 outlined, floating label) ---- */
.field { position: relative; display: block; margin-bottom: 16px; }
.field input, .field textarea, .field select { width: 100%; font-family: inherit; font-size: 15px;
  color: var(--on-surface); background: transparent; border: 1px solid var(--outline);
  border-radius: 12px; padding: 22px 14px 6px; outline: none; caret-color: var(--primary);
  transition: border-color var(--dur) var(--ease), box-shadow var(--dur) var(--ease); }
.field textarea { resize: vertical; min-height: 96px; }
.field > span { position: absolute; left: 11px; top: 15px; font-size: 15px; color: var(--on-surface-variant);
  padding: 0 5px; pointer-events: none; transition: all var(--dur) var(--ease); background: var(--sc-lowest); }
.field input:focus, .field textarea:focus, .field select:focus { border-color: var(--primary);
  box-shadow: inset 0 0 0 1px var(--primary); }
.field input[disabled], .field select[disabled] { color: var(--on-surface-variant);
  background: color-mix(in srgb, var(--on-surface) 4%, transparent); cursor: not-allowed; }
/* Asked of the field, not of the input's next sibling. `input + span` is the
   obvious way to write a floating label and it breaks in the wild: password
   managers inject their own button directly after the input they offer to
   fill, which leaves the label sitting on top of the value. `:has()` does not
   care where in the field the control is. */
.field:focus-within > span,
.field:has(input:not(:placeholder-shown)) > span,
.field:has(textarea:not(:placeholder-shown)) > span,
.field:has(input[placeholder]:not([placeholder=" "]):not([placeholder=""])) > span,
.field:has(select) > span,
.field.fixed > span, .field:has(textarea) > span { top: -8px; font-size: 12px; font-weight: 600;
  color: var(--primary); background: var(--sc-lowest); border-radius: 4px; }
.field:has(input[disabled]) > span, .field:has(select[disabled]) > span { color: var(--on-surface-variant); }
.field select { appearance: none; -webkit-appearance: none; cursor: pointer; padding-right: 40px; }
.field .car { position: absolute; right: 12px; top: 50%; transform: translateY(-50%); pointer-events: none;
  color: var(--on-surface-variant); width: 20px; height: 20px; }
.field .car svg { width: 20px; height: 20px; }
/* A field on a card that is itself tinted needs its label's backing to match,
   or the notch in the outline reads as a white sticker. */
.card.padded .field > span { background: var(--sc-lowest); }
.grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 14px; }
/* A field and the button that submits it, on one line. The fields carry
   bottom margin for stacked forms, and in a flex row that margin is part of
   the item: `align-items: flex-end` would then line the button up with the
   margin's edge, leaving it 16px below the input it belongs to. */
.inline-form { display: flex; gap: 12px; align-items: flex-end; flex-wrap: wrap; }
.inline-form .field { margin-bottom: 0; flex: 1; min-width: 220px; }
.badge { display: inline-flex; align-items: center; gap: 4px; margin-left: 8px; font-size: 10.5px;
  font-weight: 700; text-transform: uppercase; letter-spacing: .07em; padding: 2px 7px;
  border-radius: 6px; vertical-align: middle; }
.badge.locked { background: var(--amber-bg); color: var(--amber-fg); }
.badge.set { background: var(--blue-bg); color: var(--blue-fg); }

/* ---- switch (M3) ---- */
.switch { position: relative; display: inline-flex; flex: none; cursor: pointer; }
.switch input { position: absolute; opacity: 0; width: 100%; height: 100%; margin: 0; cursor: pointer; }
.switch .track { width: 52px; height: 32px; border-radius: 16px; background: var(--sc-highest);
  box-shadow: inset 0 0 0 2px var(--outline); transition: all .2s var(--ease); display: flex; align-items: center; }
.switch .thumb { width: 16px; height: 16px; border-radius: 50%; background: var(--outline);
  margin-left: 6px; transition: all .2s var(--ease); }
.switch input:checked ~ .track { background: var(--primary); box-shadow: none; }
.switch input:checked ~ .track .thumb { width: 24px; height: 24px; background: var(--on-primary);
  margin-left: auto; margin-right: 6px; }
.switch input:focus-visible ~ .track { outline: 2px solid var(--primary); outline-offset: 3px; }
.switch input:disabled ~ .track { opacity: .5; cursor: not-allowed; }
.toggle-row { display: flex; align-items: center; gap: 14px; min-height: 48px; }
.toggle-row .label { flex: 1; min-width: 0; }
.toggle-row .label b { display: block; font-size: 14.5px; font-weight: 500; }
.toggle-row .label span { display: block; font-size: 12.5px; line-height: 1.4; color: var(--on-surface-variant); }

/* ---- compact trailing controls for settings rows ----
   A row already states what the setting is, on the left. The control beside
   it is the value alone: one line, vertically centred, no floating label and
   none of the head room a label would need. */
.selbox, .textbox { position: relative; display: inline-flex; flex: none; }
.selbox select, .textbox input { height: 40px; min-width: 160px; max-width: 260px;
  border: 1px solid var(--outline); border-radius: 12px; background: transparent; color: var(--on-surface);
  font-family: inherit; font-size: 13.5px; font-weight: 500; outline: none;
  transition: border-color var(--dur) var(--ease), box-shadow var(--dur) var(--ease); }
.selbox select { appearance: none; -webkit-appearance: none; padding: 0 38px 0 16px; cursor: pointer; }
.textbox input { width: 100%; padding: 0 14px; }
.selbox select:hover, .textbox input:hover { background: color-mix(in srgb, var(--on-surface) 5%, transparent); }
.selbox select:focus, .textbox input:focus { border-color: var(--primary); box-shadow: inset 0 0 0 1px var(--primary); }
.selbox select[disabled] { color: var(--on-surface-variant); cursor: not-allowed; }
.selbox .car { position: absolute; right: 10px; top: 50%; transform: translateY(-50%); pointer-events: none;
  color: var(--on-surface-variant); }
.selbox .car svg { width: 20px; height: 20px; }

/* ---- search bar ---- */
.search { display: flex; align-items: center; gap: 12px; background: var(--sc-lowest); border-radius: 999px;
  box-shadow: var(--shadow-1); padding: 0 8px 0 18px; height: 46px; margin: 4px 0 16px;
  color: var(--on-surface-variant); }
.search input { flex: 1; min-width: 0; border: none; outline: none; background: transparent;
  font-family: inherit; font-size: 14.5px; color: var(--on-surface); height: 100%; }
.search .btn { height: 34px; }

/* ---- banners ---- */
.banner { display: flex; gap: 12px; align-items: flex-start; border-radius: 16px; padding: 14px 18px;
  font-size: 14px; margin-bottom: 12px; }
.banner svg.i { width: 20px; height: 20px; margin-top: 1px; flex: none; }
/* The message is one flex item: without this wrapper every <code> or <strong>
   inside it becomes its own flex item and the sentence breaks into columns. */
.banner > .msg { flex: 1; min-width: 0; }
.banner > .msg a { font-weight: 600; }
.banner.ok { background: var(--green-bg); color: var(--green-fg); }
.banner.error { background: var(--red-bg); color: var(--red-fg); }
.banner.info { background: var(--blue-bg); color: var(--blue-fg); }
.banner.warn { background: var(--amber-bg); color: var(--amber-fg); }
.banner .btn.tonal { background: color-mix(in srgb, currentColor 14%, transparent); color: inherit; }
.key { display: inline-flex; align-items: center; gap: 8px; background: var(--sc);
  border-radius: 8px; padding: 5px 12px; font-family: var(--mono); font-size: 12.5px;
  word-break: break-all; }

/* ---- stat tiles ---- */
.tiles { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 12px; margin-bottom: 12px; }
.tiles.c3 { grid-template-columns: repeat(3, minmax(0, 1fr)); }
.tiles.c2 { grid-template-columns: repeat(2, minmax(0, 1fr)); }
.tile { background: var(--sc-lowest); border-radius: 24px; padding: 18px 20px; box-shadow: var(--shadow-1); }
.tile .label { font-size: 13px; font-weight: 500; color: var(--on-surface-variant); }
.tile .value { font-size: 30px; font-weight: 600; font-variant-numeric: proportional-nums;
  line-height: 1.25; margin-top: 2px; letter-spacing: -.02em; }
.tile .value .unit { font-size: 13px; font-weight: 500; color: var(--on-surface-variant); letter-spacing: 0; }
.tile .delta { font-size: 12.5px; color: var(--on-surface-variant); margin-top: 2px; }
/* The usage meter under a figure. A bar at or over its ceiling has to look
   different, because that is the moment the customer needs to notice it. */
.bar { height: 6px; border-radius: 3px; background: color-mix(in srgb, var(--on-surface) 10%, transparent);
  overflow: hidden; margin-top: 10px; }
.bar > span { display: block; height: 100%; border-radius: 3px; background: var(--primary); }
.bar.full > span { background: var(--error); }
@media (max-width: 919px) { .tiles, .tiles.c3 { grid-template-columns: 1fr 1fr; } }

/* ---- data tables ----
   Rows, not cards: a list of two hundred sites is scanned down a column, and
   fixed column widths are the whole reason a table beats cards here. No
   vertical rules and no zebra — a hairline between rows is enough. */
.tbl-wrap { overflow-x: auto; }
table.tbl { width: 100%; border-collapse: collapse; font-size: 13.5px; }
table.tbl thead th { font-size: 11px; font-weight: 600; letter-spacing: .06em;
  text-transform: uppercase; color: var(--on-surface-variant); opacity: .8;
  padding: 14px 16px 10px; text-align: left; white-space: nowrap;
  border-bottom: 1px solid var(--outline-variant); }
table.tbl tbody td { padding: 12px 16px; vertical-align: middle; border-top: 1px solid var(--outline-variant); }
table.tbl tbody tr:first-child td { border-top: 0; }
table.tbl tbody tr:hover { background: color-mix(in srgb, var(--on-surface) 3%, transparent); }
table.tbl td.num, table.tbl th.num { text-align: right; font-variant-numeric: tabular-nums; }
table.tbl td.end, table.tbl th.end { text-align: right; white-space: nowrap; }
table.tbl td .lead { display: block; font-size: 14.5px; font-weight: 600; letter-spacing: -.006em; }
table.tbl td a.lead { text-decoration: none; }
table.tbl tbody tr:hover a.lead { color: var(--primary); }
table.tbl td .sub { display: block; font-size: 12.5px; color: var(--on-surface-variant); }
table.tbl td.wrap { white-space: normal; overflow-wrap: anywhere; }
table.tbl td.empty-cell { padding: 32px 16px; text-align: center; color: var(--on-surface-variant); }

/* ---- empty state ---- */
.empty { text-align: center; padding: 52px 24px 56px; }
.empty .ic { margin: 0 auto 18px; width: 60px; height: 60px; border-radius: 20px;
  background: var(--primary-container); color: var(--on-primary-container); }
.empty .ic svg { width: 30px; height: 30px; }
.empty h2 { margin: 0 0 6px; font-size: 20px; font-weight: 600; }
.empty p { margin: 0 auto 20px; max-width: 46ch; color: var(--on-surface-variant); font-size: 14px; }

/* ---- plan cards ----
   M3 filled cards. The chosen one is outlined rather than recoloured, so the
   row still reads as one set of options. */
.plans { display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 16px;
  margin-bottom: 12px; }
.plan { display: flex; flex-direction: column; gap: 4px; padding: 26px; border-radius: 28px;
  background: var(--sc-low); color: var(--on-surface); border: 1px solid var(--outline-variant);
  transition: background var(--dur) var(--ease), box-shadow var(--dur) var(--ease); }
.plan:hover { background: var(--sc-high); box-shadow: var(--shadow-1); }
.plan.on { border-color: var(--primary); box-shadow: inset 0 0 0 1px var(--primary); background: var(--sc-lowest); }
.plan h3 { margin: 0; font-size: 20px; font-weight: 600; letter-spacing: -.2px; color: var(--on-surface); }
.plan .price { font-size: 30px; font-weight: 600; letter-spacing: -.03em; line-height: 1.2; margin: 6px 0 2px; }
.plan .price span { font-size: 14px; font-weight: 500; color: var(--on-surface-variant); letter-spacing: 0; }
.plan .desc { margin: 4px 0 12px; font-size: 13.5px; color: var(--on-surface-variant); line-height: 1.55; }
.plan ul { list-style: none; padding: 0; margin: 0 0 18px; font-size: 13.5px;
  color: var(--on-surface-variant); }
.plan li { padding: 4px 0 4px 24px; position: relative; }
.plan li::before { content: ""; position: absolute; left: 0; top: 10px; width: 14px; height: 8px;
  border-left: 2px solid var(--primary); border-bottom: 2px solid var(--primary);
  transform: rotate(-45deg) scale(.9); transform-origin: left bottom; }
.plan .spacer { flex: 1; }
.plan .trial { margin: 0 0 10px; font-size: 12.5px; font-weight: 600; color: var(--primary); }
.plan .btn { width: 100%; }

/* ---- feature cards, and the public hero ---- */
.hero { text-align: center; padding: 56px 8px 36px; }
.hero h1 { font-family: var(--display); font-optical-sizing: auto; margin: 0 0 10px;
  font-size: clamp(30px, 5vw, 44px); font-weight: 600; letter-spacing: -.03em; line-height: 1.1; }
.hero p { margin: 0 auto; max-width: 54ch; font-size: 17px; color: var(--on-surface-variant); }
.feature-grid { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 16px; margin-top: 28px; }
.feature { display: flex; flex-direction: column; align-items: flex-start; gap: 14px; min-width: 0;
  padding: 26px; border-radius: 28px; background: var(--sc-low); border: 1px solid var(--outline-variant); }
.feature .ic { width: 52px; height: 52px; border-radius: 17px; }
.feature .ic svg { width: 26px; height: 26px; }
.feature h3 { margin: 0 0 8px; font-size: 17px; font-weight: 600; letter-spacing: -.1px; }
.feature p { margin: 0; color: var(--on-surface-variant); font-size: 13.5px; line-height: 1.6; }
@media (max-width: 1100px) { .feature-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
@media (max-width: 700px) { .feature-grid { grid-template-columns: minmax(0, 1fr); } }

/* ---- centred auth pages ---- */
.auth { min-height: 100vh; display: grid; place-items: center; padding: 24px; position: relative;
  overflow: hidden; }
.auth .blob { position: absolute; border-radius: 50%; filter: blur(90px); opacity: .45; pointer-events: none; }
.auth .b1 { width: 420px; height: 420px; background: var(--pink-bg); top: -140px; left: -120px; }
.auth .b2 { width: 380px; height: 380px; background: var(--blue-bg); bottom: -140px; right: -100px; }
.auth .b3 { width: 300px; height: 300px; background: var(--purple-bg); bottom: -80px; left: 18%; opacity: .35; }
/* The appearance switcher on a page with no bar to put it in. Above the blobs,
   which are `pointer-events: none` but painted later in the source. */
.auth-top { position: absolute; top: 16px; right: 16px; z-index: 2; }
.auth-box { width: min(420px, 100%); position: relative; animation: rise .5s var(--ease) both; }
.auth-logo { width: 60px; height: 60px; border-radius: 20px; background: var(--primary-container);
  color: var(--on-primary-container); display: grid; place-items: center; margin: 0 auto 20px; }
.auth-logo svg { width: 38px; height: 38px; }
.auth-box h1 { text-align: center; font-size: 26px; font-weight: 600; margin: 0 0 6px; }
.auth-box .sub { text-align: center; color: var(--on-surface-variant); font-size: 14px; margin: 0 0 24px; }
.auth-box .card { padding: 26px; }
.auth-box .btn { width: 100%; height: 46px; font-size: 15px; margin-top: 4px; }
.auth-foot { margin: 20px 0 0; text-align: center; font-size: 13.5px; color: var(--on-surface-variant); }
.auth-foot a { color: var(--primary); font-weight: 600; text-decoration: none; }
.auth-foot a:hover { text-decoration: underline; }

/* ---- a top bar, for the pages with no rail ---- */
.topbar { display: flex; align-items: center; gap: 16px; padding: 14px 24px; flex-wrap: wrap;
  border-bottom: 1px solid color-mix(in srgb, var(--outline-variant) 55%, transparent); }
.topbar .brand { min-height: 0; padding: 4px 8px; }
.topbar .brand .mark svg { width: 28px; height: 28px; }
.topbar .brand b { font-size: 17px; }
.topbar nav { margin-left: auto; display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.topbar nav a { text-decoration: none; color: var(--on-surface-variant); font-size: 13.5px;
  font-weight: 500; padding: 8px 14px; border-radius: 999px; }
.topbar nav a:hover { background: color-mix(in srgb, var(--on-surface) 6%, transparent); color: var(--on-surface); }
.topbar nav a.cta { background: var(--primary); color: var(--on-primary); font-weight: 600; }
.topbar nav a.cta:hover { box-shadow: var(--shadow-1); background: var(--primary); }

/* ---- footnote and pager ---- */
.footnote { color: var(--on-surface-variant); font-size: 13px; margin: 28px 0 0; line-height: 1.6; }
.pager { display: flex; align-items: center; gap: 12px; margin: 16px 0 4px; font-size: 13px;
  color: var(--on-surface-variant); }

/* ---- entrance motion ---- */
@keyframes rise { from { opacity: 0; transform: translateY(14px); } }
.rise { animation: rise .45s var(--ease) both; animation-delay: calc(var(--i, 0) * 70ms); }
@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after { animation: none !important; transition: none !important; }
}

/* ---- responsive: the rail becomes a top strip ---- */
@media (max-width: 919px) {
  .shell { flex-direction: column; }
  .rail { width: 100%; height: auto; position: static; flex-direction: column; overflow: visible;
    padding-bottom: 0; border-right: none;
    border-bottom: 1px solid color-mix(in srgb, var(--outline-variant) 55%, transparent); }
  .brand { padding: 14px 8px 10px; min-height: 0; }
  .rail .nav-scroll { display: flex; gap: 4px; overflow-x: auto; padding: 4px 8px 12px;
    scrollbar-width: none; scroll-snap-type: x proximity; scroll-padding-inline: 8px; }
  .rail .nav-scroll::-webkit-scrollbar { display: none; }
  .nav-item { flex: none; padding: 8px 14px 8px 10px; scroll-snap-align: start; }
  .nav-item .ic { display: none; }
  .rail-foot { margin: 0; padding: 0 8px 10px; }
  /* The rail's appearance menu opens upward, which is right when the rail is a
     tall column whose foot sits at the bottom of the screen. As a top strip the
     same rule would throw the popup off the top of the page, so the strip goes
     back to the shared default: anchored to the button, opening downward. */
  .rail .theme-menu { position: relative; }
  .rail .theme-menu .pop { top: calc(100% + 10px); bottom: auto; left: auto; right: 0; width: 220px; }
  .content { padding: 0 16px 48px; }
  .page-head { flex-wrap: wrap; }
  .grid2 { grid-template-columns: 1fr; }
}
@media (max-width: 639px) {
  .tiles, .tiles.c3, .tiles.c2 { grid-template-columns: 1fr; }
  .page-head { padding: 16px 2px 12px; }
  .page-head .actions { margin-left: 0; width: 100%; justify-content: flex-start; }
  .rows .row { flex-wrap: wrap; gap: 10px 12px; padding: 12px 16px; align-items: flex-start; }
  .rows .row .body { flex: 1 1 calc(100% - 52px); min-width: 0; order: 2; }
  .rows .row > .ic, .rows .row > .avatar { order: 1; margin-top: 1px; }
  .rows .row .title, .rows .row .detail { white-space: normal; overflow-wrap: anywhere; }
  .trailing { order: 3; flex: 1 1 100%; margin-left: 0; padding-left: 52px; justify-content: flex-start; }
  .rows .row:not(:has(> .ic)):not(:has(> .avatar)) .trailing { padding-left: 0; }
  .rows .row + .row::before { left: 20px; }
  .confirm .pop { right: auto; left: 0; }
  .plans { grid-template-columns: 1fr; }
}
"#;

/// The stylesheet, inlined.
///
/// One request, no build step, no cache-busting to get wrong. Built once and
/// cached because the dark tokens are emitted twice — for the `dark` class and
/// again inside a `prefers-color-scheme` query for `system`.
pub fn style() -> &'static str {
    static STYLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STYLE.get_or_init(|| {
        format!(
            "{BASE}\nhtml.dark {{{DARK}}}\n@media (prefers-color-scheme: dark) {{ html.system {{{DARK}}} }}\n"
        )
    })
}

// ---------------------------------------------------------------------------
// The page shell
// ---------------------------------------------------------------------------

/// A navigation entry in the rail.
pub struct NavItem {
    pub label: &'static str,
    pub href: String,
    /// A key from [`icon`].
    pub icon: &'static str,
    /// One of the `tint-*` classes.
    pub tint: &'static str,
    pub active: bool,
    /// Opens in a new tab — for "View site" and the like.
    pub external: bool,
}

impl NavItem {
    pub fn new(label: &'static str, href: impl Into<String>, icon: &'static str, tint: &'static str, active: bool) -> Self {
        Self { label, href: href.into(), icon, tint, active, external: false }
    }

    pub fn external(mut self) -> Self {
        self.external = true;
        self
    }
}

/// The signed-in identity shown in the rail's foot.
pub struct Who {
    pub name: String,
    pub detail: String,
    /// Where the block links, if anywhere.
    pub href: Option<String>,
    /// Where the sign-out button posts to.
    pub signout: String,
}

/// Everything a full page needs.
///
/// A struct rather than nine positional arguments: the panels differ in a
/// handful of these and a mis-ordered pair of `&str`s is not a compile error.
pub struct Page<'a> {
    pub title: &'a str,
    pub platform: &'a str,
    /// What distinguishes the two panels at a glance. The operator console says
    /// so in its rail, because an operator with both open needs to know which
    /// one they are about to click Suspend in.
    pub tag: Option<&'a str>,
    pub brand_href: &'a str,
    pub nav: &'a [NavItem],
    pub who: Option<&'a Who>,
    pub flash: Option<&'a Flash>,
    pub theme: Theme,
    pub body: &'a str,
}

impl<'a> Page<'a> {
    pub fn new(title: &'a str, platform: &'a str, theme: Theme, body: &'a str) -> Self {
        Self {
            title,
            platform,
            tag: None,
            brand_href: "/",
            nav: &[],
            who: None,
            flash: None,
            theme,
            body,
        }
    }
}

/// The brand lockup: Ghost's own mark, the platform's name, and the panel tag.
fn brand(platform: &str, tag: Option<&str>, href: &str) -> String {
    format!(
        r#"<a class="brand" href="{href}"><span class="mark">{mark}</span>
<span class="brand-name"><b>{platform}</b>{tag}</span></a>"#,
        href = attr(href),
        mark = svg("ghost"),
        platform = esc(platform),
        tag = tag.map(|t| format!(r#"<span class="tag">{}</span>"#, esc(t))).unwrap_or_default(),
    )
}

/// The appearance menu: Dark, Light, System, each a one-button form.
///
/// `next` is where the browser comes back to, so the choice does not cost the
/// page you were reading. It is a path from this application, never anything a
/// visitor supplied — see `crate::public::appearance`, which validates it
/// again on the way in.
fn appearance(current: Theme, next: &str) -> String {
    let options: String = [
        (Theme::Dark, "Dark", "moon"),
        (Theme::Light, "Light", "sun"),
        (Theme::System, "System", "monitor"),
    ]
    .iter()
    .map(|(value, label, glyph)| {
        format!(
            r#"<form method="post" action="/appearance">
<input type="hidden" name="theme" value="{value}">
<input type="hidden" name="next" value="{next}">
<button type="submit" class="menu-item{on}">{glyph} {label}<span class="ck">{check}</span></button>
</form>"#,
            value = value.as_str(),
            next = attr(next),
            on = if *value == current { " on" } else { "" },
            glyph = svg_stroke(glyph),
            label = label,
            check = svg("check"),
        )
    })
    .collect();

    format!(
        r#"<details class="theme-menu">
<summary class="icon-btn" title="Appearance" aria-label="Appearance">{sun}{moon}</summary>
<div class="pop"><p class="pop-title">Appearance</p>{options}</div>
</details>"#,
        sun = svg_stroke("sun"),
        moon = svg_stroke("moon"),
        options = options,
    )
}

/// The side navigation.
fn rail(page: &Page) -> String {
    let links: String = page
        .nav
        .iter()
        .map(|item| {
            format!(
                r#"<a class="nav-item{cls}" href="{href}"{target}><span class="ic tint-{tint}">{svg}</span><span class="nav-label">{label}</span></a>"#,
                cls = if item.active { " active" } else { "" },
                href = attr(&item.href),
                target = if item.external { r#" target="_blank" rel="noopener""# } else { "" },
                tint = item.tint,
                svg = svg(item.icon),
                label = esc(item.label),
            )
        })
        .collect();

    // The appearance menu comes back to the page it was opened from; with no
    // rail-bearing page that is unknowable, so the brand's own href stands in.
    let foot = match page.who {
        Some(who) => {
            let initial = who
                .name
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "?".into());
            let inner = format!(
                r#"<span class="avatar">{initial}</span><div><b>{name}</b><span>{detail}</span></div>"#,
                initial = esc(&initial),
                name = esc(&who.name),
                detail = esc(&who.detail),
            );
            // The rail is narrow enough that a full name or address is usually
            // ellipsed, so the tooltip carries them rather than restating what
            // the link obviously is.
            let block = match &who.href {
                Some(href) => format!(
                    r#"<a class="who-link" href="{href}" title="{name} · {detail}">{inner}</a>"#,
                    href = attr(href),
                    name = attr(&who.name),
                    detail = attr(&who.detail),
                    inner = inner,
                ),
                None => format!(r#"<div class="who-link">{inner}</div>"#),
            };
            format!(
                r#"<div class="rail-foot"><div class="who">{block}
{appearance}
<form method="post" action="{signout}" class="inline">
<button class="icon-btn" type="submit" title="Sign out" aria-label="Sign out">{logout}</button></form>
</div></div>"#,
                block = block,
                appearance = appearance(page.theme, page.brand_href),
                signout = attr(&who.signout),
                logout = svg("logout"),
            )
        }
        None => format!(
            r#"<div class="rail-foot"><div class="who">{appearance}</div></div>"#,
            appearance = appearance(page.theme, page.brand_href)
        ),
    };

    format!(
        r#"<aside class="rail">{brand}<div class="nav-scroll">{links}</div>{foot}</aside>"#,
        brand = brand(page.platform, page.tag, page.brand_href),
        links = links,
        foot = foot,
    )
}

/// The document around a body.
fn document(title: &str, platform: &str, theme: Theme, inner: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en"{class} data-theme="{theme}"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title} · {platform}</title>
<meta name="robots" content="noindex">
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='%23984059'%3E%3Cpath d='M12 2a8 8 0 0 0-8 8v10.6c0 .7.85 1.05 1.35.55l1.9-1.9 2.2 2.2c.3.3.78.3 1.08 0L12 20l1.47 1.45c.3.3.78.3 1.08 0l2.2-2.2 1.9 1.9c.5.5 1.35.15 1.35-.55V10a8 8 0 0 0-8-8z'/%3E%3C/svg%3E">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Outfit:wght@400;500;600;700&amp;family=Fraunces:opsz,wght@9..144,500..700&amp;family=Roboto+Mono:wght@400;500&amp;display=swap">
<style>{style}</style>
</head><body>{inner}</body></html>"#,
        class = theme.html_class(),
        theme = theme.as_str(),
        title = esc(title),
        platform = esc(platform),
        style = style(),
        inner = inner,
    )
}

/// A page with the navigation rail.
pub fn page(p: Page) -> String {
    let inner = format!(
        r#"<div class="shell">{rail}<main class="main"><div class="content">{flash}{body}</div></main></div>"#,
        rail = rail(&p),
        flash = p.flash.map(flash_banner).unwrap_or_default(),
        body = p.body,
    );
    document(p.title, p.platform, p.theme, &inner)
}

/// A page with a top bar instead of a rail — the public, signed-out pages.
pub fn public_page(p: Page) -> String {
    let nav: String = p
        .nav
        .iter()
        .map(|item| {
            format!(
                r#"<a href="{href}"{cls}>{label}</a>"#,
                href = attr(&item.href),
                // The last entry on a marketing page is the call to action;
                // `active` is what a caller sets to say so.
                cls = if item.active { r#" class="cta""# } else { "" },
                label = esc(item.label),
            )
        })
        .collect();

    let inner = format!(
        r#"<header class="topbar">{brand}<nav>{nav}</nav>{appearance}</header>
<main class="main"><div class="content">{flash}{body}</div></main>"#,
        brand = brand(p.platform, p.tag, p.brand_href),
        nav = nav,
        appearance = appearance(p.theme, p.brand_href),
        flash = p.flash.map(flash_banner).unwrap_or_default(),
        body = p.body,
    );
    document(p.title, p.platform, p.theme, &inner)
}

/// A centred, chrome-free page: sign-in, sign-up, reset, and the error pages.
///
/// `heading` and `sub` sit above the card, as Ghost's own auth screens do;
/// `body` is the card's contents.
///
/// Chrome-free does not mean appearance-free: these are the pages a signed-out
/// visitor spends the longest on, so the switcher rides in the top corner. It
/// posts no `next` of its own — `crate::public::appearance` reads the page to
/// come back to off the `Referer`, which is what keeps a half-typed sign-in
/// form on screen when somebody flips to dark.
pub fn auth_page(
    title: &str,
    platform: &str,
    theme: Theme,
    heading: &str,
    sub: &str,
    flash: Option<&Flash>,
    card: &str,
    foot: &str,
) -> String {
    let inner = format!(
        r#"<div class="auth">
<span class="blob b1"></span><span class="blob b2"></span><span class="blob b3"></span>
<div class="auth-top">{appearance}</div>
<div class="auth-box">
<div class="auth-logo">{mark}</div>
<h1>{heading}</h1>
<p class="sub">{sub}</p>
{flash}
<div class="card padded">{card}</div>
{foot}
</div></div>"#,
        appearance = appearance(theme, ""),
        mark = svg("ghost"),
        heading = esc(heading),
        sub = esc(sub),
        flash = flash.map(flash_banner).unwrap_or_default(),
        card = card,
        foot = foot,
    );
    document(title, platform, theme, &inner)
}

// ---------------------------------------------------------------------------
// Flash messages
// ---------------------------------------------------------------------------

/// A message carried across a redirect.
///
/// In the query string rather than a cookie: these are short, never secret,
/// and a cookie-based flash needs either a session for anonymous visitors or
/// server-side storage, both of which are more machinery than a sentence
/// deserves. The text is always escaped on the way out, and the kind is the
/// only other thing a URL can influence.
pub struct Flash {
    pub message: String,
    pub kind: FlashKind,
}

/// Three outcomes, because two were not enough.
///
/// [`FlashKind::Warn`] is for an action that *did* what was asked and still
/// left something wrong — saving an SMTP configuration that cannot send is the
/// case it exists for. Reporting that as success is how an operator comes to
/// believe mail works; reporting it as an error claims nothing was saved, and
/// invites them to submit the form again.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FlashKind {
    Ok,
    Warn,
    Err,
}

impl Flash {
    pub fn ok(message: impl Into<String>) -> Self {
        Self { message: message.into(), kind: FlashKind::Ok }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Self { message: message.into(), kind: FlashKind::Err }
    }
    /// Saved, but not working yet — and the message says which.
    pub fn warn(message: impl Into<String>) -> Self {
        Self { message: message.into(), kind: FlashKind::Warn }
    }

    /// Whether this reports success. Warnings do not: something was saved, but
    /// the operator still has a thing to fix.
    pub fn is_ok(&self) -> bool {
        self.kind == FlashKind::Ok
    }

    /// Read a flash out of `?ok=…` / `?warn=…` / `?err=…`.
    pub fn from_query(query: &std::collections::HashMap<String, String>) -> Option<Self> {
        if let Some(m) = query.get("ok") {
            return Some(Flash::ok(m.clone()));
        }
        if let Some(m) = query.get("warn") {
            return Some(Flash::warn(m.clone()));
        }
        query.get("err").map(|m| Flash::err(m.clone()))
    }

    /// Append this flash to a redirect target.
    pub fn to_query(&self) -> String {
        let key = match self.kind {
            FlashKind::Ok => "ok",
            FlashKind::Warn => "warn",
            FlashKind::Err => "err",
        };
        format!("{key}={}", urlencode(&self.message))
    }
}

fn flash_banner(flash: &Flash) -> String {
    banner(
        match flash.kind {
            FlashKind::Ok => BannerKind::Ok,
            FlashKind::Warn => BannerKind::Warn,
            FlashKind::Err => BannerKind::Error,
        },
        &esc(&flash.message),
    )
}

/// What a banner is saying.
#[derive(Clone, Copy)]
pub enum BannerKind {
    Ok,
    Error,
    Warn,
    Info,
}

/// A banner. `message` is **already escaped** — banners carry links often
/// enough that escaping here would make half of them unusable, so the call
/// site escapes its own values.
pub fn banner(kind: BannerKind, message: &str) -> String {
    let (class, glyph) = match kind {
        BannerKind::Ok => ("ok", "check"),
        BannerKind::Error => ("error", "warn"),
        BannerKind::Warn => ("warn", "warn"),
        BannerKind::Info => ("info", "info"),
    };
    format!(
        r#"<div class="banner {class}">{glyph}<div class="msg">{message}</div></div>"#,
        glyph = svg(glyph),
    )
}

// ---------------------------------------------------------------------------
// Chips
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum PillKind {
    Ok,
    Warn,
    Bad,
    Idle,
    Plain,
}

/// A status chip, coloured by meaning.
pub fn status_pill(label: &str, kind: PillKind) -> String {
    let class = match kind {
        PillKind::Ok => "chip ok",
        PillKind::Warn => "chip warn",
        PillKind::Bad => "chip bad",
        PillKind::Idle => "chip idle",
        PillKind::Plain => "chip",
    };
    format!(r#"<span class="{class}">{}</span>"#, esc(label))
}

/// The chip for a tenant's lifecycle state.
pub fn tenant_pill(status: pro_core::TenantStatus) -> String {
    use pro_core::TenantStatus as S;
    let kind = match status {
        S::Active => PillKind::Ok,
        S::Provisioning => PillKind::Warn,
        S::Suspended => PillKind::Bad,
        S::PendingDeletion => PillKind::Warn,
        S::Failed => PillKind::Bad,
        S::Deleted => PillKind::Idle,
    };
    status_pill(status.label(), kind)
}

/// The chip for a domain's verification state.
pub fn domain_pill(status: pro_core::DomainStatus) -> String {
    use pro_core::DomainStatus as D;
    let kind = match status {
        D::Active => PillKind::Ok,
        D::Verified => PillKind::Warn,
        D::Pending => PillKind::Warn,
        D::Failed => PillKind::Bad,
    };
    status_pill(status.label(), kind)
}

// ---------------------------------------------------------------------------
// Page furniture
// ---------------------------------------------------------------------------

/// The sticky per-page app bar. `sub` and `actions` are **already escaped** —
/// both routinely carry links and chips.
pub fn page_head(title: &str, sub: &str, actions: &str) -> String {
    format!(
        r#"<div class="page-head editorial"><div class="heading"><h1>{title}</h1>{sub}</div>
{actions}</div>"#,
        title = esc(title),
        sub = if sub.is_empty() {
            String::new()
        } else {
            format!(r#"<p class="sub">{sub}</p>"#)
        },
        actions = if actions.is_empty() {
            String::new()
        } else {
            format!(r#"<div class="actions">{actions}</div>"#)
        },
    )
}

/// A group heading between cards.
pub fn section(title: &str) -> String {
    format!(r#"<p class="section">{}</p>"#, esc(title))
}

/// A card. `body` is trusted — it is assembled by these same functions.
pub fn card(body: &str) -> String {
    format!(r#"<div class="card padded">{body}</div>"#)
}

/// A card with no padding of its own, for a table or a list of rows.
pub fn card_flush(body: &str) -> String {
    format!(r#"<div class="card">{body}</div>"#)
}

/// One list row: a tinted leading icon, a title over a detail line, and
/// whatever trails it. `detail` and `trailing` are already escaped.
pub fn row(icon_name: &str, tint: &str, title: &str, detail: &str, trailing: &str) -> String {
    format!(
        r#"<div class="row"><span class="ic tint-{tint}">{icon}</span>
<div class="body"><span class="title">{title}</span><span class="detail">{detail}</span></div>
{trailing}</div>"#,
        tint = tint,
        icon = svg(icon_name),
        title = esc(title),
        detail = detail,
        trailing = if trailing.is_empty() {
            String::new()
        } else {
            format!(r#"<div class="trailing">{trailing}</div>"#)
        },
    )
}

/// A list of rows, in a card.
pub fn rows(body: &str) -> String {
    format!(r#"<div class="card"><div class="rows settings">{body}</div></div>"#)
}

/// A table, in a card. `head` cells may carry a `num`/`end` class by prefixing
/// the label with `num:` or `end:`; `body` is the already-rendered `<tr>`s.
pub fn table(head: &[&str], body: &str) -> String {
    let header: String = head
        .iter()
        .map(|label| {
            let (class, text) = match label.split_once(':') {
                Some((c, rest)) if c == "num" || c == "end" => (format!(r#" class="{c}""#), rest),
                _ => (String::new(), *label),
            };
            format!("<th{class}>{}</th>", esc(text))
        })
        .collect();
    format!(
        r#"<div class="card"><div class="tbl-wrap"><table class="tbl">
<thead><tr>{header}</tr></thead><tbody>{body}</tbody></table></div></div>"#
    )
}

/// The row a table shows when it has none of its own.
pub fn empty_row(columns: usize, message: &str) -> String {
    format!(
        r#"<tr><td class="empty-cell" colspan="{columns}">{}</td></tr>"#,
        esc(message)
    )
}

/// An empty-state block.
pub fn empty(heading: &str, body: &str, cta: Option<(&str, &str)>) -> String {
    let button = cta
        .map(|(label, href)| {
            format!(
                r#"<a class="btn" href="{}">{}{}</a>"#,
                attr(href),
                svg("add"),
                esc(label)
            )
        })
        .unwrap_or_default();
    format!(
        r#"<div class="card"><div class="empty"><span class="ic">{glyph}</span>
<h2>{heading}</h2><p>{body}</p>{button}</div></div>"#,
        glyph = svg("ghost"),
        heading = esc(heading),
        body = esc(body),
    )
}

/// A stat tile.
pub fn metric(label: &str, value: &str, note: Option<&str>) -> String {
    format!(
        r#"<div class="tile"><div class="label">{label}</div><div class="value">{value}</div>{note}</div>"#,
        label = esc(label),
        value = esc(value),
        note = note
            .map(|n| format!(r#"<div class="delta">{}</div>"#, esc(n)))
            .unwrap_or_default(),
    )
}

/// A usage meter. `used`/`limit`, with `None` meaning unlimited.
pub fn meter(label: &str, used: i64, limit: Option<i64>, render: fn(i64) -> String) -> String {
    let (value, bar) = match limit {
        Some(max) if max > 0 => {
            let pct = ((used as f64 / max as f64) * 100.0).clamp(0.0, 100.0);
            (
                format!("{} <span class=\"unit\">of {}</span>", render(used), render(max)),
                format!(
                    r#"<div class="bar{}"><span style="width:{pct:.0}%"></span></div>"#,
                    if pct >= 100.0 { " full" } else { "" }
                ),
            )
        }
        // A zero ceiling is a withheld feature, not a meter worth drawing.
        Some(_) => (
            format!("{} <span class=\"unit\">not included</span>", render(used)),
            String::new(),
        ),
        None => (
            format!("{} <span class=\"unit\">unlimited</span>", render(used)),
            String::new(),
        ),
    };
    format!(
        r#"<div class="tile"><div class="label">{}</div><div class="value">{}</div>{}</div>"#,
        esc(label),
        value,
        bar
    )
}

/// A plain number, with thousands separators.
pub fn number(n: i64) -> String {
    let negative = n < 0;
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if negative {
        format!("-{out}")
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

/// A `<form method=post>` wrapper.
///
/// Everything that changes state is a POST form rather than a link, so that
/// nothing destructive sits behind a URL a browser might prefetch or a crawler
/// might follow.
pub fn form(action: &str, body: &str) -> String {
    format!(r#"<form method="post" action="{}">{body}</form>"#, attr(action))
}

/// A one-button POST form, for an action with no other input.
///
/// `confirm` is `"Question|Explanation"`, and turns the button into the
/// `<details>` two-step described at the top of this module: no script, so it
/// still works under `default-src 'none'`.
pub fn action_button(action: &str, label: &str, class: &str, confirm: Option<&str>) -> String {
    let inner = match confirm {
        Some(text) => {
            let (question, explanation) = match text.split_once('|') {
                Some((q, e)) => (q, e),
                None => (text, "This cannot be undone."),
            };
            format!(
                r#"<details class="confirm"><summary class="btn {class}">{label}</summary>
<div class="pop"><p class="pop-title">{question}</p><p>{explanation}</p>
<div><button class="btn {class}" type="submit">{label}</button></div></div></details>"#,
                class = class,
                label = esc(label),
                question = esc(question),
                explanation = esc(explanation),
            )
        }
        None => format!(
            r#"<button class="btn {class}" type="submit">{}</button>"#,
            esc(label)
        ),
    };
    format!(
        r#"<form method="post" action="{}" class="inline">{inner}</form>"#,
        attr(action)
    )
}

/// An M3 outlined text field with a floating label.
///
/// The `placeholder=" "` is not decoration: it is what drives
/// `:placeholder-shown`, which is how the label knows whether to float.
pub fn field(
    name: &str,
    label: &str,
    kind: &str,
    value: &str,
    help: Option<&str>,
    required: bool,
) -> String {
    format!(
        r#"<label class="field" for="{id}">
<input type="{kind}" id="{id}" name="{name}" value="{value}" placeholder=" "{req}>
<span>{label}</span></label>{help}"#,
        id = attr(name),
        name = attr(name),
        kind = attr(kind),
        label = esc(label),
        value = attr(value),
        req = flag("required", required),
        help = help
            .map(|h| format!(r#"<p class="hint">{}</p>"#, esc(h)))
            .unwrap_or_default(),
    )
}

/// The same, with every attribute a caller might need.
pub fn field_full(
    name: &str,
    label: &str,
    kind: &str,
    value: &str,
    help: Option<&str>,
    extra: &str,
) -> String {
    format!(
        r#"<label class="field" for="{id}">
<input type="{kind}" id="{id}" name="{name}" value="{value}" placeholder=" "{extra}>
<span>{label}</span></label>{help}"#,
        id = attr(name),
        name = attr(name),
        kind = attr(kind),
        label = esc(label),
        value = attr(value),
        extra = extra,
        help = help
            .map(|h| format!(r#"<p class="hint">{}</p>"#, esc(h)))
            .unwrap_or_default(),
    )
}

/// A select, in the same outlined box. `options` is already-rendered
/// `<option>` markup.
pub fn select(
    name: &str,
    label: &str,
    options: &str,
    help: Option<&str>,
    disabled: bool,
) -> String {
    format!(
        r#"<label class="field fixed" for="{id}">
<select id="{id}" name="{name}"{dis}>{options}</select>
<span>{label}</span><span class="car">{car}</span></label>{help}"#,
        id = attr(name),
        name = attr(name),
        label = esc(label),
        options = options,
        dis = flag("disabled", disabled),
        car = svg("down"),
        help = help
            .map(|h| format!(r#"<p class="hint">{}</p>"#, esc(h)))
            .unwrap_or_default(),
    )
}

/// A compact select for a settings row's trailing slot — the value alone, with
/// the row's own text acting as its label.
pub fn selbox(name: &str, options: &str, disabled: bool, label: &str) -> String {
    format!(
        r#"<span class="selbox"><select name="{name}"{dis} aria-label="{label}">{options}</select>
<span class="car">{car}</span></span>"#,
        name = attr(name),
        dis = flag("disabled", disabled),
        label = attr(label),
        options = options,
        car = svg("down"),
    )
}

/// An M3 switch.
pub fn switch(name: &str, on: bool, disabled: bool, label: &str) -> String {
    format!(
        r#"<label class="switch"><input type="checkbox" name="{name}" value="1"{on}{dis} aria-label="{label}">
<span class="track"><span class="thumb"></span></span></label>"#,
        name = attr(name),
        on = flag("checked", on),
        dis = flag("disabled", disabled),
        label = attr(label),
    )
}

/// A row of filter links. Each item is `(href, label, count, active)`.
pub fn seg_bar(items: &[(String, String, Option<i64>, bool)]) -> String {
    let links: String = items
        .iter()
        .map(|(href, label, count, active)| {
            format!(
                r#"<a class="seg{on}" href="{href}">{label}{count}</a>"#,
                on = if *active { " on" } else { "" },
                href = attr(href),
                label = esc(label),
                count = count
                    .map(|n| format!(r#"<span class="seg-count">{}</span>"#, number(n)))
                    .unwrap_or_default(),
            )
        })
        .collect();
    format!(r#"<div class="seg-bar">{links}</div>"#)
}

/// A search bar that submits itself.
pub fn search_form(action: &str, value: &str, placeholder: &str) -> String {
    format!(
        r#"<form method="get" action="{action}" class="search" role="search">{glyph}
<input type="search" name="q" value="{value}" placeholder="{placeholder}" aria-label="{placeholder}">
<button class="btn tonal" type="submit">Search</button></form>"#,
        action = attr(action),
        glyph = svg("search"),
        value = attr(value),
        placeholder = attr(placeholder),
    )
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// Render a timestamp as something a person reads.
///
/// Relative for anything recent, because "3 minutes ago" is what a reader of
/// an operator console actually wants from a timestamp, and absolute beyond a
/// week where the exact date starts to matter more than the distance.
pub fn when(timestamp: Option<&str>) -> String {
    let Some(parsed) = timestamp.and_then(pro_core::parse_iso) else {
        return "—".into();
    };
    let delta = chrono::Utc::now().signed_duration_since(parsed);
    let secs = delta.num_seconds();
    if secs < 0 {
        // A future timestamp: a trial end or a scheduled deletion.
        let ahead = -secs;
        return match ahead {
            s if s < 3600 => format!("in {} min", (s / 60).max(1)),
            s if s < 86_400 => format!("in {} h", s / 3600),
            s => format!("in {} days", s / 86_400),
        };
    }
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s if s < 604_800 => format!("{} days ago", s / 86_400),
        _ => parsed.format("%-d %b %Y").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule the whole module rests on. A customer controls their site
    /// title; an operator reading the console must not be attackable by it.
    #[test]
    fn escapes_every_dangerous_character() {
        assert_eq!(
            esc("<script>alert('x')</script>"),
            "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"
        );
        assert_eq!(esc(r#"a"b"#), "a&quot;b");
        assert_eq!(esc("a&b"), "a&amp;b");
        // Ampersand first, or an escape would be double-escaped into a
        // literal `&amp;lt;`.
        assert_eq!(esc("&lt;"), "&amp;lt;");
        assert_eq!(esc("plain text"), "plain text");
        assert_eq!(esc(""), "");
    }

    /// A hostile site title must not break out of an attribute.
    #[test]
    fn attribute_injection_is_closed() {
        let hostile = r#"" onmouseover="steal()"#;
        let html = field("title", "Title", "text", hostile, None, false);
        assert!(!html.contains(r#"onmouseover="steal()"#), "got {html}");
        assert!(html.contains("&quot;"));
    }

    fn sample_page<'a>(body: &'a str, flash: Option<&'a Flash>, nav: &'a [NavItem]) -> Page<'a> {
        Page {
            title: "<title>",
            platform: "<platform>",
            tag: Some("<tag>"),
            brand_href: "/",
            nav,
            who: None,
            flash,
            theme: Theme::Light,
            body,
        }
    }

    /// A tenant name goes into a page unescaped nowhere.
    #[test]
    fn page_escapes_its_inputs() {
        let nav = [NavItem::new("Sites", "/x?a=1&b=2", "sites", "blue", true)];
        let flash = Flash::err("<bad>");
        let html = page(sample_page("<p>body passes through</p>", Some(&flash), &nav));
        // Both the page title and the platform name arrive escaped inside the
        // real <title> element, so a hostile value cannot close it and open a
        // tag of its own.
        assert!(
            html.contains("<title>&lt;title&gt; · &lt;platform&gt;</title>"),
            "got {html}"
        );
        assert!(html.contains("&lt;platform&gt;"));
        assert!(html.contains("&lt;tag&gt;"));
        assert!(html.contains("&lt;bad&gt;"));
        assert!(html.contains("/x?a=1&amp;b=2"));
        // The body is trusted: it is assembled by these same functions.
        assert!(html.contains("<p>body passes through</p>"));
    }

    /// The rail is what the Material 3 shell is, so a nav page must have one.
    #[test]
    fn a_nav_page_renders_the_rail() {
        let nav = [
            NavItem::new("Sites", "/dashboard", "sites", "blue", true),
            NavItem::new("Billing", "/dashboard/billing", "billing", "green", false),
        ];
        let html = page(sample_page("", None, &nav));
        assert!(html.contains(r#"<aside class="rail">"#));
        assert!(html.contains(r#"class="nav-item active""#));
        assert!(html.contains(r#"class="ic tint-blue""#));
        assert!(html.contains(r#"class="ic tint-green""#));
        // The appearance switcher is three forms, not a script.
        assert!(html.contains(r#"action="/appearance""#));
        assert!(!html.to_lowercase().contains("<script"));
    }

    /// Every page carries the M3 tokens and both colour schemes.
    #[test]
    fn the_stylesheet_carries_both_schemes() {
        let css = style();
        assert!(css.contains("--primary: #984059"), "the light scheme's accent");
        assert!(css.contains("html.dark"), "a forced dark choice");
        assert!(
            css.contains("prefers-color-scheme: dark") && css.contains("html.system"),
            "a `system` choice resolves in CSS, since the server cannot"
        );
        // Called twice, built once.
        assert!(std::ptr::eq(style(), style()));
    }

    #[test]
    fn themes_round_trip() {
        assert_eq!(Theme::parse("dark"), Some(Theme::Dark));
        assert_eq!(Theme::parse("system"), Some(Theme::System));
        assert_eq!(Theme::parse("light"), Some(Theme::Light));
        assert_eq!(Theme::parse("neon"), None);
        assert_eq!(Theme::default(), Theme::Light);
        // Light is the stylesheet's own :root, so it needs no class.
        assert_eq!(Theme::Light.html_class(), "");
        assert!(Theme::Dark.html_class().contains("dark"));
    }

    /// A dark page must render dark from the first paint — no light flash,
    /// which without script means the class has to be on `<html>` already.
    #[test]
    fn a_dark_choice_is_server_rendered() {
        let mut p = sample_page("", None, &[]);
        p.theme = Theme::Dark;
        let html = page(p);
        assert!(html.contains(r#"<html lang="en" class="dark" data-theme="dark">"#), "got {html}");
    }

    /// The switcher must be *on* every surface, not just the panels. A visitor
    /// who has not signed in only ever sees the public pages and the auth
    /// pages, so a control missing from those is a control they do not have.
    #[test]
    fn every_surface_carries_the_switcher() {
        let rail = page(sample_page("", None, &[]));
        let public = public_page(sample_page("", None, &[]));
        let centred = auth_page("Sign in", "Pro", Theme::Light, "Sign in", "", None, "", "");
        for (surface, html) in [("rail", &rail), ("public", &public), ("auth", &centred)] {
            assert!(
                html.contains(r#"<details class="theme-menu">"#),
                "the {surface} surface has no appearance switcher"
            );
            // All three choices, or it is not a switcher.
            for choice in [r#"value="dark""#, r#"value="light""#, r#"value="system""#] {
                assert!(html.contains(choice), "the {surface} switcher is missing {choice}");
            }
        }
    }

    /// The popover is `position: absolute`, so it needs a positioned ancestor.
    /// Without one it resolves against the initial containing block, and the
    /// rail's `bottom: 100%` then puts it off the top of the page — which is
    /// exactly how the public pages' switcher became unclickable. The anchor
    /// and the direction are both asserted, because either one alone is enough
    /// to lose the menu.
    #[test]
    fn the_popover_is_anchored_to_the_switcher_and_opens_into_the_page() {
        let css = style();

        // An anchor on the control itself, for every placement but the rail's.
        assert!(css.contains(".theme-menu { position: relative; }"), "no positioning context");
        // And downward, because every such placement sits at the top of the page.
        let default_pop = declarations(css, ".theme-menu .pop {");
        assert!(
            default_pop.contains("top: calc(100% + 10px)") && default_pop.contains("bottom: auto"),
            "the default popover must open downward, got: {default_pop}"
        );

        // The rail is the exception: it anchors to the foot row and opens
        // upward, out of the bottom of a tall column.
        assert!(css.contains(".rail .theme-menu { position: static; }"));
        let rail_pop = declarations(css, ".rail .theme-menu .pop {");
        assert!(
            rail_pop.contains("bottom: calc(100% + 10px)") && rail_pop.contains("top: auto"),
            "the rail popover must open upward, got: {rail_pop}"
        );
    }

    /// The declarations of the first rule with this selector. The stylesheet is
    /// a string constant, so this is enough of a parser to assert on one rule.
    fn declarations<'a>(css: &'a str, selector: &str) -> &'a str {
        css.split(selector)
            .nth(1)
            .unwrap_or_else(|| panic!("no rule for {selector}"))
            .split('}')
            .next()
            .unwrap()
    }

    /// A flash survives a redirect and is escaped on the way out.
    #[test]
    fn flashes_round_trip_through_a_query_string() {
        let f = Flash::ok("Site created & ready");
        let encoded = f.to_query();
        assert!(encoded.starts_with("ok="));
        assert!(!encoded.contains('&'), "the message must not break the query: {encoded}");

        let mut query = std::collections::HashMap::new();
        query.insert("err".to_string(), "<script>".to_string());
        let parsed = Flash::from_query(&query).unwrap();
        assert!(!parsed.is_ok());
        let html = page(sample_page("", Some(&parsed), &[]));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains(r#"class="banner error""#));
        assert!(!html.contains("<script>alert"));
    }

    #[test]
    fn ok_wins_when_both_are_present() {
        let mut query = std::collections::HashMap::new();
        query.insert("ok".to_string(), "done".to_string());
        query.insert("err".to_string(), "nope".to_string());
        assert!(Flash::from_query(&query).unwrap().is_ok());
        assert!(Flash::from_query(&std::collections::HashMap::new()).is_none());
    }

    /// A warning is saved-but-not-working: it round-trips as its own kind and
    /// draws as a warning, not as the success it is not.
    #[test]
    fn a_warning_flash_is_neither_success_nor_failure() {
        let f = Flash::warn("Settings saved. Mail is still not being sent.");
        assert!(f.to_query().starts_with("warn="));
        assert!(!f.is_ok());

        let mut query = std::collections::HashMap::new();
        query.insert("warn".to_string(), "Settings saved, but mail is off".to_string());
        let parsed = Flash::from_query(&query).unwrap();
        assert_eq!(parsed.kind, FlashKind::Warn);
        let html = page(sample_page("", Some(&parsed), &[]));
        assert!(html.contains(r#"class="banner warn""#), "got {html}");
        assert!(html.contains("but mail is off"));
    }

    #[test]
    fn meters_show_a_proportion() {
        let html = meter("Members", 250, Some(500), number);
        assert!(html.contains("250"));
        assert!(html.contains("of 500"));
        assert!(html.contains("width:50%"));
        assert!(!html.contains("bar full"));
    }

    /// A meter at or over its ceiling has to look different, because that is
    /// the moment the customer needs to notice it.
    #[test]
    fn a_full_meter_is_marked() {
        let at = meter("Members", 500, Some(500), number);
        assert!(at.contains("bar full"));
        // Over, which happens after a downgrade, must not overflow the bar.
        let over = meter("Members", 900, Some(500), number);
        assert!(over.contains("width:100%"));
        assert!(over.contains("bar full"));
    }

    #[test]
    fn unlimited_and_withheld_read_differently() {
        assert!(meter("Members", 10, None, number).contains("unlimited"));
        let withheld = meter("Newsletters", 0, Some(0), number);
        assert!(withheld.contains("not included"));
        assert!(!withheld.contains("<div class=\"bar"), "no bar to draw");
    }

    #[test]
    fn formats_thousands() {
        assert_eq!(number(0), "0");
        assert_eq!(number(999), "999");
        assert_eq!(number(1_000), "1,000");
        assert_eq!(number(1_234_567), "1,234,567");
        assert_eq!(number(-4_200), "-4,200");
    }

    #[test]
    fn describes_times_relatively() {
        assert_eq!(when(None), "—");
        assert_eq!(when(Some("not a date")), "—");
        assert_eq!(when(Some(&pro_core::now_iso())), "just now");
        assert!(when(Some(&pro_core::days_from_now(-2))).contains("days ago"));
        // Far enough back that a date is more useful than a distance.
        assert!(when(Some("2020-03-01T00:00:00Z")).contains("2020"));
    }

    /// Future timestamps are real here — a trial end, a scheduled deletion —
    /// and must not read as "just now".
    #[test]
    fn describes_future_times() {
        let future = when(Some(&pro_core::days_from_now(3)));
        assert!(future.starts_with("in "), "got {future}");
        assert!(future.contains("days"));
    }

    /// The confirmation step must survive a page with no JavaScript, because
    /// the Content-Security-Policy on every one of these pages blocks inline
    /// handlers. A `<details>` holding the submit button is that step.
    #[test]
    fn confirmations_need_no_script() {
        let html = action_button(
            "/x",
            "Delete",
            "danger",
            Some("Delete this site?|It's permanent, and it cannot be undone."),
        );
        assert!(html.contains(r#"<details class="confirm">"#), "got {html}");
        assert!(html.contains("Delete this site?"));
        assert!(html.contains("It&#39;s permanent"), "the apostrophe is escaped: {html}");
        assert!(!html.contains("onsubmit"), "an inline handler would be blocked by the CSP");
        // Exactly one button actually posts; the summary only opens the box.
        assert_eq!(html.matches(r#"type="submit""#).count(), 1);
    }

    /// Everything destructive is a POST, so nothing dangerous sits behind a
    /// URL a browser might prefetch.
    #[test]
    fn action_buttons_are_posts() {
        let html = action_button("/console/sites/x/suspend", "Suspend", "danger", None);
        assert!(html.contains(r#"method="post""#));
        assert!(html.contains("<button"));
        assert!(!html.contains("<details"), "no confirmation was asked for");
    }

    #[test]
    fn pills_reflect_status() {
        assert!(tenant_pill(pro_core::TenantStatus::Active).contains("chip ok"));
        assert!(tenant_pill(pro_core::TenantStatus::Suspended).contains("chip bad"));
        assert!(tenant_pill(pro_core::TenantStatus::Provisioning).contains("chip warn"));
        assert!(domain_pill(pro_core::DomainStatus::Active).contains("chip ok"));
        assert!(domain_pill(pro_core::DomainStatus::Failed).contains("chip bad"));
        // The label comes from the model, so it stays consistent with the API.
        assert!(tenant_pill(pro_core::TenantStatus::Active).contains("Active"));
    }

    /// A table's header may mark a column as numeric, and the class has to
    /// actually land — the alignment of every figure under it depends on it.
    #[test]
    fn tables_mark_their_numeric_columns() {
        let html = table(&["Site", "num:Members", "end:"], "<tr><td>x</td></tr>");
        assert!(html.contains(r#"<th class="num">Members</th>"#), "got {html}");
        assert!(html.contains(r#"<th class="end"></th>"#));
        assert!(html.contains("<th>Site</th>"));
    }

    #[test]
    fn an_auth_page_is_centred_and_has_no_rail() {
        let html = auth_page(
            "Sign in",
            "Ghost Pro",
            Theme::System,
            "Sign in",
            "Welcome back",
            None,
            "<p>form</p>",
            "",
        );
        assert!(html.contains(r#"<div class="auth">"#));
        assert!(!html.contains(r#"<aside class="rail">"#));
        assert!(html.contains(r#"class="system""#), "system resolves in CSS");
    }

    /// The public pages are the only ones a search engine might reach, and a
    /// control plane has nothing to index.
    #[test]
    fn pages_ask_not_to_be_indexed() {
        assert!(page(sample_page("", None, &[])).contains(r#"name="robots""#));
    }
}
