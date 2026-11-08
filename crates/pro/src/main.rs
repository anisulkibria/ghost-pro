//! `ghost-pro` — the hosting platform's binary.
//!
//! One process runs the whole control plane: the edge proxy, both panels, the
//! job queue and the supervisor that starts and stops tenant sites.
//!
//! ```text
//!                         ┌──────────────────────────┐
//!   visitors ───────────► │  edge (this process)     │
//!                         │  · TLS for every domain  │
//!                         │  · Host → tenant         │
//!                         │  · /dashboard  /console  │
//!                         └───────────┬──────────────┘
//!                                     │ loopback
//!                   ┌─────────────────┼─────────────────┐
//!                   ▼                 ▼                 ▼
//!              ghost :21000      ghost :21001      ghost :21002
//!              (tenant a)        (tenant b)        (tenant c)
//! ```
//!
//! Each tenant is an unmodified `ghost` binary with its own directory,
//! database and config. Nothing here links against the CMS.
mod cli;
mod serve;

use pro_core::{Config, ProError, Result};
use std::sync::Arc;

fn main() {
    // Logging before anything else, so a config failure is reported rather
    // than silently exiting.
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info,sqlx=warn".into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let command = cli::parse();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("could not start the async runtime: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = runtime.block_on(run(command)) {
        // A config error is the operator's to fix and deserves to be the last
        // thing they see, not buried in a log line.
        eprintln!("\n{e}\n");
        std::process::exit(1);
    }
}

