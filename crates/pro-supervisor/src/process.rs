//! The process registry.
use pro_core::{Config, ProError, Result, Tenant};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// A tenant process that is up.
#[derive(Debug, Clone)]
pub struct Running {
    pub tenant_id: String,
    pub port: u16,
    pub pid: u32,
    pub started_at: String,
}

/// What [`Supervisor::ensure_running`] had to do.
///
/// The caller — the proxy — uses this to decide whether to log a cold start,
/// which is the thing worth noticing when a page felt slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    AlreadyUp,
    Started,
}

/// One tenant's slot in the registry.
///
/// The `Mutex<Option<Child>>` is per tenant, not global, which is what lets
/// two different tenants start at the same time while ten simultaneous
/// requests for *one* hibernating tenant start it exactly once. Without that
/// per-tenant serialisation, a burst of traffic to a sleeping site spawns a
/// burst of processes, all but one of which fail to bind the port and die —
/// and the one that survives is whichever won a race.
struct Slot {
    lock: Mutex<Option<Child>>,
    meta: std::sync::Mutex<Option<Running>>,
}

impl Slot {
    fn new() -> Self {
        Self { lock: Mutex::new(None), meta: std::sync::Mutex::new(None) }
    }

    fn snapshot(&self) -> Option<Running> {
        match self.meta.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    fn set(&self, running: Option<Running>) {
        match self.meta.lock() {
            Ok(mut g) => *g = running,
            Err(p) => *p.into_inner() = running,
        }
    }
}

pub struct Supervisor {
    config: Arc<Config>,
    /// Resolved once at construction so a missing binary is a boot failure
    /// with a clear message, rather than a confusing spawn error the first
    /// time somebody signs up.
    ghost_binary: PathBuf,
    slots: std::sync::Mutex<HashMap<String, Arc<Slot>>>,
}

impl Supervisor {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        let ghost_binary = resolve_binary(&config)?;
        tracing::info!("tenant processes will run {}", ghost_binary.display());
        Ok(Self { config, ghost_binary, slots: std::sync::Mutex::new(HashMap::new()) })
    }

    fn slot(&self, tenant_id: &str) -> Arc<Slot> {
        let mut slots = match self.slots.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        slots.entry(tenant_id.to_string()).or_insert_with(|| Arc::new(Slot::new())).clone()
    }

    /// Everything currently up.
    pub fn running(&self) -> Vec<Running> {
        let slots = match self.slots.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        slots.values().filter_map(|s| s.snapshot()).collect()
    }

    pub fn is_running(&self, tenant_id: &str) -> bool {
        self.slot(tenant_id).snapshot().is_some()
    }

    pub fn running_count(&self) -> usize {
        self.running().len()
    }

