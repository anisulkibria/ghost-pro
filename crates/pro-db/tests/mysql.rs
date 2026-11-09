//! The control plane on MySQL, exercised against a real server.
//!
//! These are the tests the SQLite suite cannot stand in for. Everything in
//! `pro-db`'s unit tests runs on an in-memory SQLite database, which proves
//! the queries are *correct* but says nothing about whether MySQL will accept
//! them — and the ways the two engines differ are exactly the ways that do not
//! show up until production: a reserved word, a `TEXT` column that cannot be a
//! key, a duplicate-key error worded differently, an upsert MySQL has no
//! syntax for.
//!
//! Skipped unless `PRO_TEST_MYSQL_URL` names a server, so a checkout with no
//! MySQL still passes `cargo test`. To run them:
//!
//! ```sh
//! docker run -d --name ghostpro-mysql -e MYSQL_ROOT_PASSWORD=rootpw -p 13306:3306 mysql:8
//! PRO_TEST_MYSQL_URL='mysql://root:rootpw@127.0.0.1:13306' cargo test -p pro-db --test mysql
//! ```
//!
//! The URL names a *server*, with no database: each test creates its own and
//! drops it afterwards, so they neither interfere with each other nor leave
//! anything behind.
use pro_core::DbTarget;
use pro_db::Db;

/// The server to test against, or `None` to skip.
fn server() -> Option<String> {
    std::env::var("PRO_TEST_MYSQL_URL").ok().filter(|u| !u.trim().is_empty())
}

/// A fresh, migrated database of its own, and the name to drop afterwards.
async fn fixture() -> Option<(Db, String, String)> {
    let server = server()?;
    let name = format!("pro_test_{}", pro_core::ids::object_id());
    let url = format!("{}/{name}", server.trim_end_matches('/'));
    let target = DbTarget::parse(&url).expect("a mysql:// url");
    // `open` creates the database, so there is no setup step here — which is
    // the behaviour an operator depends on, and this is where it is tested.
    let pool = pro_db::open(&target, 4).await.expect("open mysql");
    Some((pool, name, server))
}

async fn teardown(pool: Db, name: &str, server: &str) {
    pool.close().await;
    let admin = format!("{}/mysql", server.trim_end_matches('/'));
    if let Ok(p) = sqlx::AnyPool::connect(&admin).await {
        let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS `{name}`")).execute(&p).await;
        p.close().await;
    }
}

/// `$pool` is bound to a live MySQL pool, or the test returns early.
macro_rules! mysql_test {
    ($name:ident, |$pool:ident| $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some(($pool, db_name, srv)) = fixture().await else {
                eprintln!("skipping {}: PRO_TEST_MYSQL_URL is not set", stringify!($name));
                return;
            };
            $body
            teardown($pool, &db_name, &srv).await;
        }
    };
}

// ---------------------------------------------------------------------------
// The schema itself
// ---------------------------------------------------------------------------

// The migration runs at all, and produces the tables the queries expect.
//
// This is the test that catches a `TEXT PRIMARY KEY` or a `CREATE INDEX IF
// NOT EXISTS` surviving into the MySQL file — both are accepted by SQLite and
// rejected outright here.
mysql_test!(the_schema_applies, |pool| {
    let expected = [
        "accounts", "audit_events", "backups", "domains", "jobs",
        "platform_settings", "sessions", "staff", "subscriptions", "tenants",
        "tokens", "usage_samples", "webhook_events",
    ];
    for table in expected {
        let (n,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("{table} is not usable on MySQL: {e}"));
        assert_eq!(n, 0, "{table} should start empty");
    }
});

// ---------------------------------------------------------------------------
// The constructs that differ between engines
// ---------------------------------------------------------------------------

