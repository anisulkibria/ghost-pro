//! Writing a tenant's `config.<env>.json`.
//!
//! This file is the entire interface between the platform and a tenant's CMS.
//! Everything the platform decides about a site — its canonical URL, where its
//! database lives, which plan limits apply — is expressed here and nowhere
//! else. The tenant process reads it at boot, and that is why a plan change
//! needs a restart.
//!
//! Two details are easy to get wrong and both are silent:
//!
//! 1. **`hostSettings` must be in the file, not the environment.** Ghost's
//!    env-var layer parses values as scalars, so `GHOST_HOSTSETTINGS={...}`
//!    arrives as a *string*. `Limits::load` then finds no `limits` object and
//!    every plan ceiling disappears — a paid plan silently becomes unlimited.
//! 2. **`url` must be what the browser sees**, not what the proxy speaks to
//!    the tenant. The tenant serves plain HTTP on loopback, but its `url` is
//!    `https://…`, because that string is the base of every link it emits and
//!    decides whether its cookies are marked secure.
use pro_core::{Plan, Result};
use serde_json::{json, Value};
use std::path::Path;

/// Build the config body for a tenant.
///
/// `public_url` is the canonical address — the primary domain, with the scheme
/// the browser uses. `port` is the loopback port the process binds.
pub fn build(
    public_url: &str,
    port: u16,
    plan: &Plan,
    period_start: &str,
    billing_url: &str,
    mail: &pro_core::config::MailConfig,
) -> Value {
    json!({
        "url": public_url,
        // Loopback only. Every request arrives through the platform's proxy,
        // so a tenant is not addressable from outside even if its port leaked.
        "server": { "host": "127.0.0.1", "port": port },
        "database": {
            "client": "sqlite3",
            // Relative to the working directory, which is this tenant's own
            // directory — so the whole site is one movable, archivable tree.
            "connection": { "filename": "content/data/ghost.db" }
        },
        "paths": { "contentPath": "content" },
        // The edge terminates TLS for every hostname; a tenant doing its own
        // ACME would both fail (it cannot bind 443) and be redundant.
        "ssl": false,
        "mail": mail_block(mail),
        // The plan, in the only form the CMS understands.
        "hostSettings": plan.host_settings(period_start, billing_url),
    })
}

/// The tenant's transactional mail.
///
/// Inherits the platform's SMTP by default, so member signup confirmations and
/// password resets work the moment a site exists. A tenant's *newsletters* are
/// a separate concern the site owner configures themselves — this is only the
/// one-to-one mail that a site is broken without.
fn mail_block(mail: &pro_core::config::MailConfig) -> Value {
    if mail.transport != "smtp" || mail.host.is_none() {
        // No usable platform SMTP: say so explicitly rather than writing a
        // half-configured block the tenant would fail on at send time.
        return json!({ "transport": "Direct" });
    }
    json!({
        "transport": "SMTP",
        "options": {
            "host": mail.host,
            "port": mail.port.unwrap_or(587),
            "auth": {
                "user": mail.user,
                "pass": mail.pass,
            }
        },
        "from": mail.from_address,
    })
}

/// Write the config file atomically.
///
/// Written to a temporary file and renamed, because a tenant may be starting
/// while this runs — and a process that reads a half-written config either
/// fails to boot or, worse, boots with partial limits. `rename` within a
/// directory is atomic, so a reader sees either the old file or the new one.
pub async fn write(dir: &Path, env: &str, body: &Value) -> Result<()> {
    let final_path = dir.join(format!("config.{env}.json"));
    let tmp_path = dir.join(format!(".config.{env}.json.tmp"));
    let pretty = serde_json::to_string_pretty(body)?;
    tokio::fs::write(&tmp_path, pretty.as_bytes()).await?;
    tokio::fs::rename(&tmp_path, &final_path).await?;
    Ok(())
}

