//! `pro-mail` — the transactional mail the *platform* sends.
//!
//! Confirm your address, your site is ready, your card was declined. Nothing
//! to do with the newsletters a tenant sends: those go through that tenant's
//! own Ghost mail configuration, deliberately separate so one customer's
//! sending reputation cannot be spent by another's.
//!
//! The `log` transport is the default, and is what development should use. It
//! writes the whole message — including any link — to the platform log, so a
//! signup flow can be walked end to end with no mail server anywhere. A
//! platform that quietly failed to send instead would look like it worked
//! until somebody waited for an email that never came.
use pro_core::config::MailConfig;
use pro_core::{ProError, Result};

pub mod templates;

/// One message.
pub struct Message {
    pub to: String,
    pub to_name: Option<String>,
    pub subject: String,
    /// Plain text. Always sent, and always first: a text part is what makes a
    /// message readable in a client that refuses HTML, and its absence is one
    /// of the strongest spam signals there is.
    pub text: String,
    pub html: Option<String>,
}

pub struct Mailer {
    config: MailConfig,
    platform_name: String,
}

/// Unset, or set to nothing but spaces — which an operator pasting into a
/// settings field does by accident, and which must not read as configured.
fn blank(value: &Option<String>) -> bool {
    value.as_ref().map(|v| v.trim().is_empty()).unwrap_or(true)
}

impl Mailer {
    pub fn new(config: MailConfig, platform_name: String) -> Self {
        Self { config, platform_name }
    }

    /// Whether mail will actually leave the machine.
    ///
    /// The dashboard uses this to decide whether to promise a customer an
    /// email, rather than telling them to check an inbox nothing was sent to.
    pub fn is_live(&self) -> bool {
        self.not_live_reason().is_none()
    }

    /// Why mail cannot leave the machine, in a sentence an operator can act
    /// on — or `None` when it can.
    ///
    /// One function, because every place that reports mail state (the console
    /// dashboard, `doctor`, the log line on a send) was previously deciding
    /// for itself what "configured" meant, and they disagreed. `transport:
    /// "smtp"` with no host passed the console's check and failed the
    /// mailer's, so the platform told an operator mail was fine while quietly
    /// writing every message to the log.
    ///
    /// A missing `fromAddress` counts, too. Without it a send fails inside
    /// [`Self::from`] *after* the customer has been told to check their
    /// inbox, which is the same broken promise arriving by a longer route.
    pub fn not_live_reason(&self) -> Option<String> {
        if self.config.transport != "smtp" {
            return Some(format!(
                "mail.transport is {:?}, so messages are written to the platform log \
                 instead of being sent",
                self.config.transport
            ));
        }

        let mut missing: Vec<&str> = Vec::new();
        if blank(&self.config.host) {
            missing.push("mail.host");
        }
        if blank(&self.config.from_address) {
            missing.push("mail.fromAddress");
        }
        if missing.is_empty() {
            return None;
        }
        Some(format!(
            "mail.transport is \"smtp\" but {} {} not set, so messages are written to \
             the platform log instead of being sent",
            missing.join(" and "),
            if missing.len() == 1 { "is" } else { "are" },
        ))
    }

    fn from(&self) -> Result<lettre::message::Mailbox> {
        let address = self.config.from_address.clone().ok_or_else(|| {
            ProError::Config("mail.fromAddress must be set to send platform email".into())
        })?;
        let name = self.config.from_name.clone().unwrap_or_else(|| self.platform_name.clone());
        format!("{name} <{address}>")
            .parse()
            .map_err(|e| ProError::Config(format!("mail.fromAddress {address:?} is not valid: {e}")))
    }

    /// Send, or log.
    ///
    /// Returns `Ok` for the log transport, because "the platform is configured
    /// not to send" is not a failure of the caller's operation — a signup
    /// should still succeed on a development machine with no SMTP.
    pub async fn send(&self, message: Message) -> Result<()> {
        if let Some(reason) = self.not_live_reason() {
            // WARN, not INFO: a message that was not sent is the kind of thing
            // somebody goes looking for in the log after waiting for an email
            // that never came, and the reason is the whole answer.
            tracing::warn!(
                to = %message.to,
                subject = %message.subject,
                "not sent — {reason}. The message, link and all, was:\n{}",
                message.text
            );
            return Ok(());
        }
        self.send_smtp(message).await
    }

    async fn send_smtp(&self, message: Message) -> Result<()> {
        use lettre::message::{header::ContentType, MultiPart, SinglePart};
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Message as Email, Tokio1Executor};

        let to: lettre::message::Mailbox = match &message.to_name {
            Some(name) if !name.trim().is_empty() => {
                format!("{name} <{}>", message.to).parse()
            }
            _ => message.to.parse(),
        }
        .map_err(|e| ProError::BadRequest(format!("{:?} is not a valid address: {e}", message.to)))?;

        let mut builder = Email::builder().from(self.from()?).to(to).subject(&message.subject);
        if let Some(reply_to) = &self.config.reply_to {
            if let Ok(mailbox) = reply_to.parse::<lettre::message::Mailbox>() {
                builder = builder.reply_to(mailbox);
            }
        }

