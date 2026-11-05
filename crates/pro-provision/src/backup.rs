//! Tenant snapshots.
//!
//! A tenant is one directory, so a backup is one archive — the database, the
//! uploads, the themes and the config, all from the same moment. That is the
//! payoff of the one-directory-per-tenant layout: there is no cross-referencing
//! to get wrong, and a restore on another machine is a copy and a start.
//!
//! The database is copied through SQLite's own backup path rather than by
//! reading the file, because a live SQLite database with WAL enabled is not a
//! single file: a plain copy of `ghost.db` while the tenant is writing can
//! miss committed transactions that are still in `ghost.db-wal`, producing an
//! archive that restores to a slightly older — or internally inconsistent —
//! database.
use pro_core::{ProError, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// `?` on a zip call.
///
/// `zip`'s error type lives outside `pro-core`, which has no archive
/// dependency, so it is folded in here rather than given a `From` impl there.
/// A malformed archive is a bad request (an operator supplied it); a failure
/// while *writing* one is internal. Reading is the only case a caller can
/// cause, so that distinction is made at the two call sites that matter and
/// this is the generic fallback.
trait ZipExt<T> {
    fn zip_err(self) -> Result<T>;
}

impl<T> ZipExt<T> for zip::result::ZipResult<T> {
    fn zip_err(self) -> Result<T> {
        self.map_err(|e| ProError::Internal(format!("archive: {e}")))
    }
}

/// Paths inside a tenant directory that are never worth archiving.
///
/// The WAL and shared-memory files are deliberately excluded: the database is
/// archived as a consistent snapshot instead, and including a stale WAL
/// alongside it is how a restore ends up with a database that disagrees with
/// its own journal.
fn is_excluded(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    s.ends_with("-wal")
        || s.ends_with("-shm")
        || s.ends_with(".tmp")
        || s.starts_with(".config.")
        // A nested archive would make each backup contain the last.
        || s.starts_with("backups/")
}

/// Write a snapshot of `tenant_dir` to `dest`.
///
/// `db_snapshot` is a consistent copy of the tenant's database, taken by
/// [`snapshot_database`]; it replaces whatever is on disk at
/// `content/data/ghost.db` inside the archive.
pub async fn pack(tenant_dir: &Path, dest: &Path, db_snapshot: Option<PathBuf>) -> Result<u64> {
    let tenant_dir = tenant_dir.to_path_buf();
    let dest = dest.to_path_buf();
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Zipping a directory tree is blocking, CPU-bound work; on the async
    // runtime it would stall every other task on its thread.
    tokio::task::spawn_blocking(move || pack_blocking(&tenant_dir, &dest, db_snapshot.as_deref()))
        .await
        .map_err(|e| ProError::internal(format!("backup task failed: {e}")))?
}

fn pack_blocking(tenant_dir: &Path, dest: &Path, db_snapshot: Option<&Path>) -> Result<u64> {
    let file = std::fs::File::create(dest)?;
    let mut zip = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    const DB_PATH: &str = "content/data/ghost.db";

    for entry in walkdir::WalkDir::new(tenant_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let Ok(rel) = entry.path().strip_prefix(tenant_dir) else { continue };
        if rel.as_os_str().is_empty() || is_excluded(rel) {
            continue;
        }
        let name = rel.to_string_lossy().replace('\\', "/");

        if entry.file_type().is_dir() {
            zip.add_directory(format!("{name}/"), options).zip_err()?;
        } else if entry.file_type().is_file() {
            // The live database file is replaced by the consistent snapshot.
            if db_snapshot.is_some() && name == DB_PATH {
                continue;
            }
            zip.start_file(&name, options).zip_err()?;
            let bytes = std::fs::read(entry.path())?;
            zip.write_all(&bytes)?;
        }
        // Symlinks are skipped rather than followed, so an archive cannot
        // reach outside the tenant's tree.
    }

    if let Some(snapshot) = db_snapshot {
        if snapshot.exists() {
            zip.start_file(DB_PATH, options).zip_err()?;
            zip.write_all(&std::fs::read(snapshot)?)?;
        }
    }

    let out = zip.finish().zip_err()?;
    Ok(out.metadata()?.len())
}

/// Take a consistent copy of a tenant's database.
///
/// Uses `VACUUM INTO`, which writes a complete, self-contained database while
/// readers and writers continue — the right tool here precisely because it
/// does not require stopping the tenant. The result has no WAL of its own, so
/// it restores cleanly.
pub async fn snapshot_database(tenant_dir: &Path, into: &Path) -> Result<Option<PathBuf>> {
    let db = tenant_dir.join("content/data/ghost.db");
    if !db.exists() {
        return Ok(None);
    }
    if let Some(parent) = into.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // VACUUM INTO refuses to overwrite.
    if into.exists() {
        tokio::fs::remove_file(into).await?;
    }

    use sqlx::sqlite::SqliteConnectOptions;
    use std::str::FromStr;
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))
        .map_err(|e| ProError::Db(e.to_string()))?
        // Read-only: a backup must never be able to modify the live site.
        .read_only(true);

    let pool = sqlx::SqlitePool::connect_with(opts)
        .await
        .map_err(|e| ProError::Db(format!("opening {} for backup: {e}", db.display())))?;

    let result = sqlx::query(&format!("VACUUM INTO '{}'", into.display().to_string().replace('\'', "''")))
        .execute(&pool)
        .await;
    pool.close().await;
    result.map_err(|e| ProError::Db(format!("snapshotting {}: {e}", db.display())))?;

    Ok(Some(into.to_path_buf()))
}

