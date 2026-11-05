//! Hostnames that route to a tenant.
//!
//! Two kinds, with very different trust. A **subdomain** is ours: issued with
//! the tenant, instantly live, and nobody else can claim it. A **custom
//! domain** is the customer's: it must be proved and pointed here before a
//! single request is served for it, because the alternative is that one
//! customer attaches `competitor.com` and the platform cheerfully serves their
//! content on it.
use pro_core::{
    ids, normalize_host, now_iso, DbErrorExt, Domain, DomainKind, DomainStatus, ProError, Result,
};
use crate::Db;
use sqlx::any::AnyRow;
use sqlx::Row;

fn map(row: &AnyRow) -> Domain {
    Domain {
        id: row.get("id"),
        tenant_id: row.get("tenant_id"),
        domain: row.get("domain"),
        kind: DomainKind::parse(row.get::<String, _>("kind").as_str())
            .unwrap_or(DomainKind::Custom),
        // An unreadable status reads as `Pending`, the state that serves no
        // traffic. A corrupt row must not become a live route.
        status: DomainStatus::parse(row.get::<String, _>("status").as_str())
            .unwrap_or(DomainStatus::Pending),
        is_primary: row.get::<i64, _>("is_primary") != 0,
        verification_token: row.get("verification_token"),
        verified_at: row.get("verified_at"),
        last_checked_at: row.get("last_checked_at"),
        last_error: row.get("last_error"),
        created_at: row.get("created_at"),
    }
}

const COLS: &str = "id, tenant_id, domain, kind, status, is_primary, verification_token, \
                    verified_at, last_checked_at, last_error, created_at";

/// Is this a hostname we could ever serve?
///
/// Shape only — whether it resolves here is a separate question, answered by
/// `pro_proxy::dns`. The rules are DNS's: labels of 1–63 characters, 253
/// overall, letters digits and hyphens, no leading or trailing hyphen per
/// label.
pub fn validate_hostname(host: &str) -> Result<String> {
    let h = normalize_host(host);
    if h.is_empty() {
        return Err(ProError::BadRequest("Enter a domain.".into()));
    }
    if h.len() > 253 {
        return Err(ProError::BadRequest("That domain is too long.".into()));
    }
    if !h.contains('.') {
        return Err(ProError::BadRequest(
            "Enter a full domain, like blog.example.com.".into(),
        ));
    }
    // An IP address is not a domain, and no certificate authority will issue
    // for one over the public ACME endpoints.
    if h.parse::<std::net::IpAddr>().is_ok() {
        return Err(ProError::BadRequest("Enter a domain name, not an IP address.".into()));
    }
    if h.contains('*') {
        return Err(ProError::BadRequest(
            "Wildcard domains are not supported. Add each hostname you want to use.".into(),
        ));
    }
    for label in h.split('.') {
        if label.is_empty() {
            return Err(ProError::BadRequest("That domain is not valid.".into()));
        }
        if label.len() > 63 {
            return Err(ProError::BadRequest("One part of that domain is too long.".into()));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(ProError::BadRequest(
                "Parts of a domain may not start or end with a hyphen.".into(),
            ));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(ProError::BadRequest(
                "Domains may only contain letters, numbers and hyphens. For an \
                 internationalised domain, enter its punycode form."
                    .into(),
            ));
        }
    }
    Ok(h)
}