// `key` is a reserved word in MySQL, and the settings upsert is spelled
// `ON DUPLICATE KEY UPDATE` rather than `ON CONFLICT`. Both are produced by
// `pro_db::dialect`, and this is the only place the MySQL spelling is ever
// actually run.
mysql_test!(settings_insert_and_update_in_place, |pool| {
    let config = std::sync::Arc::new(test_config());
    let store = pro_db::SettingsStore::load(&pool, config).await.unwrap();
    let role = pro_core::StaffRole::Owner;

    // `mail.fromAddress` rather than a billing key: switching provider is
    // refused unless its credentials are supplied in the same save, and this
    // test is about the upsert, not that rule.
    let set = |v: &str| vec![("mail.fromAddress".to_string(), Some(serde_json::json!(v)))];

    store.save(&pool, &set("first@example.com"), role, "staff:test").await.unwrap();
    assert_eq!(store.current().mail().from_address.as_deref(), Some("first@example.com"));

    // The second save is the half that matters: the row already exists, so
    // this takes the conflict branch MySQL spells `ON DUPLICATE KEY UPDATE`
    // and SQLite spells `ON CONFLICT … DO UPDATE`.
    store.save(&pool, &set("second@example.com"), role, "staff:test").await.unwrap();
    assert_eq!(store.current().mail().from_address.as_deref(), Some("second@example.com"));

    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM platform_settings WHERE `key` = 'mail.fromAddress'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 1, "the upsert inserted a second row instead of updating in place");

    // Reading it back through the store also proves `value` decodes — it is a
    // VARCHAR here precisely because a MySQL TEXT would not.
    store.refresh(&pool).await.unwrap();
    assert_eq!(store.current().mail().from_address.as_deref(), Some("second@example.com"));

    // Clearing deletes the override, so the config file shows through again.
    let clear = vec![("mail.fromAddress".to_string(), None)];
    store.save(&pool, &clear, role, "staff:test").await.unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM platform_settings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
});

// Every string column that the platform reads back has to decode.
//
// This is the guard for the trap described in `pro_db::dialect`: a MySQL
// `TEXT` column arrives as a blob and fails only at read time, so a column
// added as `TEXT` would pass migration, pass an insert, and break a page.
// Reading one row of every table is the cheapest way to prove the whole
// schema decodes.
mysql_test!(every_table_decodes_the_values_it_stores, |pool| {
    let account = seed_account(&pool).await;
    let tenant = pro_db::tenants::create(&pool, &account, "decode", "Decode", "starter", "2026-01-01T00:00:00Z", 21000, 21010)
        .await
        .unwrap();

    // tenants: suspended_reason and notes are both nullable strings.
    pro_db::tenants::suspend(&pool, &tenant.id, "unpaid").await.unwrap();
    let back = pro_db::tenants::get(&pool, &tenant.id).await.unwrap();
    assert_eq!(back.suspended_reason.as_deref(), Some("unpaid"));

    // domains: last_error.
    pro_db::domains::add(&pool, &tenant.id, "decode.example.com", pro_core::DomainKind::Custom, false)
        .await
        .unwrap();
    let domains = pro_db::domains::list_for_tenant(&pool, &tenant.id).await.unwrap();
    let custom = domains.iter().find(|d| d.kind == pro_core::DomainKind::Custom).unwrap();
    pro_db::domains::set_status(&pool, &custom.id, pro_core::DomainStatus::Failed, Some("no A record"))
        .await
        .unwrap();
    let checked = pro_db::domains::get(&pool, &custom.id).await.unwrap();
    assert_eq!(checked.last_error.as_deref(), Some("no A record"));

    // jobs: payload and last_error.
    let job_id =
        pro_db::jobs::enqueue(&pool, "verify_domain", Some(&tenant.id), serde_json::json!({"a": 1}))
            .await
            .unwrap();
    let claimed = pro_db::jobs::claim(&pool).await.unwrap().expect("a queued job");
    assert_eq!(claimed.id, job_id);
    // Stored as a string and parsed back, so this proves both the decode and
    // that the round trip produced valid JSON rather than a blob of bytes.
    assert_eq!(claimed.payload["a"], 1, "job payload did not decode: {}", claimed.payload);
    pro_db::jobs::fail(&pool, &claimed, "boom").await.unwrap();

    // audit_events: meta.
    pro_db::audit::record(
        &pool,
        "staff:test",
        "tenant.suspend",
        Some(&tenant.id),
        None,
        serde_json::json!({"why": "test"}),
    )
    .await;
    let events = pro_db::audit::list(&pool, Some(&tenant.id), None, 10, 0).await.unwrap();
    assert!(
        events.iter().any(|e| e.meta["why"] == "test"),
        "audit meta did not decode: {events:?}"
    );

    // sessions: user_agent.
    let token = pro_db::sessions::create(
        &pool,
        pro_db::sessions::Subject::Account,
        &account,
        Some("127.0.0.1"),
        Some("Mozilla/5.0 (test)"),
    )
    .await
    .unwrap();
    assert!(!token.is_empty());
    let listed = pro_db::sessions::list_for(&pool, pro_db::sessions::Subject::Account, &account)
        .await
        .unwrap();
    assert!(
        listed.iter().any(|s| s.user_agent.as_deref() == Some("Mozilla/5.0 (test)")),
        "session user_agent did not decode"
    );
});

