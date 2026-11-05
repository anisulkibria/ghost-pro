//! Deciding which tenant a request belongs to.
//!
//! Every request that arrives at the edge is for one of three things, and the
//! `Host` header is all there is to tell them apart:
//!
//! * the platform itself — the dashboard, the console, the pricing page;
//! * a tenant, by its platform subdomain;
//! * a tenant, by a custom domain the customer attached.
//!
//! The lookup has to be fast, because it happens on every request including
//! every image a site serves, and it has to be *correct*, because getting it
//! wrong serves one customer's site on another's domain. So it is a cache in
//! front of a unique index: the database is the authority, and the cache is
//! invalidated explicitly whenever routing changes rather than merely expiring.
use pro_core::{normalize_host, Tenant};
use pro_db::Db;
use std::collections::HashMap;
use std::sync::RwLock;

/// What a hostname resolves to.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// The platform's own web surface.
    Platform,
    /// A tenant site.
    Tenant { tenant_id: String },
    /// Nothing here. A domain that was never attached, or was removed.
    ///
    /// Cached like any other answer: an unknown host is the commonest thing an
    /// internet-facing edge is asked about — scanners, stale DNS, somebody
    /// else's misconfigured CNAME — and querying the database for each one
    /// would make that traffic a denial of service.
    Unknown,
}

/// Hostname → route, with an explicit invalidation.
pub struct Router {
    platform_host: String,
    subdomain_base: String,
    cache: RwLock<HashMap<String, Route>>,
}

