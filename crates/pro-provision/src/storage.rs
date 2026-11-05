//! Measuring what a tenant is using on disk.
//!
//! Ghost has no notion of a disk quota, so this is the platform's own
//! accounting. It is a directory walk, which is cheap per tenant and
//! expensive across thousands — so it runs as a background job on a schedule,
//! never on a request.
use pro_core::Result;
use std::path::Path;

/// Total bytes under `dir`, following no symlinks.
///
/// Symlinks are skipped deliberately. A tenant can upload files, and if one
/// could create a symlink the walk would follow it out of the tenant's tree —
/// at best reporting the whole disk as that tenant's usage, at worst walking a
/// cycle forever.
pub fn tenant_dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(dir).follow_links(false).into_iter().filter_map(|e| e.ok()) {
        // `metadata` on a walkdir entry does not traverse a symlink, so a
        // link is counted as the few bytes it is rather than its target.
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Measure one tenant and record it.
///
/// The counts that come from inside the tenant's own database — members,
/// posts — are *not* read here. Opening a tenant's SQLite file while its
/// process has it open in WAL mode is safe for reading, but it would make the
/// control plane depend on the CMS's schema, which is the coupling this whole
/// design avoids. Those figures come from the tenant's own API instead, and
/// a tenant that is hibernating simply keeps its last reading.
pub async fn measure_tenant(
    pool: &sqlx::SqlitePool,
    config: &pro_core::Config,
    tenant_id: &str,
) -> Result<u64> {
    let dir = config.tenant_dir(tenant_id);
    if !dir.exists() {
        return Ok(0);
    }
    // A directory walk is blocking work; doing it on the async runtime would
    // stall every other task on that thread for as long as it takes.
    let bytes = tokio::task::spawn_blocking(move || tenant_dir_size(&dir))
        .await
        .map_err(|e| pro_core::ProError::internal(format!("storage walk failed: {e}")))?;

    pro_db::tenants::set_storage_bytes(pool, tenant_id, bytes as i64).await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sums_file_sizes() {
        let dir = std::env::temp_dir().join(format!("pro-size-{}", pro_core::ids::object_id()));
        tokio::fs::create_dir_all(dir.join("content/images")).await.unwrap();
        tokio::fs::write(dir.join("content/images/a.jpg"), vec![0u8; 1000]).await.unwrap();
        tokio::fs::write(dir.join("content/images/b.jpg"), vec![0u8; 2000]).await.unwrap();
        tokio::fs::write(dir.join("config.production.json"), "{}").await.unwrap();

        let size = tenant_dir_size(&dir);
        assert_eq!(size, 3002, "both images plus the two-byte config");

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn an_absent_directory_is_zero() {
        assert_eq!(tenant_dir_size(Path::new("/nonexistent/tenant/dir")), 0);
    }

    /// A symlink must not be followed, or one tenant's usage becomes the
    /// whole disk — or an infinite walk.
    #[tokio::test]
    async fn does_not_follow_symlinks_out_of_the_tree() {
        let base = std::env::temp_dir().join(format!("pro-link-{}", pro_core::ids::object_id()));
        let outside = base.join("outside");
        let tenant = base.join("tenant");
        tokio::fs::create_dir_all(&outside).await.unwrap();
        tokio::fs::create_dir_all(&tenant).await.unwrap();
        tokio::fs::write(outside.join("big.bin"), vec![0u8; 100_000]).await.unwrap();
        tokio::fs::write(tenant.join("own.txt"), vec![0u8; 10]).await.unwrap();

        std::os::unix::fs::symlink(&outside, tenant.join("escape")).unwrap();

        let size = tenant_dir_size(&tenant);
        assert!(size < 1000, "followed the symlink: got {size}");

        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    /// A symlink loop must terminate rather than hang the measuring job.
    #[tokio::test]
    async fn survives_a_symlink_loop() {
        let dir = std::env::temp_dir().join(format!("pro-loop-{}", pro_core::ids::object_id()));
        tokio::fs::create_dir_all(dir.join("inner")).await.unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("inner/back")).unwrap();

        // Returns at all, which is the assertion.
        let _ = tenant_dir_size(&dir);
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