// A payload at the cap has to survive storage and come back as a String.
//
// `MAX_PAYLOAD_CHARS` exists because MySQL reports any string wider than
// about 16,000 characters as a blob, which sqlx's `Any` driver will not
// decode — so this is the test that pins the cap to something that works.
mysql_test!(a_large_webhook_payload_round_trips, |pool| {
    let big = "x".repeat(200_000);
    pro_db::audit::claim_webhook(&pool, "stripe", "evt_big", "invoice.paid", &big).await.unwrap();

    let stored = pro_db::audit::webhook_payload(&pool, "stripe", "evt_big")
        .await
        .unwrap()
        .expect("the payload was kept for replay");
    assert_eq!(
        stored.len(),
        pro_db::audit::MAX_PAYLOAD_CHARS,
        "the payload was truncated differently than MAX_PAYLOAD_CHARS promises"
    );
    assert!(stored.chars().all(|c| c == 'x'));
});

// A duplicate key must read as a conflict, not as an internal error.
//
// MySQL words this as `Duplicate entry … for key …`, which shares no text
// with SQLite's `UNIQUE constraint failed`. If `pro_db::unique` does not know
// both, a customer taking an existing slug gets a 500 instead of "that
// address is taken", and a webhook retry is redelivered forever.
mysql_test!(a_duplicate_slug_is_a_conflict_not_a_fault, |pool| {
    let account = seed_account(&pool).await;
    pro_db::tenants::create(&pool, &account, "taken", "Taken", "starter", "2026-01-01T00:00:00Z", 21000, 21010)
        .await
        .unwrap();

    let err = pro_db::tenants::create(&pool, &account, "taken", "Again", "starter", "2026-01-01T00:00:00Z", 21000, 21010)
        .await
        .expect_err("the second site took the same slug");

    assert!(
        matches!(err, pro_core::ProError::Conflict(_)),
        "a duplicate slug on MySQL surfaced as {err:?} rather than a conflict — \
         pro_db::unique does not recognise MySQL's wording"
    );
});

// Slug uniqueness is case-insensitive. SQLite gets that from a `lower(slug)`
// index; MySQL gets it from the column's collation, which is a different
// mechanism for the same guarantee — so it has to be checked, not assumed.
mysql_test!(slug_uniqueness_ignores_case, |pool| {
    let account = seed_account(&pool).await;
    pro_db::tenants::create(&pool, &account, "acme", "Acme", "starter", "2026-01-01T00:00:00Z", 21000, 21010)
        .await
        .unwrap();
    let err = pro_db::tenants::create(&pool, &account, "ACME", "Acme Two", "starter", "2026-01-01T00:00:00Z", 21000, 21010)
        .await
        .expect_err("ACME and acme are the same hostname");
    assert!(matches!(err, pro_core::ProError::Conflict(_)), "{err:?}");
});

// The same for account addresses: `Owner@example.com` and
// `owner@example.com` must not be two logins.
//
// Inserted with raw SQL on purpose. `accounts::create` lowercases the address
// in Rust before it ever reaches the database, so going through it would test
// that `to_lowercase` works and prove nothing about the index. The index is
// the backstop — the thing that still holds if a future caller forgets — and
// on MySQL it is a different mechanism from SQLite's `lower(email)`.
mysql_test!(the_email_index_itself_ignores_case, |pool| {
    let insert = "INSERT INTO accounts (id, email, name, password_hash, status, created_at) \
                  VALUES (?, ?, 'Owner', 'hash', 'active', ?)";
    sqlx::query(insert)
        .bind(pro_core::ids::object_id())
        .bind("Owner@Example.com")
        .bind(pro_core::now_iso())
        .execute(&pool)
        .await
        .unwrap();

    let err = sqlx::query(insert)
        .bind(pro_core::ids::object_id())
        .bind("owner@example.com")
        .bind(pro_core::now_iso())
        .execute(&pool)
        .await
        .expect_err("the same address in different case was accepted");

    // And the error has to be recognisable as a duplicate, since that is what
    // turns it into "that address is already registered" rather than a 500.
    assert!(
        pro_db::unique::is_unique_violation(&err.to_string(), &["accounts_email_unique", "accounts.email"]),
        "unrecognised duplicate wording: {err}"
    );
});

