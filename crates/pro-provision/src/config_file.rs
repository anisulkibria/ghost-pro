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
    database: &Value,
) -> Value {
    let mut body = json!({
        "url": public_url,
        // Loopback only. Every request arrives through the platform's proxy,
        // so a tenant is not addressable from outside even if its port leaked.
        "server": { "host": "127.0.0.1", "port": port },
        "database": database,
        "paths": { "contentPath": "content" },
        // The edge terminates TLS for every hostname; a tenant doing its own
        // ACME would both fail (it cannot bind 443) and be redundant.
        "ssl": false,
        "mail": mail_block(mail),
        // The plan, in the only form the CMS understands.
        "hostSettings": plan.host_settings(period_start, billing_url),
    });
    apply_managed_mail(&mut body, mail);
    body
}

/// Update only the protected mail policy; preserve limits and billing metadata.
pub fn apply_managed_mail(body: &mut Value, mail: &pro_core::config::MailConfig) {
    body["hostSettings"]["emailQuota"]["autoStop"] = json!(mail.auto_stop_at_limit);
    if let Some(values) = mail.managed_mail() {
        body["hostSettings"]["managedMail"] = values;
    } else if let Some(host) = body.get_mut("hostSettings").and_then(Value::as_object_mut) {
        host.remove("managedMail");
    }
}

/// Replace plan terms without dropping operator mail credentials.
pub fn apply_host_settings(body: &mut Value, mut host: Value) {
    if let Some(mail) = body.pointer("/hostSettings/managedMail") {
        host["managedMail"] = mail.clone();
    }
    if let Some(value) = body.pointer("/hostSettings/emailQuota/autoStop") {
        host["emailQuota"]["autoStop"] = value.clone();
    }
    body["hostSettings"] = host;
}

pub fn apply_email_allowance(body: &mut Value, max: i64) {
    body["hostSettings"]["limits"]["emails"] = json!({"maxPeriodic":max,"error":"Your monthly email allowance has been reached."});
    body["hostSettings"]["emailQuota"]["autoStop"] = body.pointer("/hostSettings/emailQuota/autoStop").cloned().unwrap_or(json!(true));
}

/// Reconcile staff limits and feature access on existing sites while preserving
/// email allowances, billing metadata, and protected mail configuration.
pub fn apply_plan_access(body: &mut Value, plan: &Plan) {
    let host = plan.host_settings("", "");
    for name in ["staff", "customIntegrations", "limitSocialWeb", "limitCustomFields", "limitStripeConnect", "limitAnalytics"] {
        if let Some(restriction) = host["limits"].get(name) {
            body["hostSettings"]["limits"][name] = restriction.clone();
        } else if let Some(limits) = body["hostSettings"]["limits"].as_object_mut() {
            limits.remove(name);
        }
    }
}

/// The `database` block for a tenant, in the form its CMS expects.
///
/// Knex's shape, which is what Ghost's config uses and Vuuut reads: a
/// `client` naming the engine and a `connection` holding either a filename or
/// a set of server fields.
///
/// The SQLite filename is deliberately *relative*. The tenant process runs
/// with its own directory as the working directory, so the whole site stays
/// one movable, archivable tree — which is what makes a backup a single zip
/// and a move between machines a single copy.
///
/// On MySQL that property is gone: the archive holds the files and the
/// database is on a server, so the two have to be captured together. See
/// [`crate::backup`].
pub fn database_block(creds: Option<&crate::tenant_db::Credentials>) -> Value {
    match creds {
        None => json!({
            "client": "sqlite3",
            "connection": { "filename": "content/data/ghost.db" }
        }),
        Some(c) => json!({
            "client": "mysql",
            "connection": {
                "host": c.host,
                "port": c.port,
                "user": c.user,
                "password": c.password,
                "database": c.database,
                // utf8mb4 or a post cannot hold an emoji, and the failure is a
                // truncated row rather than an error.
                "charset": "utf8mb4",
            }
        }),
    }
}

/// The tenant's transactional mail.
///
/// Inherits the platform's SMTP by default, so member signup confirmations and
/// password resets work the moment a site exists. A tenant's *newsletters* are
/// configured separately, either by the tenant or through the protected
/// hostSettings.managedMail policy.
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

/// Serialize read/modify/write operations so concurrent plan, domain and mail
/// changes cannot overwrite one another's protected settings.
pub async fn update(
    dir: &Path,
    env: &str,
    mutate: impl FnOnce(&mut Value),
) -> Result<bool> {
    static UPDATES: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = UPDATES.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    let mut body = read(dir, env).await?;
    let before = body.clone();
    mutate(&mut body);
    if body == before { return Ok(false); }
    write(dir, env, &body).await?;
    Ok(true)
}