        let email = match &message.html {
            Some(html) => builder.multipart(
                MultiPart::alternative()
                    // Text first: a client picks the last part it understands,
                    // so this order means HTML wins where it is supported and
                    // text is what a plain reader sees.
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_PLAIN)
                            .body(message.text.clone()),
                    )
                    .singlepart(
                        SinglePart::builder().header(ContentType::TEXT_HTML).body(html.clone()),
                    ),
            ),
            None => builder.body(message.text.clone()),
        }
        .map_err(|e| ProError::internal(format!("could not build the message: {e}")))?;

        let host = self.config.host.clone().unwrap_or_default();
        let port = self.config.port.unwrap_or(587);

        // Port 465 is implicit TLS; everything else is STARTTLS on a plain
        // connection. Getting this backwards produces a handshake error that
        // reads like a certificate problem, so it is chosen from the port
        // rather than configured separately.
        let mut transport = if port == 465 {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&host)
                .map_err(|e| ProError::Config(format!("mail.host {host:?}: {e}")))?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)
                .map_err(|e| ProError::Config(format!("mail.host {host:?}: {e}")))?
        }
        .port(port);

        if let (Some(user), Some(pass)) = (&self.config.user, &self.config.pass) {
            transport = transport.credentials(Credentials::new(user.clone(), pass.clone()));
        }

        transport
            .build()
            .send(email)
            .await
            .map_err(|e| ProError::Unavailable(format!("could not send mail: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_mailer() -> Mailer {
        Mailer::new(MailConfig::default(), "Ghost Pro".into())
    }

    fn smtp_config() -> MailConfig {
        MailConfig {
            transport: "smtp".into(),
            host: Some("smtp.example.com".into()),
            port: Some(587),
            user: Some("u".into()),
            pass: Some("p".into()),
            from_address: Some("hello@example.com".into()),
            from_name: Some("Example Hosting".into()),
            reply_to: Some("help@example.com".into()),
        }
    }

    /// The default must not pretend to send.
    #[test]
    fn the_log_transport_is_not_live() {
        assert!(!log_mailer().is_live());
    }

    /// SMTP with no host is a half-configured platform, and must not read as
    /// live — the dashboard uses this to decide whether to promise an email.
    #[test]
    fn smtp_without_a_host_is_not_live() {
        let half = MailConfig { host: None, ..smtp_config() };
        assert!(!Mailer::new(half, "P".into()).is_live());
        assert!(Mailer::new(smtp_config(), "P".into()).is_live());
    }

    /// The trap this pair of tests exists for: `transport: "smtp"` with a
    /// field missing used to pass the console's and `doctor`'s idea of
    /// "configured" while the mailer quietly logged every message.
    #[test]
    fn a_half_configured_smtp_says_which_field_is_missing() {
        let no_host = MailConfig { host: None, ..smtp_config() };
        let reason = Mailer::new(no_host, "P".into()).not_live_reason().unwrap();
        assert!(reason.contains("mail.host"), "got {reason}");
        assert!(!reason.contains("mail.fromAddress"), "got {reason}");

        let neither = MailConfig { host: None, from_address: None, ..smtp_config() };
        let reason = Mailer::new(neither, "P".into()).not_live_reason().unwrap();
        assert!(reason.contains("mail.host") && reason.contains("mail.fromAddress"), "got {reason}");
    }

    /// Without a from address a send fails *after* the customer has been told
    /// to check their inbox, so it has to count as not live.
    #[test]
    fn smtp_without_a_from_address_is_not_live() {
        let cfg = MailConfig { from_address: None, ..smtp_config() };
        assert!(!Mailer::new(cfg, "P".into()).is_live());
    }

    /// A field holding spaces is not a configured field.
    #[test]
    fn whitespace_does_not_count_as_configured() {
        let cfg = MailConfig { host: Some("   ".into()), ..smtp_config() };
        assert!(!Mailer::new(cfg, "P".into()).is_live());
    }

    /// The log transport's reason names the transport, so an operator reading
    /// it knows this is a setting and not a failure.
    #[test]
    fn the_log_transport_explains_itself() {
        let reason = log_mailer().not_live_reason().unwrap();
        assert!(reason.contains("mail.transport"), "got {reason}");
    }

    #[test]
    fn a_fully_configured_smtp_has_no_reason() {
        assert!(Mailer::new(smtp_config(), "P".into()).not_live_reason().is_none());
    }

    /// A development machine with no SMTP must still complete a signup.
    #[tokio::test]
    async fn logging_a_message_succeeds() {
        let result = log_mailer()
            .send(Message {
                to: "customer@example.com".into(),
                to_name: Some("Customer".into()),
                subject: "Your site is ready".into(),
                text: "Visit https://alice.sites.example.com/ghost/".into(),
                html: None,
            })
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn builds_a_from_header() {
        let m = Mailer::new(smtp_config(), "Ghost Pro".into());
        let from = m.from().unwrap();
        assert_eq!(from.email.to_string(), "hello@example.com");
        assert_eq!(from.name.as_deref(), Some("Example Hosting"));
    }

    /// Falls back to the platform name rather than sending from a bare
    /// address with no display name.
    #[test]
    fn from_name_defaults_to_the_platform() {
        let cfg = MailConfig { from_name: None, ..smtp_config() };
        let m = Mailer::new(cfg, "Acme Hosting".into());
        assert_eq!(m.from().unwrap().name.as_deref(), Some("Acme Hosting"));
    }

    /// A send that cannot name a sender has to fail with a message about
    /// configuration, not a protocol error.
    #[test]
    fn a_missing_from_address_is_a_config_error() {
        let cfg = MailConfig { from_address: None, ..smtp_config() };
        let err = Mailer::new(cfg, "P".into()).from().unwrap_err();
        assert!(err.to_string().contains("mail.fromAddress"), "got {err}");
    }

    #[test]
    fn an_invalid_from_address_is_rejected() {
        let cfg = MailConfig { from_address: Some("not an address".into()), ..smtp_config() };
        assert!(Mailer::new(cfg, "P".into()).from().is_err());
    }
}
