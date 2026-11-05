//! Webhook signature verification.
//!
//! All three providers sign with HMAC-SHA256 and differ only in what they sign
//! and how they present it:
//!
//! | Provider | Header | Signed payload |
//! |---|---|---|
//! | Stripe | `Stripe-Signature: t=…,v1=…` | `"{t}.{body}"` |
//! | Paddle | `Paddle-Signature: ts=…;h1=…` | `"{ts}:{body}"` |
//! | Lemon Squeezy | `X-Signature: …` | the body alone |
//!
//! Three rules apply to all of them, and each one is a real vulnerability if
//! broken:
//!
//! 1. **Verify the raw bytes.** Re-serialising parsed JSON changes key order
//!    and whitespace, so the signature stops matching — and the usual
//!    "fix" is to stop verifying.
//! 2. **Compare in constant time.** A short-circuiting comparison leaks the
//!    expected signature a byte at a time, and a webhook endpoint can be
//!    probed as often as an attacker likes.
//! 3. **Reject an old timestamp.** Without it, a single captured webhook can
//!    be replayed forever — and a captured "subscription became active" is a
//!    free subscription, repeatable.
//!
//! An unconfigured secret is a *refusal*, never a pass. An unverified billing
//! event is how a stranger grants themselves a plan, so the absence of a
//! secret has to fail closed.
use pro_core::{ids, ProError, Result};

/// How far out of date a signed timestamp may be.
///
/// Five minutes, which is Stripe's own recommendation. Long enough to absorb
/// clock skew and a slow retry, short enough that a captured request is not a
/// standing credential.
pub const MAX_AGE_SECS: i64 = 300;

/// HMAC-SHA256, hex-encoded.
///
/// Implemented directly over SHA-256 rather than pulling in an HMAC crate:
/// the construction is four lines, this is the only place it is needed, and
/// the block-size handling below is the whole of it.
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;

    // A key longer than the block size is hashed down first; a shorter one is
    // zero-padded. Skipping this step is the classic HMAC implementation bug.
    let mut normalised = if key.len() > BLOCK {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    normalised.resize(BLOCK, 0);

    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        inner_pad[i] ^= normalised[i];
        outer_pad[i] ^= normalised[i];
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    hex::encode(outer.finalize())
}

/// Reject a timestamp that is too old to be a live delivery.
fn check_age(timestamp: &str) -> Result<()> {
    let ts: i64 = timestamp
        .trim()
        .parse()
        .map_err(|_| ProError::BadRequest("webhook timestamp is not a number".into()))?;
    let now = chrono::Utc::now().timestamp();
    // Only the past is bounded tightly; a slightly future timestamp is clock
    // skew on the provider's side, which is not an attack and not worth
    // rejecting a real event over.
    if now - ts > MAX_AGE_SECS {
        return Err(ProError::BadRequest(format!(
            "webhook is {}s old, beyond the {MAX_AGE_SECS}s replay window",
            now - ts
        )));
    }
    if ts - now > MAX_AGE_SECS {
        return Err(ProError::BadRequest("webhook timestamp is in the future".into()));
    }
    Ok(())
}

/// Verify a `Stripe-Signature` header.
///
/// The header carries a timestamp and one or more `v1` signatures — more than
/// one while a secret is being rotated. Any match is acceptance, which is what
/// makes rotation possible without dropping events.
pub fn verify_stripe(body: &[u8], header: &str, secret: &str) -> Result<()> {
    if secret.is_empty() {
        return Err(ProError::Config(
            "billing.stripe.webhookSecret is unset, so webhooks cannot be verified".into(),
        ));
    }
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => timestamp = Some(v.trim().to_string()),
            Some(("v1", v)) => signatures.push(v.trim().to_string()),
            _ => {}
        }
    }
    let timestamp = timestamp
        .ok_or_else(|| ProError::BadRequest("Stripe-Signature has no timestamp".into()))?;
    if signatures.is_empty() {
        return Err(ProError::BadRequest("Stripe-Signature has no v1 signature".into()));
    }
    check_age(&timestamp)?;

    let mut signed = Vec::with_capacity(timestamp.len() + 1 + body.len());
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(body);
    let expected = hmac_sha256_hex(secret.as_bytes(), &signed);

    if signatures.iter().any(|s| ids::constant_time_eq(s, &expected)) {
        Ok(())
    } else {
        Err(ProError::Forbidden("webhook signature does not match".into()))
    }
}

