//! Per-site MySQL provisioning, against a real server.
//!
//! The decision this proves out is the one that matters for a hosting
//! platform: each site gets a database *and a login* of its own, and that
//! login can reach nothing else. A unit test can check the SQL that is built;
//! only a real server can confirm that the grant it produces actually refuses
//! the other customer's data.
//!
//! Skipped unless `PRO_TEST_MYSQL_URL` names a server with an account holding
//! `CREATE DATABASE`, `CREATE USER` and `GRANT`:
//!
//! ```sh
//! docker run -d --name ghostpro-mysql -e MYSQL_ROOT_PASSWORD=rootpw -p 13306:3306 mysql:8
//! PRO_TEST_MYSQL_URL='mysql://root:rootpw@127.0.0.1:13306' \
//!   cargo test -p pro-provision --test tenant_mysql
//! ```
use pro_core::TenantStore;
use pro_provision::tenant_db;

fn store() -> Option<TenantStore> {
    let server = std::env::var("PRO_TEST_MYSQL_URL").ok().filter(|s| !s.trim().is_empty())?;
    Some(TenantStore::MySqlServer {
        server: server.trim_end_matches('/').to_string(),
        // A prefix of its own so these tests cannot collide with a real
        // deployment that happens to share the server.
        prefix: "ghost_t_".into(),
    })
}

macro_rules! mysql_test {
    ($name:ident, |$store:ident| $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($store) = store() else {
                eprintln!("skipping {}: PRO_TEST_MYSQL_URL is not set", stringify!($name));
                return;
            };
            $body
        }
    };
}

/// A 24-hex id, as a real tenant has.
fn tenant_id() -> String {
    pro_core::ids::object_id()
}

// The configured account can do what provisioning needs, and leaves nothing
// behind when it checks.
mysql_test!(the_server_check_cleans_up_after_itself, |store| {
    tenant_db::check_server(&store).await.expect("root should be able to provision");

    let before = probe_databases(&store, "ghost_probe_").await;
    tenant_db::check_server(&store).await.unwrap();
    let after = probe_databases(&store, "ghost_probe_").await;
    assert_eq!(before, after, "the grant check left a probe database behind");
});

// Provisioning produces a usable database, and the credentials handed to the
// site actually work — which is the thing a unit test cannot establish.
mysql_test!(a_site_can_use_the_credentials_it_is_given, |store| {
    let id = tenant_id();
    let creds = tenant_db::create(&store, &id).await.expect("provision");

    assert_eq!(creds.database, format!("ghost_t_{id}"));
    assert_eq!(creds.port, 13306, "the port must come from the configured server");

    // Connect as the tenant, not as the admin.
    let pool = connect_as(&creds).await.expect("the site should be able to connect");
    sqlx::query("CREATE TABLE posts (id VARCHAR(24) PRIMARY KEY, title VARCHAR(255))")
        .execute(&pool)
        .await
        .expect("a site must be able to create its own schema");
    sqlx::query("INSERT INTO posts (id, title) VALUES ('1', 'hello')")
        .execute(&pool)
        .await
        .unwrap();
    let (title,): (String,) =
        sqlx::query_as("SELECT title FROM posts WHERE id = '1'").fetch_one(&pool).await.unwrap();
    assert_eq!(title, "hello");
    pool.close().await;

    tenant_db::drop(&store, &id).await.unwrap();
});