    /// Start `tenant` if it is not already up, and wait until it answers.
    ///
    /// Safe to call concurrently for the same tenant: the per-tenant lock
    /// means the first caller starts it and the rest wait for that start and
    /// then see `AlreadyUp`.
    pub async fn ensure_running(&self, tenant: &Tenant) -> Result<StartOutcome> {
        let slot = self.slot(&tenant.id);
        let mut guard = slot.lock.lock().await;

        // Re-check under the lock. Between us taking the lock and the previous
        // holder releasing it, the tenant may have been started by them.
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                // Still running.
                Ok(None) => return Ok(StartOutcome::AlreadyUp),
                // It exited while we were not looking — a crash. Fall through
                // and start a replacement, which is the whole point of
                // supervising rather than just spawning.
                Ok(Some(status)) => {
                    tracing::warn!(
                        tenant = %tenant.slug,
                        "tenant process exited on its own ({status}); restarting"
                    );
                    *guard = None;
                    slot.set(None);
                }
                Err(e) => {
                    tracing::warn!(tenant = %tenant.slug, "could not check tenant process: {e}");
                }
            }
        }

        let child = self.spawn(tenant).await?;
        let pid = child.id().unwrap_or(0);
        *guard = Some(child);

        // Wait for the port before reporting success, so a caller that got
        // `Started` can proxy immediately. A tenant runs its migrations
        // before it binds, so this also covers a first boot.
        let deadline = std::time::Duration::from_secs(self.config.tenants.start_timeout_secs);
        if let Err(e) = wait_for_port(tenant.port, deadline).await {
            // Reap the process rather than leaving it half-started: it may be
            // stuck, and it is holding the port either way.
            if let Some(mut child) = guard.take() {
                let _ = child.kill().await;
            }
            slot.set(None);
            return Err(e);
        }

        let running = Running {
            tenant_id: tenant.id.clone(),
            port: tenant.port,
            pid,
            started_at: pro_core::now_iso(),
        };
        slot.set(Some(running));
        tracing::info!(tenant = %tenant.slug, port = tenant.port, pid, "tenant started");
        Ok(StartOutcome::Started)
    }

    /// Launch the process.
    ///
    /// Two things here are load-bearing:
    ///
    /// * **`current_dir`** is the tenant's directory. Ghost reads
    ///   `config.production.json` from its working directory and resolves
    ///   `contentPath` and the database path relative to it, so the working
    ///   directory *is* how a tenant is told which site it is. Nothing is
    ///   passed on the command line.
    /// * **`hostSettings` is not passed through the environment.** Ghost's
    ///   env-var layer parses values as scalars only, so a JSON object
    ///   arriving that way becomes a *string* and the limits silently vanish.
    ///   It has to be in the config file, which is `pro-provision`'s job.
    async fn spawn(&self, tenant: &Tenant) -> Result<Child> {
        let dir = self.config.tenant_dir(&tenant.id);
        if !dir.join(format!("config.{}.json", self.config.tenants.env)).exists() {
            return Err(ProError::Internal(format!(
                "tenant {} has no config.{}.json in {} — it was never provisioned, or its \
                 directory has been moved",
                tenant.slug,
                self.config.tenants.env,
                dir.display()
            )));
        }

        // One log file per tenant, appended to across restarts, so one noisy
        // site is separable from the platform's own log and from every other
        // tenant's.
        let log_dir = self.config.log_dir();
        tokio::fs::create_dir_all(&log_dir).await?;
        let log_path = log_dir.join(format!("{}.log", tenant.slug));
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
        let log_err = log.try_clone()?;

        let mut cmd = Command::new(&self.ghost_binary);
        cmd.current_dir(&dir)
            .env("GHOST_ENV", &self.config.tenants.env)
            // The tenant binds loopback only. Every request reaches it
            // through this platform's proxy, so a tenant cannot be addressed
            // directly from outside even if its port were guessed.
            .env("GHOST_SERVER__HOST", "127.0.0.1")
            .env("GHOST_SERVER__PORT", tenant.port.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(log_err)
            // Reaped by this supervisor; without this, a killed tenant stays
            // a zombie until the platform exits.
            .kill_on_drop(false);

        cmd.spawn().map_err(|e| {
            ProError::Internal(format!(
                "could not start tenant {}: {e} (binary {})",
                tenant.slug,
                self.ghost_binary.display()
            ))
        })
    }

    /// Stop a tenant, politely first.
    ///
    /// SIGTERM, then SIGKILL after a grace period. The politeness is not
    /// cosmetic: it lets the tenant finish in-flight requests and close its
    /// SQLite connections, so the next start does not begin with WAL
    /// recovery. A site that is hibernated and woken many times a day would
    /// otherwise recover its journal on every single start.
    pub async fn stop(&self, tenant_id: &str) -> Result<()> {
        let slot = self.slot(tenant_id);
        let mut guard = slot.lock.lock().await;
        let Some(mut child) = guard.take() else {
            slot.set(None);
            return Ok(());
        };

        if let Some(pid) = child.id() {
            // SAFETY: `pid` came from a child this process spawned and has not
            // been reaped — `try_wait` has not reported an exit — so the id
            // still refers to that child and not to a recycled one.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }

        let graceful = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;
        match graceful {
            Ok(Ok(status)) => {
                tracing::info!(tenant = %tenant_id, "tenant stopped ({status})");
            }
            // Ignored SIGTERM for ten seconds, or we could not wait on it.
            _ => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                tracing::warn!(tenant = %tenant_id, "tenant did not stop politely; killed");
            }
        }
        slot.set(None);
        Ok(())
    }

    /// Stop and start, for a change that only takes effect at boot — a new
    /// plan's `hostSettings`, or a new primary domain in `url`.
    pub async fn restart(&self, tenant: &Tenant) -> Result<()> {
        self.stop(&tenant.id).await?;
        self.ensure_running(tenant).await?;
        Ok(())
    }

    /// Stop every tenant, for a clean platform shutdown.
    ///
    /// Sequential rather than concurrent: these all contend for the same disk,
    /// and a hundred simultaneous SQLite closes is slower than a hundred
    /// sequential ones, not faster.
    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.running().into_iter().map(|r| r.tenant_id).collect();
        if ids.is_empty() {
            return;
        }
        tracing::info!("stopping {} tenant process(es)", ids.len());
        for id in ids {
            if let Err(e) = self.stop(&id).await {
                tracing::warn!(tenant = %id, "could not stop tenant: {e}");
            }
        }
    }

    /// Notice tenants that have exited on their own, and forget them.
    ///
    /// Called on a timer. A crashed tenant would otherwise still look running
    /// to [`Self::is_running`], and the proxy would forward requests to a port
    /// nothing is listening on. Reaping here also collects the zombie.
    pub async fn reap_exited(&self) -> Vec<String> {
        let ids: Vec<String> = self.running().into_iter().map(|r| r.tenant_id).collect();
        let mut gone = Vec::new();
        for id in ids {
            let slot = self.slot(&id);
            // `try_lock` rather than `lock`: a tenant currently being started
            // or stopped is busy and will be correct when that finishes.
            // Blocking here would make the sweeper wait on a cold start.
            let Ok(mut guard) = slot.lock.try_lock() else { continue };
            if let Some(child) = guard.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    tracing::warn!(tenant = %id, "tenant process exited unexpectedly ({status})");
                    *guard = None;
                    slot.set(None);
                    gone.push(id);
                }
            }
        }
        gone
    }

    /// Tenants that have been up with no traffic for longer than the idle
    /// timeout, least recently used first.
    ///
    /// `last_request_at` comes from the database rather than from this
    /// process, so a platform restart does not make every site look freshly
    /// used — and so idleness is measured across however many edge processes
    /// there are.
    pub fn idle_candidates(&self, tenants: &[Tenant], idle_timeout_secs: u64) -> Vec<String> {
        if idle_timeout_secs == 0 {
            return Vec::new();
        }
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(idle_timeout_secs as i64);
        let mut idle: Vec<(String, String)> = tenants
            .iter()
            .filter(|t| self.is_running(&t.id))
            .filter_map(|t| {
                // A tenant that is up but has never served a request is a
                // fresh start; its process start time is what counts, and the
                // registry has that.
                let last = t
                    .last_request_at
                    .clone()
                    .or_else(|| self.slot(&t.id).snapshot().map(|r| r.started_at))?;
                let seen = pro_core::parse_iso(&last)?;
                (seen < cutoff).then_some((t.id.clone(), last))
            })
            .collect();
        idle.sort_by(|a, b| a.1.cmp(&b.1));
        idle.into_iter().map(|(id, _)| id).collect()
    }

    /// Stop the least recently used tenants until `max_running` is satisfied.
    ///
    /// Called before starting one more when the platform is already at its
    /// ceiling. Returns how many it stopped.
    pub async fn evict_to_fit(&self, tenants: &[Tenant], max_running: usize) -> usize {
        let running = self.running_count();
        if running < max_running {
            return 0;
        }
        let over = running + 1 - max_running;

        // Order every running tenant by idleness, not just the ones past the
        // idle timeout — at the ceiling, *something* has to go, even if
        // everything is busy.
        let mut by_age: Vec<(String, String)> = tenants
            .iter()
            .filter(|t| self.is_running(&t.id))
            .filter_map(|t| {
                let last = t
                    .last_request_at
                    .clone()
                    .or_else(|| self.slot(&t.id).snapshot().map(|r| r.started_at))
                    .unwrap_or_default();
                Some((t.id.clone(), last))
            })
            .collect();
        by_age.sort_by(|a, b| a.1.cmp(&b.1));

        let mut stopped = 0;
        for (id, _) in by_age.into_iter().take(over) {
            tracing::info!(
                tenant = %id,
                "at the running-site ceiling ({max_running}); hibernating the least recently used"
            );
            if self.stop(&id).await.is_ok() {
                stopped += 1;
            }
        }
        stopped
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }
}

