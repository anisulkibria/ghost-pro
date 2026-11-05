//! The command line: `setup`, `doctor`, `create-staff`, `serve`.
//!
//! These exist so that standing the platform up is a short, checkable
//! sequence rather than a research project — the same reasoning as
//! Ghost-rust's own CLI. `setup` writes a config, `doctor` says whether this
//! machine can actually serve it, and `create-staff` is the only way to make
//! the first operator login, because a platform with no staff cannot be
//! administered through its own console.
use pro_core::{Config, ProError, Result, StaffRole};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};

pub enum Command {
    Serve,
    Setup(Args),
    Doctor,
    Db,
    CreateStaff(Args),
    Help,
    Version,
}

/// `--key value` and `--flag`, parsed into a map.
pub struct Args(HashMap<String, String>);

impl Args {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    pub fn flag(&self, key: &str) -> bool {
        matches!(self.0.get(key).map(String::as_str), Some("true"))
    }
}

pub fn parse() -> Command {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (name, rest) = match argv.split_first() {
        None => return Command::Serve,
        Some((first, rest)) => (first.as_str(), rest),
    };

    let mut args = HashMap::new();
    let mut i = 0;
    while i < rest.len() {
        let token = &rest[i];
        if let Some(key) = token.strip_prefix("--") {
            match rest.get(i + 1) {
                Some(value) if !value.starts_with("--") => {
                    args.insert(key.to_string(), value.clone());
                    i += 2;
                }
                _ => {
                    args.insert(key.to_string(), "true".to_string());
                    i += 1;
                }
            }
        } else {
            i += 1;
        }
    }
    let args = Args(args);

    match name {
        "serve" | "start" | "run" => Command::Serve,
        "setup" | "install" => Command::Setup(args),
        "doctor" | "check" => Command::Doctor,
        "db" | "database" => Command::Db,
        "create-staff" | "createstaff" | "staff" => Command::CreateStaff(args),
        "version" | "--version" | "-V" => Command::Version,
        _ => Command::Help,
    }
}

pub fn print_help() {
    println!(
        r#"ghost-pro {version} — managed hosting for Ghost

USAGE
    ghost-pro [command] [options]

COMMANDS
    serve                 Run the platform (the default)
    setup                 Write pro.json interactively
    doctor                Check this machine can serve the current config
    db                    Create and check the database, on either engine
    create-staff          Create an operator login
    version               Print the version

SETUP OPTIONS
    --url <url>           Where the platform is reached, e.g. https://pro.example.com
    --subdomain-base <d>  Zone for tenant sites, e.g. sites.example.com
    --ghost-binary <path> Path to the Ghost-rust `ghost` binary
    --theme-source <path> Ghost-rust content/themes to copy into new sites
    --data-dir <path>     Where tenant sites and the database live (default: data)
    --ssl                 Get certificates from Let's Encrypt
    --email <address>     Contact address for certificate expiry warnings
    --staging             Use Let's Encrypt's test CA while trying things out

CREATE-STAFF OPTIONS
    --email <address>     Required
    --name <name>         Required
    --password <password> Prompted for if omitted
    --role <role>         owner | admin | support (default: owner)

DATABASE
    With no `database.url` configured the platform uses a SQLite file at
    <dataDir>/pro.db, created on first run. Nothing to install.

    For production, name a MySQL server and the platform creates its own
    schema on first connection. Put it in pro.production.json:

        {{ "database": {{
            "url": "mysql://pro:secret@db.internal:3306/pro",
            "tenantUrl": "mysql://admin:secret@db.internal:3306"
        }} }}

    `url` is the control plane's own database. `tenantUrl` is the server each
    site's database is created on, and its account needs CREATE DATABASE,
    CREATE USER and GRANT. Leave `tenantUrl` out to keep one SQLite file per
    site. Either can come from the environment instead, as
    PRO_DATABASE__URL and PRO_DATABASE__TENANT_URL.

EXAMPLES
    ghost-pro setup --url https://pro.example.com --subdomain-base sites.example.com \
        --ghost-binary /opt/ghost-rust/target/release/ghost --ssl --email you@example.com
    ghost-pro create-staff --email you@example.com --name "Your Name"
    ghost-pro db
    ghost-pro doctor
    ghost-pro serve
"#,
        version = env!("CARGO_PKG_VERSION")
    );
}