/// Verify a `Paddle-Signature` header.
///
/// Format is `ts=1700000000;h1=<hex>`, semicolon-separated, and the signed
/// payload is `ts:body` — a colon, where Stripe uses a dot.
pub fn verify_paddle(body: &[u8], header: &str, secret: &str) -> Result<()> {
    if secret.is_empty() {
        return Err(ProError::Config(
            "billing.paddle.webhookSecret is unset, so webhooks cannot be verified".into(),
        ));
    }
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for part in header.split(';') {
        match part.trim().split_once('=') {
            Some(("ts", v)) => timestamp = Some(v.trim().to_string()),
            Some(("h1", v)) => signatures.push(v.trim().to_string()),
            _ => {}
        }
    }
    let timestamp = timestamp
        .ok_or_else(|| ProError::BadRequest("Paddle-Signature has no ts".into()))?;
    if signatures.is_empty() {
        return Err(ProError::BadRequest("Paddle-Signature has no h1 signature".into()));
    }
    check_age(&timestamp)?;

    let mut signed = Vec::with_capacity(timestamp.len() + 1 + body.len());
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b':');
    signed.extend_from_slice(body);
    let expected = hmac_sha256_hex(secret.as_bytes(), &signed);

    if signatures.iter().any(|s| ids::constant_time_eq(s, &expected)) {
        Ok(())
    } else {
        Err(ProError::Forbidden("webhook signature does not match".into()))
    }
}