impl Router {
    pub fn new(platform_host: String, subdomain_base: String) -> Self {
        Self {
            platform_host: normalize_host(&platform_host),
            subdomain_base: subdomain_base.trim_matches('.').to_ascii_lowercase(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Resolve a `Host` header.
    pub async fn resolve(&self, pool: &Db, host: &str) -> Route {
        let host = normalize_host(host);
        if host.is_empty() {
            return Route::Unknown;
        }
        if host == self.platform_host {
            return Route::Platform;
        }

        if let Some(hit) = self.cached(&host) {
            return hit;
        }

        // The domains table is the authority — it holds the platform
        // subdomains too, written at provisioning time. Parsing the slug out
        // of the hostname instead would route to a tenant whose subdomain row
        // had been removed.
        let route = match pro_db::domains::find_by_host(pool, &host).await {
            Ok(Some(domain)) => {
                // Only a live domain routes. A pending custom domain has not
                // been proved to belong to this customer yet.
                if domain.status == pro_core::DomainStatus::Active {
                    Route::Tenant { tenant_id: domain.tenant_id }
                } else {
                    Route::Unknown
                }
            }
            Ok(None) => Route::Unknown,
            Err(e) => {
                // A database failure must not be cached as "unknown" — that
                // would turn a momentary blip into every site on the platform
                // being down until something invalidated the cache.
                tracing::error!("routing lookup for {host} failed: {e}");
                return Route::Unknown;
            }
        };

        self.store(host, route.clone());
        route
    }

    fn cached(&self, host: &str) -> Option<Route> {
        match self.cache.read() {
            Ok(c) => c.get(host).cloned(),
            Err(p) => p.into_inner().get(host).cloned(),
        }
    }

    fn store(&self, host: String, route: Route) {
        let mut cache = match self.cache.write() {
            Ok(c) => c,
            Err(p) => p.into_inner(),
        };
        // A bound, so that scanner traffic against random hostnames cannot
        // grow this without limit. Cleared wholesale rather than evicted
        // one-by-one: the entries are cheap to rebuild and an LRU here would
        // cost more than it saves.
        if cache.len() > 10_000 {
            cache.clear();
        }
        cache.insert(host, route);
    }

    /// Forget everything. Called when a domain is added, verified or removed,
    /// and when a tenant is deleted.
    pub fn invalidate_all(&self) {
        match self.cache.write() {
            Ok(mut c) => c.clear(),
            Err(p) => p.into_inner().clear(),
        }
    }

    /// Forget one hostname.
    pub fn invalidate(&self, host: &str) {
        let host = normalize_host(host);
        match self.cache.write() {
            Ok(mut c) => c.remove(&host),
            Err(p) => p.into_inner().remove(&host),
        };
    }

    /// Whether `host` is under the tenant subdomain zone.
    ///
    /// Used to tell a mistyped tenant address ("no such site") from a
    /// stranger's domain pointed here by accident ("not configured"), which
    /// deserve different pages.
    pub fn is_tenant_zone(&self, host: &str) -> bool {
        let host = normalize_host(host);
        !self.subdomain_base.is_empty()
            && host.len() > self.subdomain_base.len() + 1
            && host.ends_with(&format!(".{}", self.subdomain_base))
    }

    pub fn platform_host(&self) -> &str {
        &self.platform_host
    }

    pub fn cache_len(&self) -> usize {
        match self.cache.read() {
            Ok(c) => c.len(),
            Err(p) => p.into_inner().len(),
        }
    }
}

/// A tenant resolved and confirmed routable, ready to be proxied to.
pub struct Target {
    pub tenant: Tenant,
}

/// Load a tenant and decide whether traffic may go to it.
///
/// A suspended tenant *is* routable, deliberately: its own
/// `publicSiteAccess` limit closes the public half of the site while leaving
/// its admin reachable, so the owner can sign in and pay. Refusing at the edge
/// instead would lock them out of the screen that fixes the problem.
pub async fn target(pool: &Db, tenant_id: &str) -> Option<Target> {
    match pro_db::tenants::get(pool, tenant_id).await {
        Ok(tenant) if tenant.status.is_routable() => Some(Target { tenant }),
        Ok(tenant) => {
            tracing::debug!(
                tenant = %tenant.slug,
                status = tenant.status.as_str(),
                "request for a tenant that is not routable"
            );
            None
        }
        Err(e) => {
            // A domain row pointing at a missing tenant: worth noticing, since
            // the foreign key should make it impossible.
            tracing::warn!("domain resolved to tenant {tenant_id}, which could not be loaded: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pro_core::{DomainKind, DomainStatus, TenantStatus};

    async fn fixture() -> (Db, Router, String) {
        let pool = pro_db::open_in_memory().await.unwrap();
        let account = pro_db::accounts::create(&pool, "o@example.com", "O", "h").await.unwrap();
        let tenant = pro_db::tenants::create(
            &pool, &account.id, "alice", "Alice", "starter", "2026-01-01T00:00:00Z", 21000, 21010,
        )
        .await
        .unwrap();
        pro_db::tenants::set_status(&pool, &tenant.id, TenantStatus::Active).await.unwrap();
        pro_db::domains::add(
            &pool,
            &tenant.id,
            "alice.sites.example.com",
            DomainKind::Subdomain,
            true,
        )
        .await
        .unwrap();

        let router = Router::new("pro.example.com".into(), "sites.example.com".into());
        (pool, router, tenant.id)
    }

    #[tokio::test]
    async fn routes_the_platform_host() {
        let (pool, router, _) = fixture().await;
        assert_eq!(router.resolve(&pool, "pro.example.com").await, Route::Platform);
        // Case and a trailing dot are both legal in a Host header.
        assert_eq!(router.resolve(&pool, "PRO.example.com.").await, Route::Platform);
        // A port is present whenever the platform runs on a non-default one.
        assert_eq!(router.resolve(&pool, "pro.example.com:8080").await, Route::Platform);
    }

    #[tokio::test]
    async fn routes_a_tenant_subdomain() {
        let (pool, router, tenant_id) = fixture().await;
        assert_eq!(
            router.resolve(&pool, "alice.sites.example.com").await,
            Route::Tenant { tenant_id: tenant_id.clone() }
        );
        assert_eq!(
            router.resolve(&pool, "ALICE.sites.example.com:443").await,
            Route::Tenant { tenant_id }
        );
    }

    #[tokio::test]
    async fn routes_a_live_custom_domain() {
        let (pool, router, tenant_id) = fixture().await;
        let custom =
            pro_db::domains::add(&pool, &tenant_id, "blog.customer.com", DomainKind::Custom, false)
                .await
                .unwrap();

        // Pending: proved to belong to nobody yet, so it must not route.
        assert_eq!(router.resolve(&pool, "blog.customer.com").await, Route::Unknown);

        pro_db::domains::set_status(&pool, &custom.id, DomainStatus::Active, None).await.unwrap();
        router.invalidate("blog.customer.com");
        assert_eq!(
            router.resolve(&pool, "blog.customer.com").await,
            Route::Tenant { tenant_id }
        );
    }

    /// The whole point of the cache having explicit invalidation: a domain
    /// that becomes live must start routing, and one that is removed must stop.
    #[tokio::test]
    async fn invalidation_is_required_to_see_a_change() {
        let (pool, router, tenant_id) = fixture().await;
        assert_eq!(router.resolve(&pool, "late.customer.com").await, Route::Unknown);

        let d = pro_db::domains::add(&pool, &tenant_id, "late.customer.com", DomainKind::Custom, false)
            .await
            .unwrap();
        pro_db::domains::set_status(&pool, &d.id, DomainStatus::Active, None).await.unwrap();

        // Still cached as unknown — which is why every routing change calls
        // invalidate.
        assert_eq!(router.resolve(&pool, "late.customer.com").await, Route::Unknown);
        router.invalidate_all();
        assert_eq!(
            router.resolve(&pool, "late.customer.com").await,
            Route::Tenant { tenant_id }
        );
    }

    #[tokio::test]
    async fn an_unknown_host_is_unknown() {
        let (pool, router, _) = fixture().await;
        assert_eq!(router.resolve(&pool, "random.example.org").await, Route::Unknown);
        assert_eq!(router.resolve(&pool, "").await, Route::Unknown);
    }

    /// Scanner traffic against random hostnames must not grow the cache
    /// without bound.
    #[tokio::test]
    async fn the_cache_is_bounded() {
        let (pool, router, _) = fixture().await;
        for i in 0..10_050 {
            router.resolve(&pool, &format!("scan{i}.example.org")).await;
        }
        assert!(router.cache_len() <= 10_001, "got {}", router.cache_len());
    }

    /// A mistyped tenant address and a stranger's misconfigured CNAME deserve
    /// different pages.
    #[tokio::test]
    async fn distinguishes_the_tenant_zone() {
        let (_pool, router, _) = fixture().await;
        assert!(router.is_tenant_zone("typo.sites.example.com"));
        assert!(!router.is_tenant_zone("someone-elses.domain.com"));
        // The zone apex itself is not a tenant.
        assert!(!router.is_tenant_zone("sites.example.com"));
        assert!(!router.is_tenant_zone("pro.example.com"));
    }

    /// Suspended sites still route: their own publicSiteAccess limit closes
    /// the public half, which is what lets the owner sign in and pay.
    #[tokio::test]
    async fn a_suspended_tenant_is_still_a_target() {
        let (pool, _router, tenant_id) = fixture().await;
        pro_db::tenants::suspend(&pool, &tenant_id, "unpaid").await.unwrap();
        assert!(target(&pool, &tenant_id).await.is_some());
    }

    /// A tenant still being built, or already deleted, must not receive
    /// traffic.
    #[tokio::test]
    async fn provisioning_and_deleted_tenants_are_not_targets() {
        let (pool, _router, tenant_id) = fixture().await;

        pro_db::tenants::set_status(&pool, &tenant_id, TenantStatus::Provisioning).await.unwrap();
        assert!(target(&pool, &tenant_id).await.is_none());

        pro_db::tenants::mark_deleted(&pool, &tenant_id).await.unwrap();
        assert!(target(&pool, &tenant_id).await.is_none());

        assert!(target(&pool, "no-such-tenant").await.is_none());
    }
}