/// Find the Ghost-rust checkout this platform should run tenant sites from.
///
/// Returned relative to the deployment directory, deliberately: the whole
/// point is a `pro.json` that survives being cloned onto another machine, and
/// an absolute path baked in at setup time is the one thing that guarantees it
/// will not. The usual layout is the two repositories side by side, with the
/// deployment directory one level inside Ghost-pro (`Ghost-pro/run`), so that
/// is the first place looked.
///
/// `None` when no checkout is found, which is not an error — an operator may
/// be setting up before building Ghost-rust, and `doctor` says so later.
fn discover_ghost_rust() -> Option<String> {
    // Searched in order of how likely each is to be the intended one, so a
    // machine with more than one checkout gets the nearest.
    const PREFIXES: [&str; 3] = ["..", "../..", "../../.."];
    const NAMES: [&str; 4] = ["Ghost-rust", "ghost-rust", "rust-ghost", "Ghost-Rust"];

    let cwd = std::env::current_dir().ok()?;
    for prefix in PREFIXES {
        for name in NAMES {
            let rel = format!("{prefix}/{name}");
            let candidate = cwd.join(&rel);
            // A directory with the right name is not enough; it has to look
            // like the checkout. Either half is sufficient — the themes are
            // committed so they are there straight after a clone, and the
            // binary appears once it has been built.
            let looks_right = candidate.join("content/themes").is_dir()
                || candidate.join("target/release/ghost").is_file();
            if looks_right {
                return Some(rel);
            }
        }
    }
    None
}

/// Write a `pro.json`.
pub async fn setup(args: Args) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    let discovered = discover_ghost_rust();

    let url = match args.get("url") {
        Some(u) => u.to_string(),
        None if interactive => ask("Platform URL (e.g. https://pro.example.com)", None)?,
        None => return Err(ProError::Config("--url is required".into())),
    };
    if !url.contains("://") {
        return Err(ProError::Config(format!("url needs a scheme: {url}")));
    }

    let subdomain_base = match args.get("subdomain-base") {
        Some(s) => s.to_string(),
        None if interactive => ask(
            "Zone for tenant sites (e.g. sites.example.com)",
            None,
        )?,
        None => return Err(ProError::Config("--subdomain-base is required".into())),
    };

    // A discovered checkout supplies the default for both the binary and the
    // themes, so the common case needs neither flag.
    let binary_default = discovered
        .as_deref()
        .map(|d| format!("{d}/target/release/ghost"))
        .unwrap_or_else(|| "ghost".to_string());

    let ghost_binary = match args.get("ghost-binary") {
        Some(p) => p.to_string(),
        None if interactive => {
            ask("Path to the Ghost-rust `ghost` binary", Some(&binary_default))?
        }
        None => binary_default,
    };

    // Checked now rather than at the first signup: a wrong path here is the
    // commonest setup mistake, and the failure it causes otherwise appears as
    // a customer's site failing to provision.
    if ghost_binary.contains('/') && !std::path::Path::new(&ghost_binary).exists() {
        tracing::warn!("{ghost_binary} does not exist yet — build Ghost-rust before serving");
    }

    let ssl = args.flag("ssl");
    let mut config = serde_json::json!({
        "url": url,
        "subdomainBase": subdomain_base,
        "paths": {
            "dataDir": args.get("data-dir").unwrap_or("data"),
            "ghostBinary": ghost_binary,
        },
        "ssl": { "enabled": ssl },
        "billing": { "provider": "none" },
        "mail": { "transport": "log" },
    });

    match args.get("theme-source") {
        Some(themes) => config["paths"]["themeSource"] = serde_json::json!(themes),
        // Left unset when there is no checkout to point at: an unset
        // themeSource is a documented state (sites start bare), where one
        // pointing at a directory that does not exist is just a broken config.
        None => {
            if let Some(d) = &discovered {
                config["paths"]["themeSource"] = serde_json::json!(format!("{d}/content/themes"));
            }
        }
    }
    if ssl {
        if let Some(email) = args.get("email") {
            config["ssl"]["email"] = serde_json::json!(email);
        }
        if args.flag("staging") {
            config["ssl"]["staging"] = serde_json::json!(true);
        }
    }

    let path = std::env::current_dir()?.join("pro.json");
    if path.exists() {
        let overwrite = if interactive {
            ask("pro.json already exists. Overwrite? (yes/no)", Some("no"))? == "yes"
        } else {
            args.flag("force")
        };
        if !overwrite {
            return Err(ProError::Config(format!(
                "{} already exists. Pass --force to overwrite.",
                path.display()
            )));
        }
    }

    std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
    println!("\nWrote {}\n", path.display());
    if args.get("ghost-binary").is_none() {
        match &discovered {
            Some(d) => println!(
                "Found a Ghost-rust checkout at {d} (relative to this directory, so this \
                 config stays valid on other machines).\n"
            ),
            None => println!(
                "No Ghost-rust checkout found nearby, so paths.ghostBinary is the bare name \
                 \"ghost\" and no themes are configured. Clone and build Ghost-rust, then set \
                 paths.ghostBinary and paths.themeSource.\n"
            ),
        }
    }
    println!("Next:");
    println!("  1. ghost-pro create-staff --email you@example.com --name \"Your Name\"");
    println!("  2. ghost-pro doctor");
    println!("  3. ghost-pro serve");
    println!(
        "\nThen open {}/console to choose a payment provider.\n",
        config["url"].as_str().unwrap_or("")
    );
    Ok(())
}

