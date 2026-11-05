//! Reading and writing the operator's settings, and keeping the live view
//! fresh.
//!
//! [`SettingsStore`] holds the merged [`Effective`] view behind an
//! `RwLock<Arc<…>>`. Every request clones the `Arc` — one atomic increment,
//! no lock held while a request runs — and a save swaps in a whole new view.
//! That is what lets the operator change payment provider and have the next
//! request use it, with no restart and no per-read database hit.
use pro_core::settings::{self, Effective};
use pro_core::{Config, DbErrorExt, ProError, Result, StaffRole};
use serde_json::Value;
use crate::Db;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

pub struct SettingsStore {
    config: Arc<Config>,
    current: RwLock<Arc<Effective>>,
}

impl SettingsStore {
    /// Read every override and build the live view.
    pub async fn load(pool: &Db, config: Arc<Config>) -> Result<Self> {
        let overrides = read_all(pool).await?;
        let effective = Arc::new(Effective::new(config.clone(), overrides));
        Ok(Self { config, current: RwLock::new(effective) })
    }

    /// The live settings. Cheap; call it per request rather than holding the
    /// result across an await where it could go stale.
    pub fn current(&self) -> Arc<Effective> {
        // A poisoned lock means a writer panicked mid-swap. The stored value
        // is still a complete, consistent view — the swap is a single
        // assignment — so recovering is correct and strictly better than
        // taking the whole platform down over it.
        match self.current.read() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Apply a batch of changes as one transaction, then rebuild the view.
    ///
    /// A batch rather than one call per key because the console submits a
    /// whole form, and a half-applied form is the state that breaks things:
    /// a provider switched on with its credentials not yet saved would send
    /// every new customer to a checkout that errors. Either the form applies
    /// or none of it does.
    ///
    /// `actor_role` is checked against each key's declared minimum before
    /// anything is written, so a submission carrying a field the actor may not
    /// set is refused outright rather than partly honoured.
    pub async fn save(
        &self,
        pool: &Db,
        changes: &[(String, Option<Value>)],
        actor_role: StaffRole,
        actor: &str,
    ) -> Result<()> {
        // Authorise every key first. Rendering a field read-only in the
        // console is presentation; this is the control.
        for (key, _) in changes {
            settings::authorize(key, actor_role)?;
        }

        // Refuse a change that would leave the platform unable to charge
        // anybody. This is the one cross-field rule worth enforcing here
        // rather than in the UI: selecting a provider whose keys are absent
        // breaks every new checkout the moment it is saved, and the operator
        // would not find out until a customer did.
        if let Some(provider) = changes
            .iter()
            .find(|(k, _)| k == "billing.provider")
            .and_then(|(_, v)| v.as_ref())
            .and_then(Value::as_str)
        {
            self.check_provider_switch(provider, changes)?;
        }

        let now = pro_core::now_iso();
        let mut tx = pool.begin().await.db_err()?;
        for (key, value) in changes {
            match value {
                Some(v) => {
                    let encoded = serde_json::to_string(v)?;
                    // `key` is a reserved word in MySQL, and the upsert tail is
                    // spelled differently by each engine; both come from
                    // `crate::dialect` so this statement reads the same either
                    // way. Single-key table, which is what makes MySQL's
                    // "any unique key" form equivalent to SQLite's.
                    sqlx::query(&format!(
                        "INSERT INTO platform_settings (`key`, value, updated_at, updated_by) \
                         VALUES (?, ?, ?, ?) {}",
                        crate::dialect::upsert(&["key"], &["value", "updated_at", "updated_by"])
                    ))
                    .bind(key)
                    .bind(&encoded)
                    .bind(&now)
                    .bind(actor)
                    .execute(&mut *tx)
                    .await
                    .db_err()?;
                }
                // Clearing a field deletes the override so the config file
                // shows through again, rather than storing a blank that would
                // mask it.
                None => {
                    sqlx::query("DELETE FROM platform_settings WHERE `key` = ?")
                        .bind(key)
                        .execute(&mut *tx)
                        .await
                        .db_err()?;
                }
            }
        }
        tx.commit().await.db_err()?;

        self.refresh(pool).await
    }

    /// Would this provider be usable once the rest of the form is applied?
    ///
    /// The credentials may be arriving in the *same* submission as the switch,
    /// so the check runs against the view the form would produce, not the one
    /// currently in force.
    fn check_provider_switch(
        &self,
        provider: &str,
        changes: &[(String, Option<Value>)],
    ) -> Result<()> {
        if provider == "none" {
            return Ok(());
        }
        let live = self.current();
        let mut merged: BTreeMap<String, Value> = live.overrides().clone();
        for (key, value) in changes {
            match value {
                Some(v) => merged.insert(key.clone(), v.clone()),
                None => merged.remove(key),
            };
        }
        let projected = Effective::new(self.config.clone(), merged);
        if projected.provider_is_configured(provider) {
            Ok(())
        } else {
            Err(ProError::BadRequest(format!(
                "{provider} is missing credentials, so new checkouts would fail. \
                 Fill in its keys in the same save, or leave the provider unchanged."
            )))
        }
    }

    /// Re-read the overrides and swap in a fresh view.
    pub async fn refresh(&self, pool: &Db) -> Result<()> {
        let overrides = read_all(pool).await?;
        let next = Arc::new(Effective::new(self.config.clone(), overrides));
        match self.current.write() {
            Ok(mut g) => *g = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
        Ok(())
    }

    /// The file-and-environment layer, for the parts that are not
    /// operator-editable.
    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }
}

async fn read_all(pool: &Db) -> Result<BTreeMap<String, Value>> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT `key`, value FROM platform_settings")
        .fetch_all(pool)
        .await
        .db_err()?;
    let mut out = BTreeMap::new();
    for (key, raw) in rows {
        // A setting that has been removed from the registry is left in the
        // table but ignored, so a downgrade does not destroy an operator's
        // configuration.
        if settings::def(&key).is_none() {
            tracing::debug!("ignoring stored setting {key}, which this version does not define");
            continue;
        }
        match serde_json::from_str::<Value>(&raw) {
            Ok(v) => {
                out.insert(key, v);
            }
            // One unreadable row must not stop the platform booting; the
            // config fallback for that key is used instead.
            Err(e) => tracing::warn!("setting {key} holds unreadable JSON and was ignored: {e}"),
        }
    }
    Ok(out)
}