async fn run(command: cli::Command) -> Result<()> {
    match command {
        cli::Command::Help => {
            cli::print_help();
            Ok(())
        }
        cli::Command::Version => {
            println!("ghost-pro {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        cli::Command::Setup(args) => cli::setup(args).await,
        cli::Command::Doctor => cli::doctor().await,
        cli::Command::TenantBinary => cli::tenant_binary().await,
        cli::Command::Db => cli::db().await,
        cli::Command::CreateStaff(args) => cli::create_staff(args).await,
        cli::Command::Serve => serve_platform().await,
    }
}

/// Boot the platform.
async fn serve_platform() -> Result<()> {
    let config = Arc::new(Config::load(None)?);
    tracing::info!(
        "ghost-pro {} starting in {} mode",
        env!("CARGO_PKG_VERSION"),
        config.env
    );

    // The config named a checkout that is no longer there, and one nearby was
    // used instead. The platform works either way, but an operator reading
    // `pro.json` would otherwise have no way to know why it disagrees with
    // what is running.
    if config.ghost_binary_is_rehomed() {
        tracing::warn!(
            "paths.ghostBinary ({}) does not exist, so a Vuuut checkout next to this one is \
             being used instead. Update pro.json to silence this",
            config.paths.ghost_binary,
        );
    }

    // ---- storage ---------------------------------------------------------
    for dir in [
        config.data_dir(),
        config.tenants_dir(),
        config.cert_dir(),
        config.backup_dir(),
        config.log_dir(),
        config.data_dir().join("tmp"),
    ] {
        tokio::fs::create_dir_all(&dir).await.map_err(|e| {
            ProError::Config(format!("could not create {}: {e}", dir.display()))
        })?;
    }

    let target = config.db_target()?;
    let db = pro_db::open(&target, config.database.pool_size(target.engine())).await?;
    // Redacted, because on MySQL this URL carries the password and this line
    // goes to `server.log`.
    tracing::info!("control plane database: {}", target.redacted());

    // A production platform on SQLite works, and is not warned about lightly —
    // but it is pinned to this one machine, and an operator who assumed
    // otherwise will find that out during an incident rather than now.
    if config.env == "production" && target.engine().is_single_machine() {
        tracing::warn!(
            "running production on SQLite: this platform cannot be served from a \
             second machine, and the database is one file on this disk. Set \
             database.url to a mysql:// server to change that"
        );
    }

    let settings = Arc::new(pro_db::SettingsStore::load(&db, config.clone()).await?);
    let supervisor = Arc::new(pro_supervisor::Supervisor::new(config.clone())?);

    // ---- first-run guidance ----------------------------------------------
    //
    // A platform with no staff login cannot be administered at all, and the
    // only way in is the command line. Said plainly at boot rather than left
    // for an operator to discover at the sign-in page.
    if pro_db::staff::count(&db).await? == 0 {
        tracing::warn!(
            "no operator login exists yet. Create one with:\n\n    \
             ghost-pro create-staff --email you@example.com --name \"Your Name\"\n"
        );
    }

    let live = settings.current();
    if live.billing_provider() == "none" {
        tracing::info!("no payment provider configured — sites can be created but nothing is charged");
    } else {
        tracing::info!("billing through {}", live.billing_provider());
    }

    let hosts = Arc::new(pro_proxy::Router::new(
        config.host(),
        config.subdomain_base.clone(),
    ));

    let state = pro_web::AppState {
        db: db.clone(),
        config: config.clone(),
        settings: settings.clone(),
        supervisor: supervisor.clone(),
        hosts: hosts.clone(),
        forwarder: pro_proxy::Forwarder::new(),
    };

    // ---- background loops ------------------------------------------------
    tokio::spawn(pro_web::worker::run_jobs(state.clone()));
    tokio::spawn(pro_web::worker::run_hibernation(state.clone()));
    tokio::spawn(pro_web::worker::run_housekeeping(state.clone()));

    // ---- shutdown --------------------------------------------------------
    //
    // Tenant processes are children of this one. Stopping them politely on the
    // way out lets each close its SQLite database cleanly, so the next start
    // does not begin with journal recovery.
    {
        let supervisor = supervisor.clone();
        tokio::spawn(async move {
            let mut term = match tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("could not listen for SIGTERM: {e}");
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            tracing::info!("shutting down; stopping tenant sites");
            supervisor.stop_all().await;
            std::process::exit(0);
        });
    }

    let app = pro_web::entry(state.clone());

    // ---- serve -----------------------------------------------------------
    let bind = format!("{}:{}", config.server.host, config.edge_port());

    if !config.ssl.enabled {
        tracing::info!(
            "TLS is off — serving plain HTTP. Dashboard at {}",
            config.canonical_url()
        );
        if config.ssl.behind_proxy {
            tracing::info!("trusting X-Forwarded-* from the proxy in front");
        }
        serve::serve_plain(&bind, app)
            .await
            .map_err(|e| ProError::Internal(format!("listening on {bind}: {e}")))?;
        return Ok(());
    }

    // The hostnames to hold certificates for: the platform's own, plus every
    // live tenant domain.
    let reload = Arc::new(tokio::sync::Notify::new());
    let domains = certificate_domains(&db, &config).await?;
    tracing::info!("holding certificates for {} hostname(s)", domains.len());

    // Re-read the set on demand, so a domain that passes its DNS check is
    // picked up without a restart.
    let domain_source = {
        let db = db.clone();
        let config = config.clone();
        move || {
            // A blocking read inside a sync closure: this runs at most once
            // per domain change, never per request.
            futures::executor::block_on(certificate_domains(&db, &config)).unwrap_or_default()
        }
    };

    // Nudge the certificate set periodically, so a newly verified domain is
    // picked up even though nothing signals this directly.
    {
        let reload = reload.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                reload.notify_one();
            }
        });
    }

    if config.server.http_port > 0 {
        let redirect_app = app.clone();
        let port = config.server.http_port;
        tokio::spawn(async move {
            if let Err(e) = serve::serve_http_redirect(port, redirect_app).await {
                tracing::error!("http redirect listener stopped: {e}");
            }
        });
    }

    serve::serve_acme(
        &bind,
        app,
        domains,
        config.ssl.email.clone(),
        config.ssl.staging,
        &config.cert_dir(),
        reload,
        domain_source,
    )
    .await
    .map_err(|e| ProError::Internal(format!("listening on {bind}: {e}")))?;
    Ok(())
}

/// Every hostname this platform should hold a certificate for.
///
/// Only *live* domains. A pending custom domain has not been proved to belong
/// to the customer who added it, and ordering a certificate for it would mean
/// asking a certificate authority to vouch for a hostname somebody else may
/// control — which would also fail, and burn rate limit doing so.
async fn certificate_domains(
    db: &pro_db::Db,
    config: &Config,
) -> Result<Vec<String>> {
    let mut domains = vec![config.host()];
    for (domain, _tenant) in pro_db::domains::all_routable(db).await? {
        if !domains.contains(&domain) {
            domains.push(domain);
        }
    }
    // Only hostnames a public certificate authority could validate.
    domains.retain(|d| {
        !d.is_empty()
            && d != "localhost"
            && !d.ends_with(".localhost")
            && !d.ends_with(".local")
            && d.parse::<std::net::IpAddr>().is_err()
    });
    Ok(domains)
}