/// Report whether this machine can serve the current configuration.
pub async fn doctor() -> Result<()> {
    let mut problems = 0;

    println!("\nghost-pro doctor\n");

    let config = match Config::load(None) {
        Ok(c) => {
            check(true, "config", &format!("loaded for {} mode", c.env));
            c
        }
        Err(e) => {
            check(false, "config", &e.to_string());
            println!("\nFix the configuration before anything else.\n");
            return Err(e);
        }
    };

    println!("  url              {}", config.canonical_url());
    println!("  tenant zone      *.{}", config.subdomain_base);
    println!("  data             {}", config.data_dir().display());
    println!();

    // ---- the CMS binary --------------------------------------------------
    match pro_supervisor::Supervisor::new(std::sync::Arc::new(config.clone())) {
        Ok(_) => check(true, "ghost binary", "found"),
        Err(e) => {
            check(false, "ghost binary", &e.to_string());
            problems += 1;
        }
    }

    // ---- themes ----------------------------------------------------------
    match config.theme_source_path() {
        Some(path) if path.exists() => {
            let count = std::fs::read_dir(&path)
                .map(|entries| entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).count())
                .unwrap_or(0);
            check(true, "themes", &format!("{count} available in {}", path.display()));
        }
        Some(path) => {
            check(
                false,
                "themes",
                &format!(
                    "{} does not exist — check paths.themeSource in pro.json",
                    path.display()
                ),
            );
            problems += 1;
        }
        None => check(
            true,
            "themes",
            "none configured — new sites start with no theme installed",
        ),
    }

    // ---- storage ---------------------------------------------------------
    match std::fs::create_dir_all(config.data_dir()) {
        Ok(()) => {
            let probe = config.data_dir().join(".doctor-write-test");
            match std::fs::write(&probe, b"ok") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                    check(true, "data directory", "writable");
                }
                Err(e) => {
                    check(false, "data directory", &format!("not writable: {e}"));
                    problems += 1;
                }
            }
        }
        Err(e) => {
            check(false, "data directory", &format!("could not create: {e}"));
            problems += 1;
        }
    }

    // ---- database --------------------------------------------------------
    //
    // Reported before it is opened, because a connection that hangs or is
    // refused is the most common thing `doctor` is run to diagnose, and
    // knowing *which* database was being reached is most of the answer.
    // A `database.url` this platform cannot open is reported and then stood in
    // for by the local file, so the rest of `doctor` still runs — an operator
    // fixing a bad URL should not have to run it again to see every *other*
    // problem they have.
    let target = match config.db_target() {
        Ok(t) => t,
        Err(e) => {
            check(false, "database", &e.to_string());
            problems += 1;
            pro_core::DbTarget::sqlite_file(&config.db_path())
        }
    };
    check(true, "database engine", &format!("{} — {}", target.engine().name(), target.redacted()));
    if config.env == "production" && target.engine().is_single_machine() {
        check(
            false,
            "database for production",
            "this is a SQLite file, so the platform is pinned to this one machine \
             and its database is one file on this disk. Set database.url to a \
             mysql:// server in pro.production.json, then check it with: \
             ghost-pro db",
        );
        problems += 1;
    }

    match pro_db::open(&target, config.database.pool_size(target.engine())).await {
        Ok(db) => {
            check(true, "database", "open and migrated");

            let staff = pro_db::staff::count(&db).await.unwrap_or(0);
            if staff == 0 {
                check(
                    false,
                    "operator login",
                    "none — run: ghost-pro create-staff --email you@example.com --name \"You\"",
                );
                problems += 1;
            } else {
                check(true, "operator login", &format!("{staff} configured"));
            }

            let settings = pro_db::SettingsStore::load(&db, std::sync::Arc::new(config.clone()))
                .await?;
            let live = settings.current();

            let provider = live.billing_provider();
            if provider == "none" {
                check(
                    true,
                    "billing",
                    "no provider — sites work, nothing is charged",
                );
            } else if live.provider_is_configured(&provider) {
                // The *live* catalogue, not the config file's: product ids can
                // be set from the console's Plans screen, and counting only
                // the file would report zero for a platform that is selling
                // perfectly well.
                let sellable = live.purchasable_plans().len();
                if sellable == 0 {
                    check(
                        false,
                        "billing",
                        &format!(
                            "{provider} is configured but no plan has a {provider} product id \
                             — set them on /console/plans"
                        ),
                    );
                    problems += 1;
                } else {
                    check(
                        true,
                        "billing",
                        &format!("{provider}, {sellable} plan(s) on sale"),
                    );
                }
            } else {
                check(
                    false,
                    "billing",
                    &format!("{provider} is selected but its credentials are missing"),
                );
                problems += 1;
            }

            // Whether the money can actually finish moving.
            //
            // A platform can have perfect credentials, a priced plan and a
            // working checkout and still never open a paid site, because
            // reopening it happens on the provider's webhook and nothing else.
            // Both ways that callback can be lost are silent from inside the
            // checkout — so they are reported here, where the configuration
            // is, instead of being discovered when a customer has paid.
            if provider != "none" {
                let endpoint = format!("{}/webhooks/{provider}", config.canonical_url());
                match config.webhook_unreachable_reason() {
                    Some(reason) => {
                        check(
                            false,
                            "webhooks",
                            &format!(
                                "{reason}. Expose this machine and set `url` to that address \
                                 (a tunnel such as cloudflared or ngrok is enough for \
                                 development), or open paid sites by hand from \
                                 /console/sites.",
                            ),
                        );
                        problems += 1;
                    }
                    None if live.webhook_secret(&provider).is_none() => {
                        check(
                            false,
                            "webhooks",
                            &format!(
                                "no signing secret for {provider}, so every delivery to \
                                 {endpoint} is refused — a customer would be charged and \
                                 their site would stay closed. Set it in the console."
                            ),
                        );
                        problems += 1;
                    }
                    None => check(true, "webhooks", &format!("{provider} delivers to {endpoint}")),
                }
            }

            // Asked of the mailer itself rather than re-derived here, so that
            // "configured" means one thing across doctor, the console and the
            // sending path. This check used to pass on `transport: "smtp"`
            // with no host — the one state in which the platform claims to
            // send and does not.
            let mailer = pro_mail::Mailer::new(live.mail(), live.platform_name());
            match mailer.not_live_reason() {
                None => check(true, "mail", "SMTP configured"),
                Some(reason) => {
                    check(
                        false,
                        "mail",
                        &format!("{reason}. Customers receive no email — set this in the console."),
                    );
                    problems += 1;
                }
            }

            let tenants = pro_db::tenants::count_by_status(&db).await.unwrap_or_default();
            let total: i64 = tenants.iter().map(|(_, n)| n).sum();
            check(true, "sites", &format!("{total} on this platform"));
        }
        Err(e) => {
            check(false, "database", &e.to_string());
            problems += 1;
        }
    }

    // ---- ports -----------------------------------------------------------
    let bind = format!("{}:{}", config.server.host, config.edge_port());
    match tokio::net::TcpListener::bind(&bind).await {
        Ok(listener) => {
            drop(listener);
            check(true, "edge port", &format!("{bind} is free"));
        }
        Err(e) => {
            // Binding 443 as a non-root user is the classic one, and it has a
            // specific fix worth naming.
            let hint = if config.edge_port() < 1024 && e.kind() == std::io::ErrorKind::PermissionDenied
            {
                " — a port below 1024 needs privileges. Grant them with: \
                 sudo setcap 'cap_net_bind_service=+ep' $(which ghost-pro)"
            } else {
                ""
            };
            check(false, "edge port", &format!("cannot bind {bind}: {e}{hint}"));
            problems += 1;
        }
    }

    let range = config.tenants.port_ceiling - config.tenants.port_floor + 1;
    check(
        true,
        "tenant ports",
        &format!(
            "{}–{} ({range} sites per machine)",
            config.tenants.port_floor, config.tenants.port_ceiling
        ),
    );

    // ---- TLS -------------------------------------------------------------
    if config.ssl.enabled {
        if config.ssl.staging {
            check(
                true,
                "tls",
                "Let's Encrypt STAGING — browsers will not trust these certificates",
            );
        } else {
            check(true, "tls", "Let's Encrypt");
        }
        if config.ssl.email.is_none() {
            check(
                false,
                "tls contact",
                "ssl.email is unset — nobody will be warned if renewal starts failing",
            );
            problems += 1;
        }
        if config.host() == "localhost" || config.host().ends_with(".localhost") {
            check(
                false,
                "tls hostname",
                &format!(
                    "{} cannot be validated by a certificate authority",
                    config.host()
                ),
            );
            problems += 1;
        }
    } else if config.ssl.behind_proxy {
        check(true, "tls", "terminated by the proxy in front");
    } else {
        check(true, "tls", "off — plain HTTP, right for local use only");
    }

    // ---- public addresses, for customers' DNS ----------------------------
    if config.public_addresses.is_empty() {
        check(
            true,
            "public addresses",
            "not set — customers attaching an apex domain will be shown no A record",
        );
    } else {
        check(
            true,
            "public addresses",
            &config.public_addresses.join(", "),
        );
    }

    println!();
    if problems == 0 {
        println!("Everything checks out. Run: ghost-pro serve\n");
        Ok(())
    } else {
        println!("{problems} problem(s) above need attention.\n");
        // A non-zero exit so this is usable in a deployment script.
        Err(ProError::Config(format!("{problems} problem(s) found")))
    }
}