/// Every setting's current value, for the console's form.
pub struct SettingView {
    pub def: &'static settings::SettingDef,
    /// What to put in the input. Always empty for a secret.
    pub value: String,
    /// Whether a secret has a value stored, since the value itself is never
    /// shown.
    pub secret_set: bool,
    /// Set in the console, as opposed to coming from the config file. Shown
    /// so an overridden value is visibly overridden.
    pub overridden: bool,
    /// Whether the viewer may change it. A setting above their role renders
    /// locked rather than hidden — a field that vanishes looks like a bug,
    /// one that is visibly locked explains itself.
    pub editable: bool,
}

/// Build the console's form model for `role`.
pub fn view_for(effective: &Effective, role: StaffRole) -> Vec<SettingView> {
    settings::REGISTRY
        .iter()
        .map(|def| SettingView {
            def,
            value: effective.form_value(def.key),
            secret_set: matches!(def.kind, settings::Kind::Secret)
                && effective.secret_is_set(def.key),
            overridden: effective.is_overridden(def.key),
            editable: role >= def.min_role,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn fixture() -> (Db, SettingsStore) {
        let pool = crate::pool::open_in_memory().await.unwrap();
        let mut cfg: Config = serde_json::from_value(json!({
            "url": "https://pro.example.com",
            "subdomainBase": "sites.example.com",
            "billing": { "provider": "stripe", "stripe": { "secretKey": "sk_file" } },
        }))
        .unwrap();
        cfg.plans = pro_core::plans::default_plans();
        let store = SettingsStore::load(&pool, Arc::new(cfg)).await.unwrap();
        (pool, store)
    }

    #[tokio::test]
    async fn starts_from_the_config_file() {
        let (_pool, store) = fixture().await;
        assert_eq!(store.current().billing_provider(), "stripe");
    }

    /// The point of the whole module: a saved change is in force on the next
    /// read, with no restart.
    #[tokio::test]
    async fn a_save_takes_effect_immediately() {
        let (pool, store) = fixture().await;
        store
            .save(
                &pool,
                &[
                    ("billing.paddle.apiKey".into(), Some(json!("pdl_key"))),
                    ("billing.provider".into(), Some(json!("paddle"))),
                ],
                StaffRole::Owner,
                "staff:1",
            )
            .await
            .unwrap();
        assert_eq!(store.current().billing_provider(), "paddle");
        assert_eq!(store.current().paddle().api_key.as_deref(), Some("pdl_key"));
    }

    /// Only the owner may touch the money settings, and the refusal must come
    /// before anything is written.
    #[tokio::test]
    async fn an_admin_cannot_change_the_provider() {
        let (pool, store) = fixture().await;
        let err = store
            .save(
                &pool,
                &[("billing.provider".into(), Some(json!("none")))],
                StaffRole::Admin,
                "staff:2",
            )
            .await
            .expect_err("admins may not change billing");
        assert_eq!(err.status(), 403);
        assert_eq!(store.current().billing_provider(), "stripe", "nothing was written");
    }

    /// An admin still runs the platform day to day.
    #[tokio::test]
    async fn an_admin_can_change_operations() {
        let (pool, store) = fixture().await;
        store
            .save(
                &pool,
                &[("tenants.idleTimeoutSecs".into(), Some(json!(60)))],
                StaffRole::Admin,
                "staff:2",
            )
            .await
            .unwrap();
        assert_eq!(store.current().idle_timeout_secs(), 60);
    }

    /// Switching to a provider with no credentials would break every new
    /// checkout, so it is refused.
    #[tokio::test]
    async fn refuses_a_switch_to_an_unconfigured_provider() {
        let (pool, store) = fixture().await;
        let err = store
            .save(
                &pool,
                &[("billing.provider".into(), Some(json!("lemonsqueezy")))],
                StaffRole::Owner,
                "staff:1",
            )
            .await
            .expect_err("no Lemon Squeezy keys");
        assert_eq!(err.status(), 400);
        assert_eq!(store.current().billing_provider(), "stripe");
    }

    /// …but the credentials may arrive in the same submission as the switch,
    /// which is how the form is actually filled in.
    #[tokio::test]
    async fn allows_a_switch_when_keys_arrive_together() {
        let (pool, store) = fixture().await;
        store
            .save(
                &pool,
                &[
                    ("billing.provider".into(), Some(json!("lemonsqueezy"))),
                    ("billing.lemonsqueezy.apiKey".into(), Some(json!("ls_key"))),
                    ("billing.lemonsqueezy.storeId".into(), Some(json!("4242"))),
                ],
                StaffRole::Owner,
                "staff:1",
            )
            .await
            .unwrap();
        assert_eq!(store.current().billing_provider(), "lemonsqueezy");
    }

    /// A form must apply whole or not at all — a provider switched on without
    /// its keys is the state that breaks checkout.
    #[tokio::test]
    async fn a_rejected_batch_writes_nothing() {
        let (pool, store) = fixture().await;
        let _ = store
            .save(
                &pool,
                &[
                    ("platform.name".into(), Some(json!("Should not persist"))),
                    ("billing.provider".into(), Some(json!("paddle"))),
                ],
                StaffRole::Owner,
                "staff:1",
            )
            .await
            .expect_err("paddle has no key");
        assert_ne!(store.current().platform_name(), "Should not persist");
    }

    /// Clearing an override lets the config file show through again.
    #[tokio::test]
    async fn clearing_falls_back_to_the_file() {
        let (pool, store) = fixture().await;
        store
            .save(&pool, &[("platform.name".into(), Some(json!("Acme Hosting")))], StaffRole::Admin, "s")
            .await
            .unwrap();
        assert_eq!(store.current().platform_name(), "Acme Hosting");
        assert!(store.current().is_overridden("platform.name"));

        store.save(&pool, &[("platform.name".into(), None)], StaffRole::Admin, "s").await.unwrap();
        assert_eq!(store.current().platform_name(), "Ghost Pro", "back to the default");
        assert!(!store.current().is_overridden("platform.name"));
    }

    /// Settings survive a restart, which is what makes the console the source
    /// of truth rather than a cache.
    #[tokio::test]
    async fn overrides_survive_a_reload() {
        let (pool, store) = fixture().await;
        store
            .save(&pool, &[("signup.enabled".into(), Some(json!(false)))], StaffRole::Admin, "s")
            .await
            .unwrap();
        let reloaded = SettingsStore::load(&pool, store.config().clone()).await.unwrap();
        assert!(!reloaded.current().signup_enabled());
    }

    /// A stored key this version no longer defines is ignored rather than
    /// fatal, so a downgrade does not destroy a configuration.
    #[tokio::test]
    async fn ignores_unknown_stored_keys() {
        let (pool, store) = fixture().await;
        sqlx::query("INSERT INTO platform_settings (`key`, value, updated_at) VALUES (?, ?, ?)")
            .bind("billing.someFutureThing")
            .bind("\"x\"")
            .bind(pro_core::now_iso())
            .execute(&pool)
            .await
            .unwrap();
        store.refresh(&pool).await.unwrap();
        assert_eq!(store.current().billing_provider(), "stripe");
    }

    /// Unreadable JSON in one row must not stop the platform booting.
    #[tokio::test]
    async fn survives_a_corrupt_row() {
        let (pool, store) = fixture().await;
        sqlx::query("INSERT INTO platform_settings (`key`, value, updated_at) VALUES (?, ?, ?)")
            .bind("platform.name")
            .bind("{not json")
            .bind(pro_core::now_iso())
            .execute(&pool)
            .await
            .unwrap();
        store.refresh(&pool).await.unwrap();
        assert_eq!(store.current().platform_name(), "Ghost Pro", "fell back to the default");
    }

    /// The console's form model: secrets write-only, locked fields visible.
    #[tokio::test]
    async fn builds_the_console_form() {
        let (pool, store) = fixture().await;
        store
            .save(
                &pool,
                &[("billing.stripe.secretKey".into(), Some(json!("sk_live_x")))],
                StaffRole::Owner,
                "s",
            )
            .await
            .unwrap();

        let eff = store.current();
        let admin_view = view_for(&eff, StaffRole::Admin);
        let secret = admin_view.iter().find(|v| v.def.key == "billing.stripe.secretKey").unwrap();
        assert_eq!(secret.value, "", "a secret is never sent to the browser");
        assert!(secret.secret_set, "but the console says one is stored");
        assert!(!secret.editable, "and an admin cannot change it");

        let owner_view = view_for(&eff, StaffRole::Owner);
        assert!(owner_view.iter().all(|v| v.editable));
        // Every setting is still listed for an admin, just locked.
        assert_eq!(admin_view.len(), owner_view.len());
    }
}