/// Write the config file atomically.
///
/// Written to a temporary file and renamed, because a tenant may be starting
/// while this runs — and a process that reads a half-written config either
/// fails to boot or, worse, boots with partial limits. `rename` within a
/// directory is atomic, so a reader sees either the old file or the new one.
pub async fn write(dir: &Path, env: &str, body: &Value) -> Result<()> {
    let final_path = dir.join(format!("config.{env}.json"));
    let tmp_path = dir.join(format!(".config.{env}.{}.json.tmp", pro_core::ids::object_id()));
    let pretty = serde_json::to_string_pretty(body)?;
    use tokio::io::AsyncWriteExt;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&tmp_path).await?;
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600)).await?;
    file.write_all(pretty.as_bytes()).await?;
    file.sync_all().await?;
    drop(file);
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
            ..MailConfig::default()
        }
    }

    #[test]
    fn reconciles_existing_staff_and_feature_access_and_preserves_suspension() {
        let mut plan = pro_core::plans::default_plans().remove(0);
        plan.max_staff = Some(1);
        plan.custom_integrations = false;
        plan.social_web = false;
        plan.custom_fields = false;
        let mut body = build("https://site.example", 21000, &plan, "2026-01-01", "https://billing", &mail(), &database_block(None));
        let protected_mail = body["hostSettings"]["emailQuota"].clone();
        let terms = body["hostSettings"]["limits"]["emails"].clone();
        apply_plan_access(&mut body, &pro_core::plans::default_plans()[0]);
        for name in ["customIntegrations", "limitSocialWeb", "limitCustomFields", "limitStripeConnect", "limitAnalytics"] {
            assert!(body["hostSettings"]["limits"][name].is_null());
        }
        assert_eq!(body["hostSettings"]["limits"]["staff"]["max"], 3);
        apply_plan_access(&mut body, &pro_core::plans::default_plans()[1]);
        assert_eq!(body["hostSettings"]["limits"]["staff"]["max"], 7);
        assert_eq!(body["hostSettings"]["emailQuota"], protected_mail);
        assert_eq!(body["hostSettings"]["limits"]["emails"], terms);
        let suspended = pro_core::plans::suspended_plan();
        apply_host_settings(&mut body, suspended.host_settings("2026-01-01", "https://billing"));
        apply_plan_access(&mut body, &suspended);
        assert_eq!(body["hostSettings"]["limits"]["publicSiteAccess"]["disabled"], true);
        assert_eq!(body["hostSettings"]["limits"]["customIntegrations"]["disabled"], true);
        assert_eq!(body["hostSettings"]["limits"]["emails"]["maxPeriodic"], 0);
        assert!(body["hostSettings"]["limits"]["staff"].is_null());
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
            &database_block(None),
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
        let cfg = build("https://a.example.com", 21000, plan, "2026-01-01T00:00:00Z", "https://b/billing", &mail(), &database_block(None));

        assert!(cfg["hostSettings"].is_object());
        assert!(cfg["hostSettings"]["limits"].is_object());
        // Staff rather than members: members are uncapped on every shipped
        // plan now, and this needs a ceiling that is actually present to
        // prove the numbers survived as numbers.
        assert!(cfg["hostSettings"]["limits"]["staff"]["max"].is_i64());
        assert_eq!(cfg["hostSettings"]["subscription"]["start"], "2026-01-01T00:00:00Z");
        assert_eq!(cfg["hostSettings"]["billing"]["url"], "https://b/billing");
    }

    /// The tenant speaks plain HTTP on loopback, but its url has to describe
    /// the browser's view or it emits http:// links and non-secure cookies.
    #[test]
    fn url_is_the_public_one_not_the_loopback_one() {
        let plan = &pro_core::plans::default_plans()[0];
        let cfg = build("https://blog.customer.com", 21005, plan, "2026-01-01T00:00:00Z", "https://b/billing", &mail(), &database_block(None));
        assert!(cfg["url"].as_str().unwrap().starts_with("https://"));
        assert_eq!(cfg["server"]["host"], "127.0.0.1");
    }

    #[test]
    fn inherits_platform_smtp() {
        let plan = &pro_core::plans::default_plans()[0];
        let cfg = build("https://a.example.com", 21000, plan, "2026-01-01T00:00:00Z", "https://b/billing", &mail(), &database_block(None));
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
        let cfg = build("https://a.example.com", 21000, plan, "2026-01-01T00:00:00Z", "https://b/billing", &no_mail, &database_block(None));
        assert_eq!(cfg["mail"]["transport"], "Direct");

        let smtp_without_host = MailConfig { host: None, ..mail() };
        let cfg2 = build("https://a.example.com", 21000, plan, "2026-01-01T00:00:00Z", "https://b/billing", &smtp_without_host, &database_block(None));
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
            &database_block(None),
        );
        assert_eq!(cfg["hostSettings"]["limits"]["publicSiteAccess"]["disabled"], true);
    }

    /// With no MySQL configured, a tenant keeps its own file — and the path
    /// stays *relative*, which is what makes the whole site one movable tree.
    #[test]
    fn a_local_tenant_gets_a_relative_sqlite_path() {
        let db = database_block(None);
        assert_eq!(db["client"], "sqlite3");
        assert_eq!(db["connection"]["filename"], "content/data/ghost.db");
        assert!(
            !db["connection"]["filename"].as_str().unwrap().starts_with('/'),
            "an absolute path would pin the site to one machine"
        );
    }

    /// On MySQL the tenant is handed its own database and its own login, in
    /// the shape Ghost's config expects.
    #[test]
    fn a_mysql_tenant_gets_its_own_database_and_user() {
        let creds = crate::tenant_db::Credentials {
            database: "ghost_c32a6ef5d844be317eca55e2".into(),
            user: "ghost_c32a6ef5d844be317eca55e2".into(),
            password: "s3cret".into(),
            host: "db.internal".into(),
            port: 3307,
        };
        let db = database_block(Some(&creds));
        assert_eq!(db["client"], "mysql");
        assert_eq!(db["connection"]["database"], "ghost_c32a6ef5d844be317eca55e2");
        assert_eq!(db["connection"]["user"], "ghost_c32a6ef5d844be317eca55e2");
        assert_eq!(db["connection"]["host"], "db.internal");
        assert_eq!(db["connection"]["port"], 3307);
        // Without utf8mb4 a post holding an emoji is silently truncated.
        assert_eq!(db["connection"]["charset"], "utf8mb4");
    }

    /// The tenant's whole config is written to its directory, so a site must
    /// never be handed credentials for a database that is not its own.
    #[test]
    fn two_tenants_never_share_a_database() {
        let store = pro_core::TenantStore::MySqlServer {
            server: "mysql://root:r@db:3306".into(),
            prefix: "ghost_".into(),
        };
        let a = store.database_name("aaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = store.database_name("bbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        assert_ne!(a, b);
    }

    /// A tenant may be starting while this runs; a reader must never see a
    /// half-written file.
    #[tokio::test]
    async fn writes_atomically_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("pro-cfg-{}", pro_core::ids::object_id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let plan = &pro_core::plans::default_plans()[0];
        let body = build("https://a.example.com", 21000, plan, "2026-01-01T00:00:00Z", "https://b/billing", &mail(), &database_block(None));
        write(&dir, "production", &body).await.unwrap();

        let back = read(&dir, "production").await.unwrap();
        assert_eq!(back["url"], "https://a.example.com");
        assert!(back["hostSettings"]["limits"].is_object());

        // No temporary file left behind.
        let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            assert!(!entry.file_name().to_string_lossy().ends_with(".tmp"));
        }
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(tokio::fs::metadata(dir.join("config.production.json")).await.unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(dir.join("config.production.json").exists());

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn managed_mail_survives_plan_changes_and_is_removed_when_disabled() {
        let mut mail = mail();
        mail.tenant_provider = "ses".into();
        mail.ses = pro_core::config::SesConfig { endpoint: Some("example.fips.region.mail-manager-smtp.amazonaws.com".into()), max_send_rate: None, region: Some("us-east-1".into()), user: Some("ses-user".into()), pass: Some("ses-password".into()) };
        mail.validate_managed().unwrap();
        let plan = &pro_core::plans::default_plans()[0];
        let mut body = build("https://a.example.test", 21000, plan, "2026-01-01", "https://billing", &mail, &database_block(None));
        assert_eq!(body["hostSettings"]["managedMail"]["mail_transport"], "SES");
        assert_eq!(body["hostSettings"]["managedMail"]["ses_smtp_pass"], "ses-password");
        assert_eq!(body["hostSettings"]["managedMail"]["ses_smtp_endpoint"], "example.fips.region.mail-manager-smtp.amazonaws.com");
        apply_host_settings(&mut body, pro_core::plans::suspended_plan().host_settings("2026-01-01", "https://billing"));
        assert_eq!(body["hostSettings"]["managedMail"]["newsletter_transport"], "ses");
        assert_eq!(body["hostSettings"]["limits"]["publicSiteAccess"]["disabled"], true);
        mail.tenant_provider = "mailgun".into();
        mail.mailgun.api_key = Some("mg-key".into());
        mail.mailgun.domain = Some("mg.example.test".into());
        apply_managed_mail(&mut body, &mail);
        assert_eq!(body["hostSettings"]["managedMail"]["newsletter_transport"], "mailgun");
        mail.tenant_provider = "tenant".into();
        apply_managed_mail(&mut body, &mail);
        assert!(body["hostSettings"].get("managedMail").is_none());
        assert!(body["hostSettings"]["limits"].is_object());
    }

    #[tokio::test]
    async fn concurrent_mail_and_plan_changes_preserve_both() {
        let dir = std::env::temp_dir().join(format!("pro-mail-update-{}", pro_core::ids::object_id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        write(&dir, "production", &json!({"hostSettings":{"limits":{}}})).await.unwrap();
        let (mail, plan) = tokio::join!(
            update(&dir, "production", |body| body["hostSettings"]["managedMail"] = json!({"newsletter_transport":"ses"})),
            update(&dir, "production", |body| apply_host_settings(body, json!({"limits":{"staff":{"max":3}}}))),
        );
        mail.unwrap(); plan.unwrap();
        let body = read(&dir, "production").await.unwrap();
        assert_eq!(body["hostSettings"]["managedMail"]["newsletter_transport"], "ses");
        assert_eq!(body["hostSettings"]["limits"]["staff"]["max"], 3);
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

}
