//! The messages the platform sends.
//!
//! Plain functions returning a [`Message`], rather than a template engine.
//! There are six of them, they change rarely, and having the text in Rust
//! means a change to a link cannot silently diverge from the code that builds
//! it. Each has a text part; the HTML part is a light wrapper over the same
//! words, so nothing is said in one and not the other.
use crate::Message;

/// A minimal HTML shell.
///
/// No images, no external stylesheet, no tracking. A transactional email that
/// loads remote content is both a privacy leak and a deliverability problem,
/// and none of these messages need it — every one is a sentence and a link.
fn wrap(platform: &str, heading: &str, body_html: &str, cta: Option<(&str, &str)>) -> String {
    let button = match cta {
        Some((label, url)) => format!(
            r#"<p style="margin:32px 0"><a href="{url}" style="background:#15171a;color:#fff;\
text-decoration:none;padding:12px 20px;border-radius:6px;display:inline-block;\
font-weight:600">{label}</a></p>"#
        ),
        None => String::new(),
    };
    format!(
        r#"<!doctype html><html><body style="margin:0;padding:24px;background:#f6f7f8;\
font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif;\
color:#15171a;line-height:1.6">
<div style="max-width:520px;margin:0 auto;background:#fff;border-radius:10px;padding:32px">
<h1 style="font-size:20px;margin:0 0 16px">{heading}</h1>
{body_html}
{button}
<p style="color:#7c8b9a;font-size:13px;margin-top:32px;border-top:1px solid #e5eaef;\
padding-top:16px">{platform}</p>
</div></body></html>"#
    )
}

/// Confirm a new customer's address before their site is built.
pub fn verify_email(platform: &str, to: &str, name: &str, link: &str) -> Message {
    let text = format!(
        "Welcome to {platform}.\n\n\
         Confirm your email address to finish setting up your account:\n\n{link}\n\n\
         This link expires in three days. If you did not sign up, you can ignore this email."
    );
    Message {
        to: to.into(),
        to_name: Some(name.into()),
        subject: format!("Confirm your email for {platform}"),
        text,
        html: Some(wrap(
            platform,
            &format!("Welcome to {platform}"),
            "<p>Confirm your email address to finish setting up your account.</p>",
            Some(("Confirm email", link)),
        )),
    }
}

/// The site is up.
///
/// Carries the admin URL rather than the public one: the next thing a new
/// customer wants is to write something, and that is behind `/ghost/`.
pub fn site_ready(platform: &str, to: &str, name: &str, site_url: &str) -> Message {
    let admin = format!("{}/ghost/", site_url.trim_end_matches('/'));
    let text = format!(
        "Your site is ready.\n\n\
         Visit it at {site_url}\n\
         Sign in to write at {admin}\n\n\
         Sign in with the email and password you chose when you created the site — \
         they are separate from your {platform} account."
    );
    Message {
        to: to.into(),
        to_name: Some(name.into()),
        subject: "Your site is ready".into(),
        text,
        html: Some(wrap(
            platform,
            "Your site is ready",
            &format!(
                "<p>It is live at <a href=\"{site_url}\">{site_url}</a>.</p>\
                 <p>Sign in with the email and password you chose when you created the site — \
                 they are separate from your {platform} account.</p>"
            ),
            Some(("Start writing", &admin)),
        )),
    }
}

/// A password reset for the *platform* account.
pub fn reset_password(platform: &str, to: &str, name: &str, link: &str) -> Message {
    let text = format!(
        "Someone asked to reset the password for your {platform} account.\n\n{link}\n\n\
         This link expires in two hours and can be used once. \
         If it was not you, nothing has changed and you can ignore this email."
    );
    Message {
        to: to.into(),
        to_name: Some(name.into()),
        subject: format!("Reset your {platform} password"),
        text,
        html: Some(wrap(
            platform,
            "Reset your password",
            "<p>This link expires in two hours and can be used once. \
             If it was not you, nothing has changed.</p>",
            Some(("Choose a new password", link)),
        )),
    }
}