/// Verify Lemon Squeezy's `X-Signature`.
///
/// The simplest of the three: HMAC of the raw body, hex, no timestamp.
///
/// That also means Lemon Squeezy webhooks carry **no replay protection of
/// their own** — a captured delivery stays valid indefinitely as far as the
/// signature is concerned. The event-id idempotency in
/// `pro_db::audit::claim_webhook` is what closes that gap, and for this
/// provider it is not an optimisation but the only defence. It is the reason
/// that table has a unique index rather than a best-effort cache.
pub fn verify_lemonsqueezy(body: &[u8], header: &str, secret: &str) -> Result<()> {
    if secret.is_empty() {
        return Err(ProError::Config(
            "billing.lemonsqueezy.webhookSecret is unset, so webhooks cannot be verified".into(),
        ));
    }
    let expected = hmac_sha256_hex(secret.as_bytes(), body);
    if ids::constant_time_eq(header.trim(), &expected) {
        Ok(())
    } else {
        Err(ProError::Forbidden("webhook signature does not match".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_test_secret";
    const BODY: &[u8] = br#"{"id":"evt_1","type":"customer.subscription.updated"}"#;

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// Checked against the published HMAC-SHA256 test vectors (RFC 4231), so
    /// the hand-rolled construction is not merely self-consistent.
    #[test]
    fn hmac_matches_the_rfc_vectors() {
        // RFC 4231 case 1.
        assert_eq!(
            hmac_sha256_hex(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // RFC 4231 case 2: key shorter than the block.
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 4231 case 3: key longer than the hash output.
        assert_eq!(
            hmac_sha256_hex(&[0xaa; 20], &[0xdd; 50]),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    /// A key longer than the 64-byte block must be hashed down first —
    /// skipping that is the classic HMAC bug. RFC 4231 case 6.
    #[test]
    fn hmac_handles_an_oversized_key() {
        assert_eq!(
            hmac_sha256_hex(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First"),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    fn stripe_header(body: &[u8], ts: i64, secret: &str) -> String {
        let signed = format!("{ts}.{}", String::from_utf8_lossy(body));
        format!("t={ts},v1={}", hmac_sha256_hex(secret.as_bytes(), signed.as_bytes()))
    }

    #[test]
    fn stripe_accepts_a_valid_signature() {
        let header = stripe_header(BODY, now(), SECRET);
        assert!(verify_stripe(BODY, &header, SECRET).is_ok());
    }

    #[test]
    fn stripe_rejects_a_wrong_secret_and_a_tampered_body() {
        let header = stripe_header(BODY, now(), SECRET);
        assert_eq!(verify_stripe(BODY, &header, "whsec_other").unwrap_err().status(), 403);
        assert_eq!(
            verify_stripe(br#"{"id":"evt_1","type":"evil"}"#, &header, SECRET)
                .unwrap_err()
                .status(),
            403
        );
    }

    /// Without a replay window a captured "subscription active" is a free
    /// subscription, repeatable forever.
    #[test]
    fn stripe_rejects_a_replayed_webhook() {
        let old = now() - (MAX_AGE_SECS + 60);
        let header = stripe_header(BODY, old, SECRET);
        let err = verify_stripe(BODY, &header, SECRET).unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.to_string().contains("replay window"), "got {err}");
    }

    #[test]
    fn stripe_rejects_a_future_timestamp() {
        let header = stripe_header(BODY, now() + 3600, SECRET);
        assert!(verify_stripe(BODY, &header, SECRET).is_err());
    }

    /// Small clock skew is not an attack and must not drop a real event.
    #[test]
    fn stripe_tolerates_clock_skew() {
        for offset in [-30, -1, 0, 1, 30] {
            let header = stripe_header(BODY, now() + offset, SECRET);
            assert!(verify_stripe(BODY, &header, SECRET).is_ok(), "offset {offset}");
        }
    }

    /// Several v1 signatures appear while a secret is being rotated; any
    /// match is acceptance, so rotation does not drop events.
    #[test]
    fn stripe_accepts_one_of_several_signatures() {
        let ts = now();
        let valid = stripe_header(BODY, ts, SECRET);
        let good = valid.split("v1=").nth(1).unwrap().to_string();
        let header = format!("t={ts},v1=deadbeef,v1={good}");
        assert!(verify_stripe(BODY, &header, SECRET).is_ok());
    }

    #[test]
    fn stripe_rejects_a_malformed_header() {
        assert!(verify_stripe(BODY, "", SECRET).is_err());
        assert!(verify_stripe(BODY, "v1=abc", SECRET).is_err(), "no timestamp");
        assert!(verify_stripe(BODY, &format!("t={}", now()), SECRET).is_err(), "no signature");
        assert!(verify_stripe(BODY, "t=notanumber,v1=abc", SECRET).is_err());
    }

    /// Paddle signs with a colon where Stripe uses a dot, and separates with
    /// semicolons. Getting either wrong fails closed, which is why both are
    /// pinned by a test.
    #[test]
    fn paddle_accepts_a_valid_signature() {
        let ts = now();
        let signed = format!("{ts}:{}", String::from_utf8_lossy(BODY));
        let header =
            format!("ts={ts};h1={}", hmac_sha256_hex(SECRET.as_bytes(), signed.as_bytes()));
        assert!(verify_paddle(BODY, &header, SECRET).is_ok());
    }

    /// Specifically: Stripe's own format must not verify as Paddle, which is
    /// what would happen if the separator were shared.
    #[test]
    fn paddle_does_not_accept_stripes_format() {
        let header = stripe_header(BODY, now(), SECRET);
        assert!(verify_paddle(BODY, &header, SECRET).is_err());
    }

    #[test]
    fn paddle_rejects_a_replay_and_a_bad_signature() {
        let old = now() - (MAX_AGE_SECS + 60);
        let signed = format!("{old}:{}", String::from_utf8_lossy(BODY));
        let header =
            format!("ts={old};h1={}", hmac_sha256_hex(SECRET.as_bytes(), signed.as_bytes()));
        assert_eq!(verify_paddle(BODY, &header, SECRET).unwrap_err().status(), 400);

        let ts = now();
        assert_eq!(
            verify_paddle(BODY, &format!("ts={ts};h1=deadbeef"), SECRET).unwrap_err().status(),
            403
        );
    }

    #[test]
    fn lemonsqueezy_accepts_a_valid_signature() {
        let sig = hmac_sha256_hex(SECRET.as_bytes(), BODY);
        assert!(verify_lemonsqueezy(BODY, &sig, SECRET).is_ok());
        // Whitespace from a header must not matter.
        assert!(verify_lemonsqueezy(BODY, &format!("  {sig}  "), SECRET).is_ok());
    }

    #[test]
    fn lemonsqueezy_rejects_a_bad_signature() {
        assert_eq!(verify_lemonsqueezy(BODY, "deadbeef", SECRET).unwrap_err().status(), 403);
        assert!(verify_lemonsqueezy(BODY, "", SECRET).is_err());
        let sig = hmac_sha256_hex(SECRET.as_bytes(), BODY);
        assert!(verify_lemonsqueezy(br#"{"tampered":true}"#, &sig, SECRET).is_err());
    }

    /// An unconfigured secret must refuse, never pass. This is the single most
    /// important assertion in the module: failing open here hands anybody who
    /// can reach the endpoint the ability to grant themselves a subscription.
    #[test]
    fn a_missing_secret_fails_closed() {
        let ts = now();
        let stripe = stripe_header(BODY, ts, SECRET);
        assert!(verify_stripe(BODY, &stripe, "").is_err());
        assert!(verify_paddle(BODY, &format!("ts={ts};h1=abc"), "").is_err());
        assert!(verify_lemonsqueezy(BODY, &hmac_sha256_hex(b"", BODY), "").is_err());
    }

    /// The signature covers the exact bytes sent. Re-serialised JSON — same
    /// data, different key order — must fail, which is what forces callers to
    /// verify before parsing.
    #[test]
    fn reserialised_json_does_not_verify() {
        let original = br#"{"id":"evt_1","type":"a"}"#;
        let header = stripe_header(original, now(), SECRET);
        assert!(verify_stripe(original, &header, SECRET).is_ok());

        let reordered = br#"{"type":"a","id":"evt_1"}"#;
        assert!(verify_stripe(reordered, &header, SECRET).is_err());
        let reindented = br#"{ "id": "evt_1", "type": "a" }"#;
        assert!(verify_stripe(reindented, &header, SECRET).is_err());
    }

    /// An empty body is still signed and verified normally, rather than
    /// taking a shortcut.
    #[test]
    fn verifies_an_empty_body() {
        let header = stripe_header(b"", now(), SECRET);
        assert!(verify_stripe(b"", &header, SECRET).is_ok());
    }
}
