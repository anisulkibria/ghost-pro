//! Transactional native analytics queue verification.
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn native_revisions_follow_committed_lifecycle_and_domain_changes() {
        let db = crate::open_in_memory().await.unwrap();
        let account = crate::accounts::create(&db, "click@test.local", "Click", "unused")
            .await
            .unwrap();
        let t = crate::tenants::create(
            &db,
            &account.id,
            "click",
            "Click",
            "starter",
            &pro_core::now_iso(),
            22000,
            23000,
        )
        .await
        .unwrap();
        let revision = |db: crate::Db, id: String| async move {
            sqlx::query_scalar::<_, i64>("SELECT revision FROM click_sync WHERE tenant_id=?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
        };
        let first = revision(db.clone(), t.id.clone()).await;
        let mut tx = db.begin().await.unwrap();
        sqlx::query("UPDATE tenants SET status='active' WHERE id=?")
            .bind(&t.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(first, revision(db.clone(), t.id.clone()).await);
        crate::tenants::set_status(&db, &t.id, pro_core::TenantStatus::Active)
            .await
            .unwrap();
        assert_eq!(first + 1, revision(db.clone(), t.id.clone()).await);
        // Operational traffic updates do not continuously rotate credentials.
        crate::tenants::touch(&db, &t.id).await.unwrap();
        assert_eq!(first + 1, revision(db.clone(), t.id.clone()).await);
        sqlx::query("UPDATE click_sync SET payload='snapshot',state='synced' WHERE tenant_id=?")
            .bind(&t.id)
            .execute(&db)
            .await
            .unwrap();
        crate::domains::add(
            &db,
            &t.id,
            "click.localhost",
            pro_core::DomainKind::Subdomain,
            true,
        )
        .await
        .unwrap();
        let (rev, payload, state): (i64, Option<String>, String) =
            sqlx::query_as("SELECT revision,payload,state FROM click_sync WHERE tenant_id=?")
                .bind(&t.id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(rev > first + 1);
        assert!(payload.is_none());
        assert_eq!(state, "pending");
        crate::tenants::suspend(&db, &t.id, "test").await.unwrap();
        let suspended = revision(db.clone(), t.id.clone()).await;
        crate::tenants::unsuspend(&db, &t.id).await.unwrap();
        assert_eq!(suspended + 1, revision(db.clone(), t.id.clone()).await);
        crate::tenants::mark_deleted(&db, &t.id).await.unwrap();
        assert_eq!(suspended + 2, revision(db.clone(), t.id.clone()).await);
    }
}