fn check(ok: bool, label: &str, detail: &str) {
    println!("  {} {label:<18} {detail}", if ok { "✓" } else { "✗" });
}

/// Create the database and the schema, and report what was found.
///
/// This exists so that pointing a deployment at MySQL is the same single step
/// as running locally on SQLite: name a server, run this, and the platform has
/// its schema. The alternative — telling an operator to run `CREATE DATABASE`
/// and then a pile of DDL by hand — is how a production schema ends up one
/// migration behind the binary.
///
/// Safe to run repeatedly. Creating the database and applying migrations are
/// both no-ops once done, so this is the right thing to put in a deploy
/// script ahead of `serve`.
pub async fn db() -> Result<()> {
    let config = Config::load(None)?;
    let target = config.db_target()?;
    let store = config.tenant_store()?;

    println!("\nghost-pro database\n");
    check(true, "engine", target.engine().name());
    // Redacted: this is the one config value that routinely holds a password,
    // and an operator runs this with somebody watching their screen.
    check(true, "url", &target.redacted());
    if let Some(path) = target.sqlite_path() {
        check(true, "file", &path.display().to_string());
    }

    let pool = pro_db::open(&target, config.database.pool_size(target.engine())).await?;
    check(true, "schema", "created and migrated");

    // Proof the schema is really usable, not merely that the migration table
    // says so — a half-applied migration would pass the line above.
    let staff = pro_db::staff::count(&pool).await?;
    let (tenants, accounts) =
        (pro_db::tenants::count_by_status(&pool).await?, pro_db::accounts::count(&pool).await?);
    let sites: i64 = tenants.iter().map(|(_, n)| n).sum();
    check(true, "contents", &format!("{staff} staff, {accounts} accounts, {sites} sites"));

    match &store {
        pro_core::TenantStore::SqliteFile => check(
            true,
            "tenant databases",
            "one SQLite file per site, inside its own directory",
        ),
        pro_core::TenantStore::MySqlServer { server, prefix } => {
            check(
                true,
                "tenant databases",
                &format!("{}, one per site, named {prefix}<id>", pro_core::database::redact(server)),
            );
            // The grants are checked now rather than at the first signup,
            // because that failure would strand a customer who has already
            // paid, mid-provision, with a half-created site.
            match pro_provision::tenant_db::check_server(&store).await {
                Ok(()) => check(true, "tenant grants", "can create a database and a user"),
                Err(e) => {
                    check(false, "tenant grants", &e.to_string());
                    println!();
                    return Err(e);
                }
            }
        }
    }

    if config.env == "production" && target.engine().is_single_machine() {
        println!(
            "\nNote: this is a SQLite file, so the platform is pinned to this machine.\n\
             Set database.url to a mysql:// server in pro.production.json to change that."
        );
    }

    println!("\nReady. Run: ghost-pro serve\n");
    Ok(())
}