/// Find the `ghost` binary, so a missing one fails at boot with a message
/// naming what to fix.
///
/// A path — including a relative one, which is taken from the deployment
/// directory — is used as given. A bare name is searched for on `PATH`.
fn resolve_binary(config: &Config) -> Result<PathBuf> {
    let configured = &config.paths.ghost_binary;
    if let Some(p) = config.ghost_binary_path() {
        return if p.exists() {
            Ok(p)
        } else {
            Err(ProError::Config(format!(
                "paths.ghostBinary points at {}, which does not exist. Build Vuuut \
                 (cargo build --release in its checkout), or correct the path in pro.json.",
                p.display()
            )))
        };
    }
    // A bare name: search PATH, as a shell would.
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(configured);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(ProError::Config(format!(
        "could not find the “{configured}” binary on PATH. Build Vuuut and set \
         paths.ghostBinary to its target/release/ghost."
    )))
}

/// Wait until something is listening on `port`.
///
/// A TCP connect is the readiness signal, because the tenant binds its port
/// only after migrations have run and its routes are built — so a successful
/// connect means the next HTTP request will be served rather than refused.
async fn wait_for_port(port: u16, timeout: std::time::Duration) -> Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + timeout;
    let mut delay = std::time::Duration::from_millis(20);

    loop {
        match tokio::net::TcpStream::connect(&addr).await {
            Ok(_) => return Ok(()),
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(delay).await;
                // Back off gently: a warm start answers in tens of
                // milliseconds, a first boot with migrations takes seconds,
                // and polling the whole time at 20ms is wasteful.
                delay = (delay * 2).min(std::time::Duration::from_millis(250));
            }
            Err(e) => {
                return Err(ProError::Unavailable(format!(
                    "tenant did not start listening on port {port} within {}s ({e})",
                    timeout.as_secs()
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pro_core::TenantStatus;

    fn config() -> Arc<Config> {
        let mut c: Config = serde_json::from_value(serde_json::json!({
            "url": "http://pro.localhost:8080",
            "subdomainBase": "localhost",
            // Every platform has /bin/sh, so the supervisor's own logic can be
            // tested without building the CMS.
            "paths": { "ghostBinary": "/bin/sh", "dataDir": "/tmp/pro-supervisor-test" },
        }))
        .unwrap();
        c.plans = pro_core::plans::default_plans();
        Arc::new(c)
    }

    fn tenant(id: &str, port: u16) -> Tenant {
        Tenant {
            id: id.into(),
            slug: id.into(),
            name: id.into(),
            account_id: "a1".into(),
            plan_slug: "starter".into(),
            status: TenantStatus::Active,
            port,
            period_start: "2026-01-01T00:00:00Z".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_request_at: None,
            suspended_at: None,
            suspended_reason: None,
            delete_after: None,
            storage_bytes: 0,
            notes: None,
            // The platform's integration key on the site. Nothing the
            // supervisor does reads it — it spawns and signals a process —
            // so the fixture leaves it unset, which is also what a site
            // created before the platform kept one looks like.
            platform_key_id: None,
            platform_key_secret: None,
        }
    }

    /// A missing binary must be a boot failure naming what to fix, not a
    /// confusing spawn error at the first signup.
    #[test]
    fn a_missing_binary_fails_at_construction() {
        let mut c: Config = serde_json::from_value(serde_json::json!({
            "url": "http://pro.localhost:8080",
            "subdomainBase": "localhost",
            "paths": { "ghostBinary": "/nonexistent/ghost" },
        }))
        .unwrap();
        c.plans = pro_core::plans::default_plans();
        let Err(err) = Supervisor::new(Arc::new(c)) else {
            panic!("a missing binary must not construct a supervisor")
        };
        assert_eq!(err.status(), 500);
        assert!(err.to_string().contains("does not exist"), "got {err}");
    }

    /// A config naming `binary`, deployed in `base_dir`.
    fn with_binary(binary: &str, base_dir: &str) -> Config {
        let mut c: Config = serde_json::from_value(serde_json::json!({
            "url": "http://pro.localhost:8080",
            "subdomainBase": "localhost",
            "paths": { "ghostBinary": binary },
        }))
        .unwrap();
        c.plans = pro_core::plans::default_plans();
        c.base_dir = PathBuf::from(base_dir);
        c
    }

    #[test]
    fn resolves_a_bare_name_on_path() {
        // `sh` is on PATH everywhere this runs.
        assert!(resolve_binary(&with_binary("sh", "/tmp")).is_ok());
        let err =
            resolve_binary(&with_binary("definitely-not-a-real-binary-xyz", "/tmp")).unwrap_err();
        assert!(err.to_string().contains("on PATH"), "got {err}");
    }

    /// The point of relative paths: a `pro.json` that names the Vuuut
    /// checkout beside it works on every machine the pair is cloned onto,
    /// which an absolute path does not. Resolution is from the deployment
    /// directory, not the process's working directory.
    #[test]
    fn resolves_a_relative_path_from_the_deployment_directory() {
        // /bin/sh reached as ../bin/sh from /bin — a path that only resolves
        // if base_dir is what it is joined to.
        let resolved = resolve_binary(&with_binary("../bin/sh", "/bin")).unwrap();
        assert!(resolved.exists(), "{} should exist", resolved.display());

        let err = resolve_binary(&with_binary("../bin/no-such-ghost", "/bin")).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "got {err}");
        // The message names the resolved path, not the relative fragment:
        // "../bin/no-such-ghost does not exist" sends an operator looking in
        // the wrong directory.
        assert!(err.to_string().contains("/bin/../bin/no-such-ghost"), "got {err}");
    }

    /// Spawning must refuse a tenant that was never provisioned, rather than
    /// starting a process in a directory with no config.
    #[tokio::test]
    async fn refuses_an_unprovisioned_tenant() {
        let sup = Supervisor::new(config()).unwrap();
        let err = sup.ensure_running(&tenant("never-provisioned", 29123)).await.unwrap_err();
        assert!(err.to_string().contains("never provisioned"), "got {err}");
        assert!(!sup.is_running("never-provisioned"));
    }

    /// A process that starts but never binds the port must be reaped, not
    /// left behind holding resources.
    #[tokio::test]
    async fn a_process_that_never_listens_is_cleaned_up() {
        let cfg = config();
        let dir = cfg.tenant_dir("sleeper");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // The config file only has to exist for the pre-flight check.
        tokio::fs::write(dir.join("config.production.json"), "{}").await.unwrap();

        let mut c = (*cfg).clone();
        // `sh` with no script reads stdin, which is /dev/null here, so it
        // starts and exits without ever listening.
        c.paths.ghost_binary = "/bin/sh".into();
        c.tenants.start_timeout_secs = 1;
        let Ok(sup) = Supervisor::new(Arc::new(c)) else { panic!("/bin/sh should resolve") };

        let err = sup.ensure_running(&tenant("sleeper", 29124)).await.unwrap_err();
        assert_eq!(err.status(), 503, "a start timeout is retryable");
        assert!(!sup.is_running("sleeper"), "the failed start left nothing registered");

        let _ = tokio::fs::remove_dir_all(cfg.data_dir()).await;
    }

    /// Nothing is running on a fresh supervisor, and stopping something that
    /// is not running is not an error — callers stop defensively.
    #[tokio::test]
    async fn stopping_an_unknown_tenant_is_harmless() {
        let sup = Supervisor::new(config()).unwrap();
        assert_eq!(sup.running_count(), 0);
        assert!(sup.stop("never-started").await.is_ok());
    }

    /// Idleness is measured from the database's `last_request_at`, so a
    /// platform restart does not make every site look freshly used.
    #[test]
    fn picks_idle_candidates_oldest_first() {
        let sup = Supervisor::new(config()).unwrap();

        let mut old = tenant("old", 21001);
        old.last_request_at = Some(pro_core::days_from_now(-1));
        let mut older = tenant("older", 21002);
        older.last_request_at = Some(pro_core::days_from_now(-2));
        let mut fresh = tenant("fresh", 21003);
        fresh.last_request_at = Some(pro_core::now_iso());

        // Nothing is actually running, so nothing is a candidate — the filter
        // is on the registry, not just on the timestamps.
        assert!(sup.idle_candidates(&[old.clone(), older.clone(), fresh.clone()], 900).is_empty());

        // Register them as running without spawning anything.
        for t in [&old, &older, &fresh] {
            sup.slot(&t.id).set(Some(Running {
                tenant_id: t.id.clone(),
                port: t.port,
                pid: 1,
                started_at: pro_core::now_iso(),
            }));
        }

        let idle = sup.idle_candidates(&[old, older, fresh], 900);
        assert_eq!(idle, vec!["older".to_string(), "old".to_string()]);
    }

    /// A zero timeout means "never hibernate", which an operator sets when
    /// they would rather spend memory than ever make a visitor wait.
    #[test]
    fn a_zero_timeout_disables_hibernation() {
        let sup = Supervisor::new(config()).unwrap();
        let mut t = tenant("ancient", 21001);
        t.last_request_at = Some(pro_core::days_from_now(-30));
        sup.slot(&t.id).set(Some(Running {
            tenant_id: t.id.clone(),
            port: t.port,
            pid: 1,
            started_at: pro_core::now_iso(),
        }));
        assert!(sup.idle_candidates(&[t], 0).is_empty());
    }

    /// At the ceiling something has to go, even when everything is busy.
    #[tokio::test]
    async fn evicts_the_least_recently_used_at_the_ceiling() {
        let sup = Supervisor::new(config()).unwrap();
        let mut a = tenant("a", 21001);
        a.last_request_at = Some(pro_core::days_from_now(-3));
        let mut b = tenant("b", 21002);
        b.last_request_at = Some(pro_core::now_iso());

        for t in [&a, &b] {
            sup.slot(&t.id).set(Some(Running {
                tenant_id: t.id.clone(),
                port: t.port,
                pid: 1,
                started_at: pro_core::now_iso(),
            }));
        }
        assert_eq!(sup.running_count(), 2);

        // Room for one more: nothing is evicted.
        assert_eq!(sup.evict_to_fit(&[a.clone(), b.clone()], 5).await, 0);

        // At the ceiling: the oldest goes, so the newcomer fits.
        assert_eq!(sup.evict_to_fit(&[a, b], 2).await, 1);
        assert!(!sup.is_running("a"), "the least recently used was stopped");
        assert!(sup.is_running("b"));
    }
}