// Pruning backups uses "every row after the first N", which SQLite spells
// `LIMIT -1 OFFSET ?` and MySQL rejects outright.
mysql_test!(backup_pruning_skips_the_newest, |pool| {
    let account = seed_account(&pool).await;
    let tenant = pro_db::tenants::create(&pool, &account, "site", "Site", "starter", "2026-01-01T00:00:00Z", 21000, 21010)
        .await
        .unwrap();

    for (i, at) in ["2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z", "2026-01-03T00:00:00Z"]
        .iter()
        .enumerate()
    {
        sqlx::query(
            "INSERT INTO backups (id, tenant_id, path, size_bytes, reason, created_at) \
             VALUES (?, ?, ?, ?, 'auto', ?)",
        )
        .bind(pro_core::ids::object_id())
        .bind(&tenant.id)
        .bind(format!("/tmp/b{i}.zip"))
        .bind(10i64)
        .bind(*at)
        .execute(&pool)
        .await
        .unwrap();
    }

    let pruned = pro_db::backups::take_pruned(&pool, &tenant.id, 2).await.unwrap();
    assert_eq!(pruned.len(), 1, "keeping 2 of 3 should prune exactly 1");
    assert!(pruned[0].1.ends_with("b0.zip"), "the oldest should be pruned, got {:?}", pruned[0]);

    // And "keep more than exist" must prune nothing rather than erroring.
    assert!(pro_db::backups::take_pruned(&pool, &tenant.id, 10).await.unwrap().is_empty());
});

// ---------------------------------------------------------------------------
// Types and values round-tripping through the `Any` driver
// ---------------------------------------------------------------------------

// `Any` carries a narrow set of types, and MySQL maps them differently from
// SQLite: `SMALLINT` for a boolean, `BIGINT` for an i64. A value that does
// not survive the round trip would corrupt a routing decision or a quota.
mysql_test!(booleans_and_big_integers_round_trip, |pool| {
    let account = seed_account(&pool).await;
    let tenant = pro_db::tenants::create(&pool, &account, "types", "Types", "starter", "2026-01-01T00:00:00Z", 21000, 21010)
        .await
        .unwrap();

    // `is_primary` is a TINYINT(1) here and an INTEGER on SQLite; both must
    // decode to `bool`.
    let host = "types.example.com";
    pro_db::domains::add(&pool, &tenant.id, host, pro_core::DomainKind::Subdomain, true)
        .await
        .unwrap();
    let primary = pro_db::domains::primary_for_tenant(&pool, &tenant.id).await.unwrap();
    let primary = primary.expect("a primary domain");
    assert!(primary.is_primary, "the primary flag did not survive MySQL");
    assert_eq!(primary.domain, host);

    // Beyond 2^31, which is why the column is BIGINT and not INT.
    let big = 9_000_000_000i64;
    pro_db::tenants::set_storage_bytes(&pool, &tenant.id, big).await.unwrap();
    let back = pro_db::tenants::get(&pool, &tenant.id).await.unwrap();
    assert_eq!(back.storage_bytes, big, "a large byte count was truncated");

    // The port is bound as i64 and read back as u16.
    assert_eq!(back.port, 21000);
});

// Unicode has to survive. MySQL's default `utf8` is three-byte and silently
// mangles anything outside the BMP, which is why the schema says `utf8mb4` —
// a site titled with an emoji is not exotic.
mysql_test!(four_byte_unicode_survives, |pool| {
    let name = "Café 🎉 日本語";
    let account = seed_account(&pool).await;
    let tenant = pro_db::tenants::create(&pool, &account, "uni", name, "starter", "2026-01-01T00:00:00Z", 21000, 21010)
        .await
        .unwrap();
    let back = pro_db::tenants::get(&pool, &tenant.id).await.unwrap();
    assert_eq!(back.name, name, "utf8mb4 is not in effect");
});

// Timestamps are ISO-8601 strings, not native `DATETIME`, so that `Any` can
// carry them and so that they sort as text. A `DATETIME` column would come
// back in MySQL's own format and break every comparison.
mysql_test!(timestamps_stay_sortable_strings, |pool| {
    let account = seed_account(&pool).await;
    for (slug, port) in [("a", 21000), ("b", 21001)] {
        pro_db::tenants::create(&pool, &account, slug, slug, "starter", "2026-01-01T00:00:00Z", port, port + 1)
            .await
            .unwrap();
    }
    let created: Vec<(String,)> =
        sqlx::query_as("SELECT created_at FROM tenants ORDER BY created_at")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(created.len(), 2);
    for (at,) in &created {
        assert!(at.ends_with('Z'), "{at} is not the ISO-8601 the platform writes");
        assert!(pro_core::parse_iso(at).is_some(), "{at} does not parse back");
    }
});

// ---------------------------------------------------------------------------
// Transactions and the races they protect
// ---------------------------------------------------------------------------