/// Extract an archive into `dest`.
///
/// Every entry's path is checked before it is written. A zip file is attacker
/// -supplied data as soon as an operator can restore one that came from
/// somewhere else, and an entry named `../../etc/cron.d/x` would otherwise be
/// written exactly there. This is the Zip Slip check, and it is the reason
/// this function does not just call a library helper.
pub async fn unpack(archive: &Path, dest: &Path) -> Result<()> {
    let archive = archive.to_path_buf();
    let dest = dest.to_path_buf();
    tokio::fs::create_dir_all(&dest).await?;
    tokio::task::spawn_blocking(move || unpack_blocking(&archive, &dest))
        .await
        .map_err(|e| ProError::internal(format!("restore task failed: {e}")))?
}

fn unpack_blocking(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| ProError::BadRequest(format!("not a readable archive: {e}")))?;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).zip_err()?;

        // `enclosed_name` returns None for anything that would escape the
        // destination — absolute paths, `..` components, Windows drive
        // prefixes. Refusing the whole restore is right: a archive containing
        // such an entry is not one to partially trust.
        let Some(rel) = entry.enclosed_name() else {
            return Err(ProError::BadRequest(format!(
                "archive contains an unsafe path ({}) and was not restored",
                entry.name()
            )));
        };
        let target = dest.join(&rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sample_tenant() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pro-bk-{}", pro_core::ids::object_id()));
        tokio::fs::create_dir_all(dir.join("content/data")).await.unwrap();
        tokio::fs::create_dir_all(dir.join("content/images/2026/01")).await.unwrap();
        tokio::fs::write(dir.join("config.production.json"), r#"{"url":"https://a.example"}"#)
            .await
            .unwrap();
        tokio::fs::write(dir.join("content/images/2026/01/pic.jpg"), vec![7u8; 500])
            .await
            .unwrap();
        tokio::fs::write(dir.join("content/data/ghost.db"), vec![1u8; 100]).await.unwrap();
        // Journal files that must not be archived.
        tokio::fs::write(dir.join("content/data/ghost.db-wal"), vec![2u8; 50]).await.unwrap();
        tokio::fs::write(dir.join("content/data/ghost.db-shm"), vec![3u8; 50]).await.unwrap();
        dir
    }

    fn names(archive: &Path) -> Vec<String> {
        let f = std::fs::File::open(archive).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .filter(|n| !n.ends_with('/'))
            .collect()
    }

    #[tokio::test]
    async fn packs_a_tenant_and_restores_it() {
        let dir = sample_tenant().await;
        let archive = dir.with_extension("zip");

        let size = pack(&dir, &archive, None).await.unwrap();
        assert!(size > 0);

        let restored = dir.with_file_name("restored");
        unpack(&archive, &restored).await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(restored.join("config.production.json")).await.unwrap(),
            r#"{"url":"https://a.example"}"#
        );
        assert_eq!(
            tokio::fs::read(restored.join("content/images/2026/01/pic.jpg")).await.unwrap().len(),
            500
        );

        tokio::fs::remove_dir_all(&dir).await.unwrap();
        tokio::fs::remove_dir_all(&restored).await.unwrap();
        tokio::fs::remove_file(&archive).await.unwrap();
    }

    /// A stale WAL beside a snapshot is how a restore ends up with a database
    /// that disagrees with its own journal.
    #[tokio::test]
    async fn excludes_journal_files() {
        let dir = sample_tenant().await;
        let archive = dir.with_extension("zip");
        pack(&dir, &archive, None).await.unwrap();

        let entries = names(&archive);
        assert!(entries.iter().any(|n| n == "content/data/ghost.db"));
        assert!(!entries.iter().any(|n| n.ends_with("-wal")), "got {entries:?}");
        assert!(!entries.iter().any(|n| n.ends_with("-shm")));

        tokio::fs::remove_dir_all(&dir).await.unwrap();
        tokio::fs::remove_file(&archive).await.unwrap();
    }

    /// Backups must not nest, or each one contains the last.
    #[tokio::test]
    async fn excludes_nested_backups() {
        let dir = sample_tenant().await;
        tokio::fs::create_dir_all(dir.join("backups")).await.unwrap();
        tokio::fs::write(dir.join("backups/old.zip"), vec![0u8; 10]).await.unwrap();

        let archive = dir.with_extension("zip");
        pack(&dir, &archive, None).await.unwrap();
        assert!(!names(&archive).iter().any(|n| n.starts_with("backups/")));

        tokio::fs::remove_dir_all(&dir).await.unwrap();
        tokio::fs::remove_file(&archive).await.unwrap();
    }

    /// The database snapshot replaces the live file, so the archive holds one
    /// consistent copy rather than two disagreeing ones.
    #[tokio::test]
    async fn a_snapshot_replaces_the_live_database() {
        let dir = sample_tenant().await;
        let snapshot = dir.with_file_name("snap.db");
        tokio::fs::write(&snapshot, vec![9u8; 777]).await.unwrap();

        let archive = dir.with_extension("zip");
        pack(&dir, &archive, Some(snapshot.clone())).await.unwrap();

        let restored = dir.with_file_name("restored2");
        unpack(&archive, &restored).await.unwrap();
        let db = tokio::fs::read(restored.join("content/data/ghost.db")).await.unwrap();
        assert_eq!(db.len(), 777, "the snapshot, not the live file");
        assert_eq!(db[0], 9);

        tokio::fs::remove_dir_all(&dir).await.unwrap();
        tokio::fs::remove_dir_all(&restored).await.unwrap();
        tokio::fs::remove_file(&archive).await.unwrap();
        tokio::fs::remove_file(&snapshot).await.unwrap();
    }

    /// VACUUM INTO against a real database, which is what makes a snapshot
    /// consistent without stopping the tenant.
    #[tokio::test]
    async fn snapshots_a_live_database_consistently() {
        let dir = std::env::temp_dir().join(format!("pro-vac-{}", pro_core::ids::object_id()));
        tokio::fs::create_dir_all(dir.join("content/data")).await.unwrap();
        let db_path = dir.join("content/data/ghost.db");

        // Build a real database with WAL on, as a tenant's would be.
        use sqlx::sqlite::SqliteConnectOptions;
        use std::str::FromStr;
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
            .unwrap()
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::query("CREATE TABLE posts (id TEXT PRIMARY KEY, title TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO posts VALUES ('1', 'Hello')").execute(&pool).await.unwrap();

        // Snapshot while the pool is still open — the realistic case.
        let into = dir.join("snapshot.db");
        let taken = snapshot_database(&dir, &into).await.unwrap().expect("a database exists");
        assert!(taken.exists());

        // The snapshot is a complete database holding the committed row.
        let check = sqlx::SqlitePool::connect(&format!("sqlite://{}", into.display()))
            .await
            .unwrap();
        let title: String = sqlx::query_scalar("SELECT title FROM posts WHERE id = '1'")
            .fetch_one(&check)
            .await
            .unwrap();
        assert_eq!(title, "Hello");

        check.close().await;
        pool.close().await;
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn no_database_means_no_snapshot() {
        let dir = std::env::temp_dir().join(format!("pro-nodb-{}", pro_core::ids::object_id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        assert!(snapshot_database(&dir, &dir.join("s.db")).await.unwrap().is_none());
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    /// Zip Slip: an archive entry that climbs out of the destination must be
    /// refused outright, not written.
    #[tokio::test]
    async fn refuses_an_archive_that_escapes_its_destination() {
        let base = std::env::temp_dir().join(format!("pro-slip-{}", pro_core::ids::object_id()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let archive = base.join("evil.zip");

        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            // Written with the raw name so the traversal survives into the file.
            zip.start_file("../../escaped.txt", opts).unwrap();
            zip.write_all(b"pwned").unwrap();
            zip.finish().unwrap();
        }

        let dest = base.join("dest");
        let err = unpack(&archive, &dest).await.unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.to_string().contains("unsafe path"), "got {err}");
        assert!(!base.join("escaped.txt").exists(), "nothing was written outside");

        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    #[tokio::test]
    async fn refuses_a_file_that_is_not_an_archive() {
        let base = std::env::temp_dir().join(format!("pro-bad-{}", pro_core::ids::object_id()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let not_zip = base.join("not.zip");
        tokio::fs::write(&not_zip, b"this is not a zip file").await.unwrap();

        let err = unpack(&not_zip, &base.join("dest")).await.unwrap_err();
        assert_eq!(err.status(), 400);
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }
}