/// Attach a hostname to a tenant.
///
/// A subdomain starts `Active` — the platform owns that DNS, so there is
/// nothing to prove. A custom domain starts `Pending` and gets a verification
/// token, and serves nothing until a DNS check promotes it.
pub async fn add(
    pool: &Db,
    tenant_id: &str,
    host: &str,
    kind: DomainKind,
    is_primary: bool,
) -> Result<Domain> {
    let domain = validate_hostname(host)?;
    let id = ids::object_id();
    let (status, token) = match kind {
        DomainKind::Subdomain => (DomainStatus::Active, String::new()),
        DomainKind::Custom => (DomainStatus::Pending, ids::domain_verification_token()),
    };
    let now = now_iso();

    let mut tx = pool.begin().await.db_err()?;
    // Exactly one primary per tenant. Demote the incumbent in the same
    // transaction, or a failure between the two leaves a tenant with two
    // canonical hostnames — and therefore an ambiguous `url`.
    if is_primary {
        sqlx::query("UPDATE domains SET is_primary = 0 WHERE tenant_id = ?")
            .bind(tenant_id)
            .execute(&mut *tx)
            .await
            .db_err()?;
    }

    let result = sqlx::query(
        "INSERT INTO domains (id, tenant_id, domain, kind, status, is_primary, \
         verification_token, verified_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(&domain)
    .bind(kind.as_str())
    .bind(status.as_str())
    .bind(i64::from(is_primary))
    .bind(&token)
    .bind(if status == DomainStatus::Active { Some(now.clone()) } else { None })
    .bind(&now)
    .execute(&mut *tx)
    .await;

    if let Err(e) = result {
        let msg = e.to_string();
        if crate::unique::is_unique_violation(&msg, &["domains_unique", "domains.domain"]) {
            // Deliberately does not say *which* tenant holds it. That would
            // let anybody map the platform's customers to their domains.
            return Err(ProError::Conflict(format!(
                "{domain} is already connected to a site on this platform."
            )));
        }
        return Err(ProError::Db(msg));
    }
    tx.commit().await.db_err()?;
    get(pool, &id).await
}

pub async fn get(pool: &Db, id: &str) -> Result<Domain> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM domains WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .db_err()?
        .ok_or_else(|| ProError::NotFound(format!("no domain {id}")))?;
    Ok(map(&row))
}

pub async fn find_by_host(pool: &Db, host: &str) -> Result<Option<Domain>> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM domains WHERE lower(domain) = lower(?)"))
        .bind(normalize_host(host))
        .fetch_optional(pool)
        .await
        .db_err()?;
    Ok(row.as_ref().map(map))
}

pub async fn list_for_tenant(pool: &Db, tenant_id: &str) -> Result<Vec<Domain>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM domains WHERE tenant_id = ? ORDER BY is_primary DESC, created_at"
    ))
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows.iter().map(map).collect())
}

/// The tenant's canonical hostname — what its Ghost `url` is set to.
pub async fn primary_for_tenant(pool: &Db, tenant_id: &str) -> Result<Option<Domain>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM domains WHERE tenant_id = ? AND is_primary = 1"
    ))
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .db_err()?;
    Ok(row.as_ref().map(map))
}

/// How many custom domains this tenant has, for the plan's allowance.
pub async fn count_custom(pool: &Db, tenant_id: &str) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM domains WHERE tenant_id = ? AND kind = 'custom'")
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .db_err()
}

/// Every hostname the edge should be prepared to serve.
///
/// Drives the proxy's routing table and the set of certificates to hold. Only
/// `active` domains, and only for tenants that are routable — a pending
/// domain has not been proved, and a certificate for it would be a
/// certificate for a hostname somebody else may control.
pub async fn all_routable(pool: &Db) -> Result<Vec<(String, String)>> {
    sqlx::query_as(
        "SELECT d.domain, d.tenant_id FROM domains d \
         JOIN tenants t ON t.id = d.tenant_id \
         WHERE d.status = 'active' \
           AND t.status IN ('active', 'suspended', 'pending_deletion')",
    )
    .fetch_all(pool)
    .await
    .db_err()
}

/// Domains waiting on DNS, for the verification sweeper.
pub async fn pending_verification(pool: &Db, limit: i64) -> Result<Vec<Domain>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM domains WHERE kind = 'custom' AND status IN ('pending', 'failed') \
         ORDER BY COALESCE(last_checked_at, '') LIMIT ?"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
    .db_err()?;
    Ok(rows.iter().map(map).collect())
}