/// A custom domain is live.
pub fn domain_live(platform: &str, to: &str, name: &str, domain: &str) -> Message {
    let url = format!("https://{domain}");
    let text = format!(
        "{domain} is now serving your site over HTTPS.\n\n{url}\n\n\
         The certificate renews automatically."
    );
    Message {
        to: to.into(),
        to_name: Some(name.into()),
        subject: format!("{domain} is live"),
        text,
        html: Some(wrap(
            platform,
            &format!("{domain} is live"),
            "<p>Your domain is serving over HTTPS, and the certificate renews \
             automatically.</p>",
            Some(("Visit your site", &url)),
        )),
    }
}

/// A payment failed. The one message that has to be unambiguous about
/// consequences and dates.
pub fn payment_failed(
    platform: &str,
    to: &str,
    name: &str,
    billing_url: &str,
    grace_days: i64,
) -> Message {
    let when = if grace_days == 1 {
        "tomorrow".to_string()
    } else {
        format!("in {grace_days} days")
    };
    let text = format!(
        "We could not take payment for your {platform} subscription.\n\n\
         Your site is still online. If the payment is not completed, it will be taken \
         offline {when} — nothing is deleted, and it comes straight back when payment \
         succeeds.\n\n\
         Update your payment details: {billing_url}"
    );
    Message {
        to: to.into(),
        to_name: Some(name.into()),
        subject: "Your payment did not go through".into(),
        text,
        html: Some(wrap(
            platform,
            "Your payment did not go through",
            &format!(
                "<p>Your site is still online. If the payment is not completed, it will be \
                 taken offline {when} — <strong>nothing is deleted</strong>, and it comes \
                 straight back when payment succeeds.</p>"
            ),
            Some(("Update payment details", billing_url)),
        )),
    }
}

/// The site has been taken offline for non-payment.
pub fn site_suspended(platform: &str, to: &str, name: &str, billing_url: &str) -> Message {
    let text = format!(
        "Your site is no longer visible to the public because payment is outstanding.\n\n\
         Nothing has been deleted. Your posts, members and settings are all intact, and \
         you can still sign in to your site's admin. Completing payment brings it back \
         immediately.\n\n{billing_url}"
    );
    Message {
        to: to.into(),
        to_name: Some(name.into()),
        subject: "Your site has been taken offline".into(),
        text,
        html: Some(wrap(
            platform,
            "Your site has been taken offline",
            "<p><strong>Nothing has been deleted.</strong> Your posts, members and settings \
             are intact, and you can still sign in to your site's admin. Completing payment \
             brings it back immediately.</p>",
            Some(("Complete payment", billing_url)),
        )),
    }
}