// The isolation claim, tested rather than asserted.
//
// This is the whole reason for a database and a user per site instead of one
// shared schema with table prefixes. If this test ever fails, one customer's
// Ghost can read another customer's posts, and no amount of care in the CMS
// would prevent it.
mysql_test!(one_site_cannot_reach_another_sites_data, |store| {
    let (alice, bob) = (tenant_id(), tenant_id());
    let a = tenant_db::create(&store, &alice).await.unwrap();
    let b = tenant_db::create(&store, &bob).await.unwrap();

    // Bob writes something private.
    let bob_pool = connect_as(&b).await.unwrap();
    sqlx::query("CREATE TABLE secrets (body VARCHAR(255))").execute(&bob_pool).await.unwrap();
    sqlx::query("INSERT INTO secrets VALUES ('bob draft')").execute(&bob_pool).await.unwrap();
    bob_pool.close().await;

    // Alice, connected as her own user, tries to read it by name.
    let alice_pool = connect_as(&a).await.unwrap();
    let cross = sqlx::query(&format!("SELECT body FROM `{}`.secrets", b.database))
        .fetch_all(&alice_pool)
        .await;
    assert!(
        cross.is_err(),
        "one tenant read another tenant's table — the per-site grant is not isolating"
    );

    // She cannot even see that it exists.
    let visible: Vec<(String,)> = sqlx::query_as(
        "SELECT schema_name FROM information_schema.schemata WHERE schema_name = ?",
    )
    .bind(&b.database)
    .fetch_all(&alice_pool)
    .await
    .unwrap();
    assert!(visible.is_empty(), "one tenant can enumerate another tenant's database");

    // And she cannot widen her own access.
    let escalate = sqlx::query(&format!("GRANT ALL ON `{}`.* TO '{}'@'%'", b.database, a.user))
        .execute(&alice_pool)
        .await;
    assert!(escalate.is_err(), "a tenant granted itself access to another tenant");
    alice_pool.close().await;

    tenant_db::drop(&store, &alice).await.unwrap();
    tenant_db::drop(&store, &bob).await.unwrap();
});

// Provisioning twice must not fail, because a retried provision is normal —
// the first attempt may have died after creating the database.
mysql_test!(provisioning_is_idempotent, |store| {
    let id = tenant_id();
    let first = tenant_db::create(&store, &id).await.unwrap();
    let second = tenant_db::create(&store, &id).await.expect("a retry must not fail");

    assert_eq!(first.database, second.database);
    assert_eq!(first.user, second.user);
    // The password is reissued, which is why the caller writes the config
    // before starting the site.
    assert_ne!(first.password, second.password);

    // The newest credentials are the ones that work.
    let pool = connect_as(&second).await.expect("the reissued password should work");
    pool.close().await;

    tenant_db::drop(&store, &id).await.unwrap();
});

// Deleting a site removes its database and its login. A database left behind
// is a former customer's content that nobody is looking after.
mysql_test!(deleting_a_site_removes_its_database_and_user, |store| {
    let id = tenant_id();
    let creds = tenant_db::create(&store, &id).await.unwrap();
    assert!(database_exists(&store, &creds.database).await);
    assert!(user_exists(&store, &creds.user).await);

    tenant_db::drop(&store, &id).await.unwrap();
    assert!(!database_exists(&store, &creds.database).await, "the database survived deletion");
    assert!(!user_exists(&store, &creds.user).await, "the login survived deletion");

    // And deleting again is not an error — a retried deletion is normal.
    tenant_db::drop(&store, &id).await.expect("a second delete must be a no-op");
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn connect_as(c: &tenant_db::Credentials) -> Result<sqlx::AnyPool, sqlx::Error> {
    sqlx::any::install_default_drivers();
    let url = format!(
        "mysql://{}:{}@{}:{}/{}",
        c.user, c.password, c.host, c.port, c.database
    );
    sqlx::any::AnyPoolOptions::new().max_connections(2).connect(&url).await
}

async fn admin(store: &TenantStore) -> sqlx::AnyPool {
    sqlx::any::install_default_drivers();
    let TenantStore::MySqlServer { server, .. } = store else { unreachable!() };
    sqlx::AnyPool::connect(&format!("{server}/mysql")).await.unwrap()
}

async fn database_exists(store: &TenantStore, name: &str) -> bool {
    let pool = admin(store).await;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT schema_name FROM information_schema.schemata WHERE schema_name = ?",
    )
    .bind(name)
    .fetch_all(&pool)
    .await
    .unwrap();
    pool.close().await;
    !rows.is_empty()
}

async fn user_exists(store: &TenantStore, user: &str) -> bool {
    let pool = admin(store).await;
    let rows: Vec<(String,)> = sqlx::query_as("SELECT user FROM mysql.user WHERE user = ?")
        .bind(user)
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    !rows.is_empty()
}

async fn probe_databases(store: &TenantStore, prefix: &str) -> usize {
    let pool = admin(store).await;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT schema_name FROM information_schema.schemata WHERE schema_name LIKE ?",
    )
    .bind(format!("{prefix}%"))
    .fetch_all(&pool)
    .await
    .unwrap();
    pool.close().await;
    rows.len()
}