pub async fn set_status(
    pool: &Db,
    id: &str,
    status: DomainStatus,
    error: Option<&str>,
) -> Result<()> {
    let now = now_iso();
    sqlx::query(
        "UPDATE domains SET status = ?, last_checked_at = ?, last_error = ?, \
         verified_at = CASE WHEN ? IN ('verified','active') AND verified_at IS NULL \
                           THEN ? ELSE verified_at END \
         WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(&now)
    .bind(error)
    .bind(status.as_str())
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await
    .db_err()?;
    Ok(())
}

/// Make one of a tenant's domains canonical.
///
/// Only a live domain may be promoted. Pointing a tenant's `url` at a hostname
/// that does not resolve here yet would break every link on the site the
/// moment it restarted.
pub async fn set_primary(pool: &Db, tenant_id: &str, domain_id: &str) -> Result<()> {
    let d = get(pool, domain_id).await?;
    if d.tenant_id != tenant_id {
        return Err(ProError::NotFound("no such domain on this site".into()));
    }
    if d.status != DomainStatus::Active {
        return Err(ProError::BadRequest(
            "That domain is not live yet. It can become the primary address once its DNS \
             has been verified."
                .into(),
        ));
    }
    let mut tx = pool.begin().await.db_err()?;
    sqlx::query("UPDATE domains SET is_primary = 0 WHERE tenant_id = ?")
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .db_err()?;
    sqlx::query("UPDATE domains SET is_primary = 1 WHERE id = ?")
        .bind(domain_id)
        .execute(&mut *tx)
        .await
        .db_err()?;
    tx.commit().await.db_err()?;
    Ok(())
}

/// Detach a domain.
///
/// The platform subdomain cannot be removed: it is the address of last resort,
/// the one that still works when a customer's DNS breaks or their registrar
/// lapses. Without it a tenant could become permanently unreachable, including
/// its admin, with no way back in.
pub async fn remove(pool: &Db, tenant_id: &str, domain_id: &str) -> Result<()> {
    let d = get(pool, domain_id).await?;
    if d.tenant_id != tenant_id {
        return Err(ProError::NotFound("no such domain on this site".into()));
    }
    if d.kind == DomainKind::Subdomain {
        return Err(ProError::BadRequest(
            "The built-in address cannot be removed — it is how your site stays reachable \
             if your own domain's DNS stops working."
                .into(),
        ));
    }
    let mut tx = pool.begin().await.db_err()?;
    sqlx::query("DELETE FROM domains WHERE id = ?")
        .bind(domain_id)
        .execute(&mut *tx)
        .await
        .db_err()?;
    // Removing the canonical hostname would leave the tenant with no `url`, so
    // the built-in subdomain takes over.
    if d.is_primary {
        sqlx::query(
            "UPDATE domains SET is_primary = 1 WHERE id = ( \
                SELECT id FROM domains WHERE tenant_id = ? \
                ORDER BY CASE kind WHEN 'subdomain' THEN 0 ELSE 1 END, created_at LIMIT 1)",
        )
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .db_err()?;
    }
    tx.commit().await.db_err()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (Db, String) {
        let pool = crate::pool::open_in_memory().await.unwrap();
        let account = crate::accounts::create(&pool, "o@example.com", "O", "h").await.unwrap();
        let tenant = crate::tenants::create(
            &pool, &account.id, "alice", "Alice", "starter", "2026-01-01T00:00:00Z", 21000, 21010,
        )
        .await
        .unwrap();
        // A tenant still provisioning is deliberately not routable, so these
        // tests put it live first.
        crate::tenants::set_status(&pool, &tenant.id, pro_core::TenantStatus::Active)
            .await
            .unwrap();
        // Every tenant gets its platform subdomain at provisioning time.
        add(&pool, &tenant.id, "alice.sites.example.com", DomainKind::Subdomain, true)
            .await
            .unwrap();
        (pool, tenant.id)
    }

    #[test]
    fn validates_hostnames() {
        assert_eq!(validate_hostname("Blog.Example.COM.").unwrap(), "blog.example.com");
        assert_eq!(validate_hostname("example.com:443").unwrap(), "example.com");
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("nodot").is_err());
        assert!(validate_hostname("1.2.3.4").is_err(), "an IP is not a domain");
        assert!(validate_hostname("*.example.com").is_err(), "no wildcards");
        assert!(validate_hostname("-bad.example.com").is_err());
        assert!(validate_hostname("bad-.example.com").is_err());
        assert!(validate_hostname("under_score.example.com").is_err());
        assert!(validate_hostname(&format!("{}.com", "a".repeat(64))).is_err());
        assert!(validate_hostname(&format!("{}.com", "a".repeat(63))).is_ok());
    }

    /// A subdomain is ours, so it is live at once. A custom domain is not, and
    /// must not serve a byte until its DNS is checked.
    #[tokio::test]
    async fn subdomains_are_live_custom_domains_are_not() {
        let (pool, tenant) = fixture().await;
        let sub = primary_for_tenant(&pool, &tenant).await.unwrap().unwrap();
        assert_eq!(sub.status, DomainStatus::Active);
        assert!(sub.verification_token.is_empty());

        let custom = add(&pool, &tenant, "blog.example.com", DomainKind::Custom, false)
            .await
            .unwrap();
        assert_eq!(custom.status, DomainStatus::Pending);
        assert!(custom.verification_token.starts_with("ghost-pro-verify="));
        assert!(custom.verified_at.is_none());

        // Only the live one is routable, so no traffic and no certificate for
        // an unproved hostname.
        let routable = all_routable(&pool).await.unwrap();
        assert_eq!(routable.len(), 1);
        assert_eq!(routable[0].0, "alice.sites.example.com");
    }

    /// One customer must not be able to attach a hostname another is serving.
    #[tokio::test]
    async fn a_hostname_belongs_to_one_tenant_only() {
        let (pool, tenant) = fixture().await;
        add(&pool, &tenant, "blog.example.com", DomainKind::Custom, false).await.unwrap();

        let account2 = crate::accounts::create(&pool, "two@example.com", "T", "h").await.unwrap();
        let other = crate::tenants::create(
            &pool, &account2.id, "bob", "Bob", "starter", "2026-01-01T00:00:00Z", 21000, 21010,
        )
        .await
        .unwrap();
        let err = add(&pool, &other.id, "BLOG.example.com", DomainKind::Custom, false)
            .await
            .unwrap_err();
        assert_eq!(err.status(), 409);
        // The message must not reveal whose site holds it.
        assert!(!err.to_string().contains("alice"));
    }

    #[tokio::test]
    async fn exactly_one_primary() {
        let (pool, tenant) = fixture().await;
        let custom = add(&pool, &tenant, "blog.example.com", DomainKind::Custom, true)
            .await
            .unwrap();
        let all = list_for_tenant(&pool, &tenant).await.unwrap();
        assert_eq!(all.iter().filter(|d| d.is_primary).count(), 1);
        assert_eq!(primary_for_tenant(&pool, &tenant).await.unwrap().unwrap().id, custom.id);
    }

    /// Promoting a hostname that does not resolve here yet would break every
    /// link on the site at its next restart.
    #[tokio::test]
    async fn only_a_live_domain_can_be_primary() {
        let (pool, tenant) = fixture().await;
        let custom = add(&pool, &tenant, "blog.example.com", DomainKind::Custom, false)
            .await
            .unwrap();
        assert_eq!(
            set_primary(&pool, &tenant, &custom.id).await.unwrap_err().status(),
            400
        );

        set_status(&pool, &custom.id, DomainStatus::Active, None).await.unwrap();
        set_primary(&pool, &tenant, &custom.id).await.unwrap();
        assert_eq!(primary_for_tenant(&pool, &tenant).await.unwrap().unwrap().id, custom.id);
    }

    /// Verification stamps a date once, and does not keep moving it.
    #[tokio::test]
    async fn records_verification_once() {
        let (pool, tenant) = fixture().await;
        let d = add(&pool, &tenant, "blog.example.com", DomainKind::Custom, false).await.unwrap();

        set_status(&pool, &d.id, DomainStatus::Failed, Some("no A record")).await.unwrap();
        let failed = get(&pool, &d.id).await.unwrap();
        assert_eq!(failed.status, DomainStatus::Failed);
        assert_eq!(failed.last_error.as_deref(), Some("no A record"));
        assert!(failed.verified_at.is_none());
        assert!(failed.last_checked_at.is_some());

        set_status(&pool, &d.id, DomainStatus::Active, None).await.unwrap();
        let first = get(&pool, &d.id).await.unwrap().verified_at;
        assert!(first.is_some());
        assert!(get(&pool, &d.id).await.unwrap().last_error.is_none(), "cleared");

        set_status(&pool, &d.id, DomainStatus::Active, None).await.unwrap();
        assert_eq!(get(&pool, &d.id).await.unwrap().verified_at, first, "not moved");
    }

    /// The built-in address is how a site stays reachable when a customer's
    /// own DNS breaks, so it cannot be detached.
    #[tokio::test]
    async fn the_platform_subdomain_cannot_be_removed() {
        let (pool, tenant) = fixture().await;
        let sub = primary_for_tenant(&pool, &tenant).await.unwrap().unwrap();
        assert_eq!(remove(&pool, &tenant, &sub.id).await.unwrap_err().status(), 400);
    }

    /// Removing the canonical hostname must leave a canonical hostname
    /// behind, or the tenant has no `url`.
    #[tokio::test]
    async fn removing_the_primary_falls_back_to_the_subdomain() {
        let (pool, tenant) = fixture().await;
        let custom = add(&pool, &tenant, "blog.example.com", DomainKind::Custom, true)
            .await
            .unwrap();
        assert!(get(&pool, &custom.id).await.unwrap().is_primary);

        remove(&pool, &tenant, &custom.id).await.unwrap();
        let primary = primary_for_tenant(&pool, &tenant).await.unwrap().unwrap();
        assert_eq!(primary.kind, DomainKind::Subdomain);
    }

    /// A customer must not be able to act on another tenant's domain by id.
    #[tokio::test]
    async fn operations_are_scoped_to_the_tenant() {
        let (pool, tenant) = fixture().await;
        let custom = add(&pool, &tenant, "blog.example.com", DomainKind::Custom, false)
            .await
            .unwrap();
        assert_eq!(remove(&pool, "someone-else", &custom.id).await.unwrap_err().status(), 404);
        assert_eq!(
            set_primary(&pool, "someone-else", &custom.id).await.unwrap_err().status(),
            404
        );
    }

    #[tokio::test]
    async fn counts_custom_domains_for_the_plan() {
        let (pool, tenant) = fixture().await;
        assert_eq!(count_custom(&pool, &tenant).await.unwrap(), 0, "the subdomain is not one");
        add(&pool, &tenant, "a.example.com", DomainKind::Custom, false).await.unwrap();
        add(&pool, &tenant, "b.example.com", DomainKind::Custom, false).await.unwrap();
        assert_eq!(count_custom(&pool, &tenant).await.unwrap(), 2);
    }

    /// A suspended site still routes — its own publicSiteAccess limit closes
    /// the public half, which is what lets the owner sign in and pay.
    #[tokio::test]
    async fn suspended_tenants_still_route() {
        let (pool, tenant) = fixture().await;
        crate::tenants::suspend(&pool, &tenant, "unpaid").await.unwrap();
        assert_eq!(all_routable(&pool).await.unwrap().len(), 1);

        crate::tenants::mark_deleted(&pool, &tenant).await.unwrap();
        assert!(all_routable(&pool).await.unwrap().is_empty(), "a deleted site does not");
    }

    #[tokio::test]
    async fn finds_pending_work_oldest_first() {
        let (pool, tenant) = fixture().await;
        let a = add(&pool, &tenant, "a.example.com", DomainKind::Custom, false).await.unwrap();
        add(&pool, &tenant, "b.example.com", DomainKind::Custom, false).await.unwrap();
        set_status(&pool, &a.id, DomainStatus::Pending, None).await.unwrap();

        let pending = pending_verification(&pool, 10).await.unwrap();
        assert_eq!(pending.len(), 2);
        // The never-checked one sorts first.
        assert_eq!(pending[0].domain, "b.example.com");
    }

    #[tokio::test]
    async fn finds_by_host_case_insensitively() {
        let (pool, _tenant) = fixture().await;
        assert!(find_by_host(&pool, "ALICE.sites.example.com.").await.unwrap().is_some());
        assert!(find_by_host(&pool, "nope.example.com").await.unwrap().is_none());
    }
}