/// Read back a tenant's config, for a targeted edit.
pub async fn read(dir: &Path, env: &str) -> Result<Value> {
    let path = dir.join(format!("config.{env}.json"));
    let raw = tokio::fs::read_to_string(&path).await?;
    Ok(serde_json::from_str(&raw)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pro_core::config::MailConfig;

    fn mail() -> MailConfig {
        MailConfig {
            transport: "smtp".into(),
            host: Some("smtp.example.com".into()),
            port: Some(587),
            user: Some("user".into()),
            pass: Some("pass".into()),
            from_address: Some("hello@example.com".into()),
            from_name: Some("Example".into()),
            reply_to: None,
        }
    }

    #[test]
    fn writes_the_shape_ghost_expects() {
        let plan = &pro_core::plans::default_plans()[0];
        let cfg = build(
            "https://alice.sites.example.com",
            21000,
            plan,
            "2026-01-01T00:00:00Z",
            "https://pro.example.com/billing",
            &mail(),
        );

        assert_eq!(cfg["url"], "https://alice.sites.example.com");
        assert_eq!(cfg["server"]["host"], "127.0.0.1");
        assert_eq!(cfg["server"]["port"], 21000);
        assert_eq!(cfg["database"]["client"], "sqlite3");
        assert_eq!(cfg["database"]["connection"]["filename"], "content/data/ghost.db");
        assert_eq!(cfg["paths"]["contentPath"], "content");
        assert_eq!(cfg["ssl"], false, "the edge terminates TLS, not the tenant");
    }

    /// The limits must be a real JSON object. If they were a string — which is
    /// what passing them through the environment produces — every plan ceiling
    /// would silently vanish.
    #[test]
    fn host_settings_is_an_object_with_limits() {
        let plan = &pro_core::plans::default_plans()[0];
        let cfg = build("https://a.example.com", 21000, plan, "2026-01-01T00:00:00Z", "https://b/billing", &mail());

        assert!(cfg["hostSettings"].is_object());
        assert!(cfg["hostSettings"]["limits"].is_object());
        assert!(cfg["hostSettings"]["limits"]["members"]["max"].is_i64());
        assert_eq!(cfg["hostSettings"]["subscription"]["start"], "2026-01-01T00:00:00Z");
        assert_eq!(cfg["hostSettings"]["billing"]["url"], "https://b/billing");
    }

    /// The tenant speaks plain HTTP on loopback, but its url has to describe
    /// the browser's view or it emits http:// links and non-secure cookies.
    #[test]
    fn url_is_the_public_one_not_the_loopback_one() {
        let plan = &pro_core::plans::default_plans()[0];
        let cfg = build("https://blog.customer.com", 21005, plan, "2026-01-01T00:00:00Z", "https://b/billing", &mail());
        assert!(cfg["url"].as_str().unwrap().starts_with("https://"));
        assert_eq!(cfg["server"]["host"], "127.0.0.1");
    }

    #[test]
    fn inherits_platform_smtp() {
        let plan = &pro_core::plans::default_plans()[0];
        let cfg = build("https://a.example.com", 21000, plan, "2026-01-01T00:00:00Z", "https://b/billing", &mail());
        assert_eq!(cfg["mail"]["transport"], "SMTP");
        assert_eq!(cfg["mail"]["options"]["host"], "smtp.example.com");
        assert_eq!(cfg["mail"]["options"]["auth"]["user"], "user");
        assert_eq!(cfg["mail"]["from"], "hello@example.com");
    }

    /// With no usable platform SMTP, say so plainly rather than writing a
    /// half-configured block that fails at send time.
    #[test]
    fn falls_back_when_there_is_no_smtp() {
        let plan = &pro_core::plans::default_plans()[0];
        let no_mail = MailConfig { transport: "log".into(), ..mail() };
        let cfg = build("https://a.example.com", 21000, plan, "2026-01-01T00:00:00Z", "https://b/billing", &no_mail);
        assert_eq!(cfg["mail"]["transport"], "Direct");

        let smtp_without_host = MailConfig { host: None, ..mail() };
        let cfg2 = build("https://a.example.com", 21000, plan, "2026-01-01T00:00:00Z", "https://b/billing", &smtp_without_host);
        assert_eq!(cfg2["mail"]["transport"], "Direct");
    }

    /// A suspended site keeps its admin but closes to the public.
    #[test]
    fn a_suspended_plan_closes_the_public_site() {
        let cfg = build(
            "https://a.example.com",
            21000,
            &pro_core::plans::suspended_plan(),
            "2026-01-01T00:00:00Z",
            "https://b/billing",
            &mail(),
        );
        assert_eq!(cfg["hostSettings"]["limits"]["publicSiteAccess"]["disabled"], true);
    }

    /// A tenant may be starting while this runs; a reader must never see a
    /// half-written file.
    #[tokio::test]
    async fn writes_atomically_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("pro-cfg-{}", pro_core::ids::object_id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let plan = &pro_core::plans::default_plans()[0];
        let body = build("https://a.example.com", 21000, plan, "2026-01-01T00:00:00Z", "https://b/billing", &mail());
        write(&dir, "production", &body).await.unwrap();

        let back = read(&dir, "production").await.unwrap();
        assert_eq!(back["url"], "https://a.example.com");
        assert!(back["hostSettings"]["limits"].is_object());

        // No temporary file left behind.
        assert!(!dir.join(".config.production.json.tmp").exists());
        assert!(dir.join("config.production.json").exists());

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
