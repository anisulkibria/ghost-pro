//! Identifiers and tokens.
//!
//! Two different jobs that must not be confused. An **id** only has to be
//! unique — it appears in URLs and logs and is not a secret. A **token** is a
//! bearer credential: a session cookie, a verification link, a domain
//! ownership proof. Guessing one of those is the same as holding it, so they
//! come from the OS entropy source at 256 bits and are compared in constant
//! time.
use rand::RngCore;

/// A 24-character lowercase hex id, matching the shape Ghost uses for its own
/// object ids. Not a secret; unique is all it has to be.
pub fn object_id() -> String {
    let mut bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// 256 bits of entropy, hex-encoded: session ids, verification links, API
/// keys, domain proofs.
///
/// Hex rather than base64 because these end up in URLs, DNS TXT records and
/// shell commands, and hex has no characters any of those three treat
/// specially.
pub fn secret_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Identifies Vuuut Pro ownership tokens in DNS TXT records.
pub const DOMAIN_VERIFICATION_PREFIX: &str = "vuuut-pro-verify=";
/// The prefix tokens were issued with before the rename. Still stored for
/// older domains and still accepted in DNS, so nothing already connected breaks.
pub const LEGACY_DOMAIN_VERIFICATION_PREFIX: &str = "ghost-pro-verify=";
/// The dedicated TXT name, under the domain being connected.
pub const DOMAIN_CHALLENGE_LABEL: &str = "_vuuut-pro-challenge";
/// The name used before the rename; still checked.
pub const LEGACY_DOMAIN_CHALLENGE_LABEL: &str = "_ghost-pro-challenge";

/// The random part of a stored token, whichever prefix it was issued with.
pub fn domain_verification_secret(token: &str) -> Option<&str> {
    let token = token.trim();
    token
        .strip_prefix(DOMAIN_VERIFICATION_PREFIX)
        .or_else(|| token.strip_prefix(LEGACY_DOMAIN_VERIFICATION_PREFIX))
        .filter(|secret| !secret.is_empty())
}

/// A stored token as the customer should publish it today.
pub fn domain_verification_record(token: &str) -> String {
    match domain_verification_secret(token) {
        Some(secret) => format!("{DOMAIN_VERIFICATION_PREFIX}{secret}"),
        None => token.trim().to_string(),
    }
}

/// The token a customer publishes as a DNS TXT record to prove they own a
/// domain.
///
/// Prefixed so that somebody auditing their own DNS can tell what the record
/// is for and that it is safe to remove once the domain is live.
pub fn domain_verification_token() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    format!("{DOMAIN_VERIFICATION_PREFIX}{}", hex::encode(bytes))
}

/// SHA-256, hex-encoded.
///
/// Session ids are stored as a hash, not in the clear: a dump of the sessions
/// table is then a list of useless hashes rather than a set of live logins.
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    hex::encode(h.finalize())
}

/// Compare two secrets without leaking where they first differ.
///
/// A short-circuiting `==` on a token lets an attacker recover it a byte at a
/// time by measuring how long the comparison took. Every comparison of a
/// token, signature or hash in this platform goes through here.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // The length is not a secret, and an unequal-length comparison below
    // would index out of bounds — but return early in a way that still does
    // not depend on *where* they differ.
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Turn a title into a candidate subdomain label.
///
/// Only a suggestion: the result still goes through `Config::slug_available`,
/// which is what actually decides. This exists so that signing up with the
/// name "Anna's Field Notes!" offers `anna-s-field-notes` instead of an empty
/// box.
pub fn slugify(input: &str) -> String {
    let s = slug::slugify(input);
    // A DNS label caps at 63 characters, and truncating can leave a trailing
    // hyphen, which is also invalid.
    let s = if s.len() > 63 { s[..63].to_string() } else { s };
    s.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_look_right_and_do_not_repeat() {
        let a = object_id();
        assert_eq!(a.len(), 24);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, object_id());
    }

    #[test]
    fn tokens_are_256_bits() {
        let t = secret_token();
        assert_eq!(t.len(), 64, "32 bytes as hex");
        assert_ne!(t, secret_token());
    }

    /// The token goes into a DNS TXT record, so it must survive one — no
    /// characters needing quoting, and short enough for a single string.
    #[test]
    fn domain_tokens_are_dns_safe() {
        let t = domain_verification_token();
        assert!(t.starts_with("vuuut-pro-verify="));
        let legacy = t.replace("vuuut-pro-verify=", "ghost-pro-verify=");
        assert_eq!(domain_verification_record(&legacy), t, "old tokens are shown with the new prefix");
        assert_eq!(domain_verification_secret(&legacy), domain_verification_secret(&t));
        assert!(t.len() < 255);
        assert!(t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '='));
    }

    #[test]
    fn compares_without_short_circuiting() {
        let t = secret_token();
        assert!(constant_time_eq(&t, &t.clone()));
        assert!(!constant_time_eq(&t, &secret_token()));
        assert!(!constant_time_eq("abc", "abcd"), "unequal lengths");
        assert!(constant_time_eq("", ""));
        // Differing in the first byte and the last byte must both be false —
        // the point is that neither is *faster*.
        assert!(!constant_time_eq("xbc", "abc"));
        assert!(!constant_time_eq("abx", "abc"));
    }

    #[test]
    fn hashes_are_stable() {
        assert_eq!(sha256_hex("ghost"), sha256_hex("ghost"));
        assert_eq!(sha256_hex("ghost").len(), 64);
        assert_ne!(sha256_hex("ghost"), sha256_hex("ghosu"));
    }

    #[test]
    fn slugifies_titles() {
        // The apostrophe becomes a separator rather than being dropped, which
        // is why this is only ever a suggestion the customer can edit.
        assert_eq!(slugify("Anna's Field Notes!"), "anna-s-field-notes");
        assert_eq!(slugify("  Hello   World  "), "hello-world");
    }

    /// Truncating at 63 can leave a trailing hyphen, which is not a valid DNS
    /// label — so the trim has to come after the cut, not before.
    #[test]
    fn slug_fits_a_dns_label() {
        let long = slugify(&"a".repeat(100));
        assert_eq!(long.len(), 63);

        let awkward = slugify(&format!("{} x", "a".repeat(62)));
        assert!(awkward.len() <= 63);
        assert!(!awkward.ends_with('-'), "got {awkward:?}");
        assert!(!awkward.starts_with('-'));
    }
}