/// Create an operator login.
///
/// The only way to make the first one. A platform with no staff cannot be
/// administered through its own console, and there is deliberately no
/// self-service route in — an open "become the owner" page on a fresh install
/// is a race anybody on the internet can win.
pub async fn create_staff(args: Args) -> Result<()> {
    let config = Config::load(None)?;
    let target = config.db_target()?;
    let db = pro_db::open(&target, config.database.pool_size(target.engine())).await?;
    let interactive = std::io::stdin().is_terminal();

    let email = match args.get("email") {
        Some(e) => e.to_string(),
        None if interactive => ask("Email", None)?,
        None => return Err(ProError::Config("--email is required".into())),
    };
    let name = match args.get("name") {
        Some(n) => n.to_string(),
        None if interactive => ask("Name", None)?,
        None => return Err(ProError::Config("--name is required".into())),
    };

    let role = match args.get("role") {
        Some(r) => StaffRole::parse(r)
            .ok_or_else(|| ProError::Config(format!("unknown role {r:?}")))?,
        None => StaffRole::Owner,
    };

    let password = match args.get("password") {
        Some(p) => p.to_string(),
        None if interactive => {
            // Read without echo where the platform offers it; the fallback is
            // a visible prompt, which is better than refusing to proceed.
            print!("Password (at least 12 characters): ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim().to_string()
        }
        None => {
            return Err(ProError::Config(
                "--password is required when not running interactively".into(),
            ))
        }
    };

    // The first login is the owner regardless of what was asked for: a
    // platform whose only staff member could not change the payment provider
    // would be unusable.
    let existing = pro_db::staff::count(&db).await?;
    let effective_role = if existing == 0 { StaffRole::Owner } else { role };
    if existing == 0 && role != StaffRole::Owner {
        println!("(The first login must be an owner, so --role was ignored.)");
    }

    let member =
        pro_db::staff::create(&db, &email, &name, &password, effective_role).await?;

    pro_db::audit::record(
        &db,
        "cli",
        "staff.created",
        None,
        None,
        serde_json::json!({"staff": member.id, "role": effective_role.as_str()}),
    )
    .await;

    println!(
        "\nCreated {} as {}.\n\nSign in at {}/console\n",
        member.email,
        effective_role.label(),
        config.canonical_url()
    );
    Ok(())
}

fn ask(prompt: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(d) => print!("{prompt} [{d}]: "),
        None => print!("{prompt}: "),
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let value = line.trim().to_string();
    if value.is_empty() {
        match default {
            Some(d) => Ok(d.to_string()),
            None => Err(ProError::Config(format!("{prompt} is required"))),
        }
    } else {
        Ok(value)
    }
}
