//! Durable native analytics reconciliation. Tenant/domain triggers mark changes
//! in the same transaction; no CMS schema or platform credential crosses into
//! the tenant. Snapshots are replayed byte-for-byte after uncertain delivery.
use pro_core::{Config, DbErrorExt, ProError, Result, TenantStatus};
use pro_db::Db;
use pro_supervisor::Supervisor;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub base_url: String,
    pub platform_key: String,
    #[serde(default)]
    pub plan_map: std::collections::HashMap<String, String>,
    #[serde(default = "free")]
    pub default_plan: String,
    #[serde(default = "consent")]
    pub consent_required: bool,
    #[serde(default)]
    pub engagement: bool,
    #[serde(default)]
    pub persistent_identity: bool,
}
fn free() -> String {
    "free".into()
}
fn consent() -> bool {
    true
}
impl Settings {
    pub fn load(c: &Config) -> Result<Option<Self>> {
        if c.click.is_null() || c.click.get("disabled") == Some(&Value::Bool(true)) {
            return Ok(None);
        }
        let s: Self = serde_json::from_value(c.click.clone())?;
        if s.persistent_identity && !s.consent_required {return Err(ProError::BadRequest("Persistent Click identity requires consent".into()));}
        let u = reqwest::Url::parse(&s.base_url)
            .map_err(|_| ProError::BadRequest("Click needs an HTTPS origin".into()))?;
        let local = matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if !(u.scheme() == "https" || c.env == "development" && local && u.scheme() == "http")
            || u.host_str().is_none()
            || u.path() != "/"
            || u.query().is_some()
            || u.fragment().is_some()
            || !u.username().is_empty()
            || u.password().is_some()
            || s.platform_key.len() < 32
            || s.platform_key.len() > 256
        {
            return Err(ProError::BadRequest(
                "Invalid native Click server or platform credential".into(),
            ));
        }
        Ok(Some(s))
    }
}
#[derive(Serialize, Deserialize)]
struct Desired {
    revision: i64,
    name: String,
    origin: String,
    plan_id: String,
    state: String,
}
#[derive(Deserialize)]
struct Provisioned {
    website_id: Option<String>,
    collection_id: Option<String>,
    integration_key: Option<String>,
    verification_token: Option<String>,
    verified: bool,
    revision: i64,
    state: String,
}
fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("Rustls client")
}
/// Reconcile existing tenants after enabling Click or rotating the platform
/// credential. The increment ensures new deterministic per-site credentials.
pub async fn reconcile(db: &Db) -> Result<()> {
    // Explicit per-row portable insert avoids relying on INSERT SELECT upsert
    // parsing differences between SQLite and MySQL.
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM tenants")
        .fetch_all(db)
        .await
        .db_err()?;
    for id in ids {
        let sql = format!(
            "INSERT INTO click_sync(tenant_id) VALUES(?) {}",
            pro_db::dialect::upsert(&["tenant_id"], &["tenant_id"])
        );
        sqlx::query(&sql).bind(id).execute(db).await.db_err()?;
    }
    sqlx::query("UPDATE click_sync SET revision=revision+1,payload=NULL,state='pending',attempts=0,available_at='',last_error=NULL").execute(db).await.db_err()?;
    Ok(())
}
/// One worker per control plane. A new lifecycle write can supersede an
/// in-flight request; conditional completion never acknowledges that new work.
pub async fn sync_once(db: &Db, config: &Arc<Config>, supervisor: &Supervisor) -> Result<bool> {
    let Some(settings) = Settings::load(config)? else {
        return Ok(false);
    };
    type PendingSync = (String, i64, i64, Option<String>, i64, i64);
    let candidate:Option<PendingSync>=sqlx::query_as("SELECT c.tenant_id,c.revision,c.enabled,c.payload,c.attempts,c.incarnation FROM click_sync c JOIN tenants t ON t.id=c.tenant_id WHERE c.state IN ('pending','waiting_dns') AND c.available_at<=? AND t.status NOT IN ('provisioning','failed') ORDER BY c.available_at,c.tenant_id LIMIT 1")
        .bind(pro_core::now_iso()).fetch_optional(db).await.db_err()?;
    let Some((id, revision, enabled, payload, attempts, incarnation)) = candidate else {
        return Ok(false);
    };
    let outcome:Result<bool>=async {
    let tenant = pro_db::tenants::get(db, &id).await?;
    let payload = if let Some(p) = payload {
        p
    } else {
        let domain = pro_db::domains::primary_for_tenant(db, &id).await?;
        let origin = if let Some(d) = domain {
            let scheme = if config.public_tls() { "https" } else { "http" };
            match config.edge_port() {
                80 | 443 => format!("{scheme}://{}", d.domain),
                p => format!("{scheme}://{}:{p}", d.domain),
            }
        } else {
            config.tenant_url(&tenant.slug)
        };
        let desired = Desired {
            revision,
            name: tenant.name.clone(),
            origin,
            plan_id: settings
                .plan_map
                .get(&tenant.plan_slug)
                .cloned()
                .unwrap_or_else(|| settings.default_plan.clone()),
            state: if tenant.status == TenantStatus::Deleted {
                "deleted"
            } else if enabled == 0
                || matches!(
                    tenant.status,
                    TenantStatus::Suspended | TenantStatus::PendingDeletion
                )
            {
                "suspended"
            } else {
                "active"
            }
            .into(),
        };
        let encoded = serde_json::to_string(&desired)?;
        let result = sqlx::query(
            "UPDATE click_sync SET payload=? WHERE tenant_id=? AND revision=? AND payload IS NULL",
        )
        .bind(&encoded)
        .bind(&id)
        .bind(revision)
        .execute(db)
        .await
        .db_err()?;
        if result.rows_affected() == 0 {
            return Ok(true);
        }
        encoded
    };
    let desired: Desired = serde_json::from_str(&payload)?;
    sqlx::query("UPDATE click_sync SET attempts=attempts+1 WHERE tenant_id=? AND revision=?")
        .bind(&id)
        .bind(revision)
        .execute(db)
        .await
        .db_err()?;
    let native_id=if incarnation==0 {id.clone()} else {format!("{id}-restored-{incarnation}")};
    let result = deliver(&settings, &native_id, &payload).await;
    let remote = match result {
        Ok(r) => r,
        Err(_) => {
            // Keep transport URLs, headers and bodies out of errors/logs.
            let next = (chrono::Utc::now()
                + chrono::Duration::seconds(10 * 2i64.pow((attempts + 1).min(10) as u32)))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
            sqlx::query("UPDATE click_sync SET attempts=?,state=?,available_at=?,last_error='Click unavailable or revision refused; check credentials and retry' WHERE tenant_id=? AND revision=?")
                .bind(attempts+1).bind(if attempts>=19 {"dead"} else {"pending"}).bind(next).bind(&id).bind(revision).execute(db).await.db_err()?;
            return Ok(true);
        }
    };
    if remote.revision != revision || remote.state != desired.state {
        return Err(ProError::Unavailable(
            "Click returned a mismatched lifecycle revision".into(),
        ));
    }
    let current: i64 = sqlx::query_scalar("SELECT revision FROM click_sync WHERE tenant_id=?")
        .bind(&id)
        .fetch_one(db)
        .await
        .db_err()?;
    if current != revision {
        return Ok(true);
    }
    let mut verified = remote.verified;
    if !verified && desired.state == "active" {
        if let (Some(website), Some(key)) = (&remote.website_id, &remote.integration_key) {
            verified = http()
                .post(format!(
                    "{}/native/v1/websites/{website}/verify",
                    settings.base_url.trim_end_matches('/')
                ))
                .bearer_auth(key)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
        }
    }
    // Write only tenant-scoped credentials; platform_key is never copied.
    if tenant.status != TenantStatus::Deleted {
        let connection = if enabled != 0 && desired.state == "active" {
            json!({"baseUrl":settings.base_url,"websiteId":remote.website_id,"collectionId":remote.collection_id,"integrationKey":remote.integration_key,"consentRequired":settings.consent_required,"engagement":settings.engagement,"persistentIdentity":settings.persistent_identity})
        } else {
            // Preserve management during suspension/disconnect too.
            json!({"disabled":true})
        };
        let changed =
            crate::config_file::update(&config.tenant_dir(&id), &config.tenants.env, |body| {
                body["hostSettings"]["click"] = connection
            })
            .await?;
        // If writing succeeded but the process died before restart/completion,
        // retrying must still reload the credentials already on disk.
        if (changed || attempts > 0) && supervisor.is_running(&id) {
            supervisor.restart(&tenant).await?;
        }
    }
    let waiting = !verified && desired.state == "active";
    let next = (chrono::Utc::now() + chrono::Duration::seconds(60))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    sqlx::query("UPDATE click_sync SET synced_revision=?,state=?,available_at=?,attempts=0,last_error=?,website_id=?,verification_token=? WHERE tenant_id=? AND revision=?")
        .bind(revision).bind(if waiting {"waiting_dns"} else {"synced"}).bind(next)
        .bind(if waiting {Some("Publish the website verification TXT record, then retry" )} else {None})
        .bind(remote.website_id).bind(remote.verification_token).bind(&id).bind(revision).execute(db).await.db_err()?;
    Ok(true)
    }.await;
    if outcome.is_err() {
        let next = (chrono::Utc::now()
            + chrono::Duration::seconds(10 * 2i64.pow((attempts + 1).min(10) as u32)))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
        sqlx::query("UPDATE click_sync SET attempts=?,state=?,available_at=?,last_error='Tenant configuration or restart failed; restore its files and retry sync' WHERE tenant_id=? AND revision=?")
            .bind(attempts+1).bind(if attempts>=19 {"dead"} else {"pending"}).bind(next).bind(&id).bind(revision).execute(db).await.db_err()?;
        return Ok(true);
    }
    outcome
}
async fn deliver(s: &Settings, id: &str, payload: &str) -> Result<Provisioned> {
    let response = http()
        .put(format!(
            "{}/native/v1/tenants/{id}",
            s.base_url.trim_end_matches('/')
        ))
        .bearer_auth(&s.platform_key)
        .header("content-type", "application/json")
        .body(payload.to_owned())
        .send()
        .await
        .map_err(|_| ProError::Unavailable("Click is unavailable".into()))?;
    if !response.status().is_success() {
        return Err(ProError::Unavailable("Click refused provisioning".into()));
    }
    response
        .json()
        .await
        .map_err(|_| ProError::Unavailable("Invalid Click response".into()))
}
/// Owner/customer actions change desired state durably; the worker performs
/// the network operation. Rotation is simply a new lifecycle revision.
pub async fn action(db: &Db, id: &str, action: &str) -> Result<()> {
    if !matches!(action, "retry" | "rotate" | "connect" | "disconnect") {
        return Err(ProError::BadRequest("Unknown Click action".into()));
    }
    sqlx::query("UPDATE click_sync SET revision=revision+1,enabled=CASE WHEN ?='disconnect' THEN 0 WHEN ?='connect' THEN 1 ELSE enabled END,payload=NULL,state='pending',attempts=0,available_at='',last_error=NULL WHERE tenant_id=?")
        .bind(action).bind(action).bind(id).execute(db).await.db_err()?;
    Ok(())
}