/// A trial is ending.
pub fn trial_ending(
    platform: &str,
    to: &str,
    name: &str,
    billing_url: &str,
    days_left: i64,
) -> Message {
    let when =
        if days_left <= 1 { "tomorrow".to_string() } else { format!("in {days_left} days") };
    let text = format!(
        "Your {platform} trial ends {when}.\n\n\
         Add a payment method to keep your site online. Nothing is deleted if you do \
         not — the site is simply hidden until you do.\n\n{billing_url}"
    );
    Message {
        to: to.into(),
        to_name: Some(name.into()),
        subject: format!("Your trial ends {when}"),
        text,
        html: Some(wrap(
            platform,
            &format!("Your trial ends {when}"),
            "<p>Add a payment method to keep your site online. Nothing is deleted if you \
             do not — the site is simply hidden until you do.</p>",
            Some(("Add payment method", billing_url)),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every message needs a text part: without one it is unreadable in a
    /// plain client and looks like spam to a filter.
    #[test]
    fn every_message_has_text_and_a_subject() {
        let all = vec![
            verify_email("Vuuut", "a@b.co", "A", "https://pro/verify?t=1"),
            site_ready("Vuuut", "a@b.co", "A", "https://alice.sites.example.com"),
            reset_password("Vuuut", "a@b.co", "A", "https://pro/reset?t=1"),
            domain_live("Vuuut", "a@b.co", "A", "blog.example.com"),
            payment_failed("Vuuut", "a@b.co", "A", "https://pro/billing", 14),
            site_suspended("Vuuut", "a@b.co", "A", "https://pro/billing"),
            trial_ending("Vuuut", "a@b.co", "A", "https://pro/billing", 3),
        ];
        for m in &all {
            assert!(!m.subject.trim().is_empty());
            assert!(!m.text.trim().is_empty());
            assert!(m.html.is_some());
            assert_eq!(m.to, "a@b.co");
        }
    }

    /// A link that appears in the HTML but not the text would be unreachable
    /// for anybody reading the plain part.
    #[test]
    fn links_appear_in_both_parts() {
        let m = verify_email("Vuuut", "a@b.co", "A", "https://pro.example/verify?t=abc");
        assert!(m.text.contains("https://pro.example/verify?t=abc"));
        assert!(m.html.as_ref().unwrap().contains("https://pro.example/verify?t=abc"));
    }

    /// Transactional mail must not load remote content — a privacy leak and a
    /// deliverability problem both.
    #[test]
    fn html_loads_nothing_remote() {
        for m in [
            verify_email("P", "a@b.co", "A", "https://x/1"),
            site_suspended("P", "a@b.co", "A", "https://x/2"),
        ] {
            let html = m.html.unwrap();
            assert!(!html.contains("<img"), "no remote images");
            assert!(!html.contains("<script"));
            assert!(!html.contains("stylesheet"));
        }
    }

    /// The next thing a new customer wants is to write, which is behind
    /// `/ghost/`.
    #[test]
    fn site_ready_points_at_the_admin() {
        let m = site_ready("P", "a@b.co", "A", "https://alice.sites.example.com/");
        assert!(m.text.contains("https://alice.sites.example.com/ghost/"));
        // No doubled slash from the trailing one on the input.
        assert!(!m.text.contains("com//ghost"));
    }

    /// The two passwords are different systems, and saying so here is what
    /// stops a confused support ticket.
    #[test]
    fn site_ready_explains_the_two_passwords() {
        let m = site_ready("Vuuut", "a@b.co", "A", "https://a.example.com");
        assert!(m.text.contains("separate from your Vuuut account"));
    }

    /// The dunning messages have to be unambiguous that nothing is lost —
    /// this is the single most reassurance-sensitive thing the platform says.
    #[test]
    fn dunning_promises_nothing_is_deleted() {
        let failed = payment_failed("P", "a@b.co", "A", "https://x/billing", 14);
        assert!(failed.text.contains("nothing is deleted"));
        assert!(failed.text.contains("in 14 days"));

        let suspended = site_suspended("P", "a@b.co", "A", "https://x/billing");
        assert!(suspended.text.contains("Nothing has been deleted"));
        assert!(suspended.text.contains("still sign in"));
    }

    /// "in 1 days" is the kind of thing that makes a platform look unfinished.
    #[test]
    fn grammar_holds_at_one_day() {
        let m = payment_failed("P", "a@b.co", "A", "https://x", 1);
        assert!(m.text.contains("tomorrow"), "got {}", m.text);
        assert!(!m.text.contains("1 days"));

        let t = trial_ending("P", "a@b.co", "A", "https://x", 1);
        assert!(t.subject.contains("tomorrow"));
        assert!(!t.subject.contains("1 days"));

        let t0 = trial_ending("P", "a@b.co", "A", "https://x", 0);
        assert!(!t0.subject.contains("0 days"), "got {}", t0.subject);
    }

    /// The platform's name is an operator setting, so it has to actually
    /// appear rather than being hardcoded.
    #[test]
    fn uses_the_configured_platform_name() {
        let m = verify_email("Acme Hosting", "a@b.co", "A", "https://x/1");
        assert!(m.subject.contains("Acme Hosting"));
        assert!(m.text.contains("Acme Hosting"));
        assert!(m.html.unwrap().contains("Acme Hosting"));
    }
}
