//! Aggregate tenant email reports and operator delivery policy overrides.
use crate::Db;
use pro_core::{DbErrorExt, Result};
use serde_json::Value;

pub async fn policy(pool: &Db, tenant: &str) -> Result<Option<bool>> {
    let value: Option<i64> =
        sqlx::query_scalar("SELECT auto_stop FROM tenant_email_policy WHERE tenant_id=?")
            .bind(tenant)
            .fetch_optional(pool)
            .await
            .db_err()?;
    Ok(value.map(|n| n != 0))
}

pub async fn set_policy(pool: &Db, tenant: &str, auto_stop: Option<bool>) -> Result<()> {
    if let Some(value) = auto_stop {
        sqlx::query(&format!(
            "INSERT INTO tenant_email_policy(tenant_id,auto_stop) VALUES(?,?) {}",
            crate::dialect::upsert(&["tenant_id"], &["auto_stop"])
        ))
        .bind(tenant)
        .bind(if value { 1i64 } else { 0 })
        .execute(pool)
        .await
        .db_err()?;
    } else {
        sqlx::query("DELETE FROM tenant_email_policy WHERE tenant_id=?")
            .bind(tenant)
            .execute(pool)
            .await
            .db_err()?;
    }
    Ok(())
}

pub async fn record(pool: &Db, tenant: &str, report: &Value) -> Result<()> {
    sqlx::query(&format!(
        "INSERT INTO tenant_email_usage(tenant_id,report,sampled_at) VALUES(?,?,?) {}",
        crate::dialect::upsert(&["tenant_id"], &["report", "sampled_at"])
    ))
    .bind(tenant)
    .bind(report.to_string())
    .bind(pro_core::now_iso())
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

pub async fn latest(pool: &Db, tenant: &str) -> Result<Option<(Value, String)>> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT report,sampled_at FROM tenant_email_usage WHERE tenant_id=?")
            .bind(tenant)
            .fetch_optional(pool)
            .await
            .db_err()?;
    Ok(row.and_then(|(body, date)| {
        serde_json::from_str(&body)
            .ok()
            .map(|report| (report, date))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn site_policy_and_snapshots_are_scoped_to_the_tenant() {
        let pool=crate::pool::open_in_memory().await.unwrap();
        let account=crate::accounts::create(&pool,"email@example.test","Email","hash").await.unwrap();
        let tenant=crate::tenants::create(&pool,&account.id,"email-site","Email site","starter","2026-01-01",31000,31100).await.unwrap();
        assert_eq!(policy(&pool,&tenant.id).await.unwrap(),None);
        set_policy(&pool,&tenant.id,Some(false)).await.unwrap();assert_eq!(policy(&pool,&tenant.id).await.unwrap(),Some(false));
        record(&pool,&tenant.id,&serde_json::json!({"sent":5})).await.unwrap();
        assert_eq!(latest(&pool,&tenant.id).await.unwrap().unwrap().0["sent"],5);
        record(&pool,&tenant.id,&serde_json::json!({"sent":9})).await.unwrap();
        assert_eq!(latest(&pool,&tenant.id).await.unwrap().unwrap().0["sent"],9);
        assert!(latest(&pool,"another").await.unwrap().is_none());
        set_policy(&pool,&tenant.id,None).await.unwrap();assert_eq!(policy(&pool,&tenant.id).await.unwrap(),None);
    }
}