// Port allocation and the insert are one transaction, so two signups racing
// cannot be told to bind the same port. InnoDB's locking is not SQLite's, so
// this is worth proving here rather than inferring.
mysql_test!(ports_are_allocated_without_collision, |pool| {
    let account = seed_account(&pool).await;
    let mut handles = Vec::new();
    for i in 0..8 {
        let pool = pool.clone();
        let account = account.clone();
        handles.push(tokio::spawn(async move {
            pro_db::tenants::create(
                &pool,
                &account,
                &format!("race{i}"),
                "Race",
                "starter",
                "2026-01-01T00:00:00Z",
                21000,
                21100,
            )
            .await
        }));
    }

    let mut ports = Vec::new();
    for h in handles {
        // A loser of the race is allowed to come back as a conflict and be
        // retried; what is not allowed is two tenants on one port.
        if let Ok(Ok(t)) = h.await {
            ports.push(t.port);
        }
    }
    assert!(!ports.is_empty(), "every concurrent create failed");
    let mut sorted = ports.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), ports.len(), "two tenants were given the same port: {ports:?}");
});

// Webhook idempotency: claiming an event twice must fail the second time, or
// a provider's retry would grant a second subscription.
mysql_test!(a_webhook_event_can_only_be_claimed_once, |pool| {
    use pro_db::audit::WebhookClaim;

    let first = pro_db::audit::claim_webhook(&pool, "stripe", "evt_1", "invoice.paid", "{}")
        .await
        .unwrap();
    assert!(matches!(first, WebhookClaim::Fresh), "the first delivery should be claimed");

    // The duplicate is detected from the unique index, which means this also
    // depends on `pro_db::unique` knowing MySQL's wording. Getting it wrong
    // here does not just double-process — it returns an error to the provider,
    // which then redelivers the same event forever.
    let second = pro_db::audit::claim_webhook(&pool, "stripe", "evt_1", "invoice.paid", "{}")
        .await
        .unwrap();
    assert!(
        matches!(second, WebhookClaim::Duplicate),
        "a redelivery came back as {second:?} rather than Duplicate"
    );
});

// Foreign keys must actually be enforced, or a domain row could outlive the
// tenant it routes to and the proxy would serve a dangling hostname. InnoDB
// enforces them by default; this guards against a migration that forgot the
// constraint.
mysql_test!(foreign_keys_are_enforced, |pool| {
    let err = sqlx::query(
        "INSERT INTO domains (id, tenant_id, domain, kind, status, is_primary, \
         verification_token, created_at) VALUES (?, 'nosuchtenant000000000000', \
         'orphan.example.com', 'custom', 'pending', 0, '', ?)",
    )
    .bind(pro_core::ids::object_id())
    .bind(pro_core::now_iso())
    .execute(&pool)
    .await;
    assert!(err.is_err(), "a domain was inserted for a tenant that does not exist");
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn seed_account(pool: &Db) -> String {
    pro_db::accounts::create(
        pool,
        &format!("{}@example.com", pro_core::ids::object_id()),
        "Test",
        "hash",
    )
    .await
    .unwrap()
    .id
}

fn test_config() -> pro_core::Config {
    let mut cfg: pro_core::Config = serde_json::from_value(serde_json::json!({
        "url": "https://pro.example.com",
        "subdomainBase": "sites.example.com",
    }))
    .unwrap();
    cfg.plans = pro_core::plans::default_plans();
    cfg
}

mysql_test!(native_click_changes_are_transactional_and_revisioned, |pool| {
    let account=pro_db::accounts::create(&pool,"click@mysql.test","Click","unused").await.unwrap();
    let t=pro_db::tenants::create(&pool,&account.id,"click","Click","starter",&pro_core::now_iso(),22000,23000).await.unwrap();
    let first:i64=sqlx::query_scalar("SELECT revision FROM click_sync WHERE tenant_id=?").bind(&t.id).fetch_one(&pool).await.unwrap();
    let mut tx=pool.begin().await.unwrap();
    sqlx::query("UPDATE tenants SET status='active' WHERE id=?").bind(&t.id).execute(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(first,sqlx::query_scalar::<_,i64>("SELECT revision FROM click_sync WHERE tenant_id=?").bind(&t.id).fetch_one(&pool).await.unwrap());
    pro_db::tenants::set_status(&pool,&t.id,pro_core::TenantStatus::Active).await.unwrap();
    pro_db::domains::add(&pool,&t.id,"click.example.test",pro_core::DomainKind::Subdomain,true).await.unwrap();
    let revision:i64=sqlx::query_scalar("SELECT revision FROM click_sync WHERE tenant_id=?").bind(&t.id).fetch_one(&pool).await.unwrap();
    assert_eq!(first+2,revision);
});
