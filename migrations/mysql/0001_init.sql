-- The control plane's own database, on MySQL.
--
-- The companion of `migrations/sqlite/0001_init.sql`. Same tables, same
-- columns, same meaning — the query code above does not know which engine it
-- is on, so any divergence here is a bug. `pro_db::tests` compares the two.
--
-- Why a separate file rather than one portable dialect: MySQL cannot index a
-- `TEXT` primary key without a prefix length, has no `CREATE INDEX IF NOT
-- EXISTS`, has no partial indexes, and cannot index an expression before
-- 8.0.13. Writing to the intersection of both engines would mean a schema
-- that is worse on both.
--
-- Three conventions apply throughout, and each replaces something the SQLite
-- file does differently:
--
-- 1. **`VARCHAR` where SQLite has `TEXT`**, whenever the column is indexed or
--    is a key. Lengths are not guesses: ids are 24-character hex, SHA-256
--    hex is 64, and our timestamps are the 20 characters of
--    `2026-11-05T20:45:00Z`. Columns that are only ever stored and read back
--    whole — payloads, notes, errors — stay `TEXT`.
--
-- 2. **A case-insensitive collation instead of `lower()` indexes.** SQLite
--    gets case-insensitive uniqueness from `CREATE UNIQUE INDEX … (lower(x))`.
--    MySQL's `utf8mb4_unicode_ci` compares case-insensitively by default, so
--    a plain unique index on the column gives the identical guarantee — and
--    unlike a functional index it works on 5.7 too. The `WHERE lower(x) = ?`
--    queries in `pro-db` keep working, because a ci comparison of an already
--    lowercased argument matches.
--
-- 3. **`SMALLINT` for the two booleans**, not `TINYINT` or `BOOLEAN`. This one
--    is counter-intuitive and was found the hard way: sqlx's `Any` driver maps
--    only `SMALLINT`, `INT` and `BIGINT` from MySQL, and `BOOLEAN` is an alias
--    for `TINYINT`, which it refuses outright with "Any driver does not support
--    MySql type Tiny". The platform reads both of these columns as `i64` and
--    compares against zero, exactly as it does on SQLite, so nothing above
--    this file has to care.
--
-- The `TEXT` rule has one more trap in it, and it is the reason there is
-- almost no `TEXT` here at all. MySQL sends a `TEXT` column over the wire as
-- `MYSQL_TYPE_BLOB` — only the charset says it is text, and `Any` does not
-- look at the charset. So `Any` produces a blob, `String` decodes only from
-- text, and a `TEXT` column migrates and inserts cleanly and then fails the
-- first time anything *reads* it. Hence `VARCHAR` for every column that is
-- read back, sized to stay inside MySQL's 65,535-byte row limit (`utf8mb4`
-- charges 4 bytes per declared character, so `VARCHAR(14000)` is 56,000 of
-- that budget).
--
-- There is a second limit hiding in the same place, and it is the reason
-- `webhook_events.payload` is 14,000 characters rather than larger: MySQL
-- reports *any* string result wider than about 16,000 characters as a blob,
-- including the output of `CAST(col AS CHAR)`. So a wider column cannot be
-- read back at all, by any spelling. `pro_db::audit::MAX_PAYLOAD_CHARS`
-- truncates to match, on both engines, and explains the trade.
--
-- The rule for adding a column: if anything reads it, it is a `VARCHAR`, and
-- it is narrow enough that every `VARCHAR` in the table adds up to under
-- 65,535 bytes.
--
-- Timestamps stay ISO-8601 UTC *strings* rather than becoming native
-- `DATETIME`. That is deliberate and load-bearing: `Any` has no timestamp
-- type, so a native column would not decode, and text sorts correctly on both
-- engines and stays readable in a dump.
--
-- One last divergence to know about: MySQL does not allow a `DEFAULT` on a
-- `TEXT` column, so the `DEFAULT '{}'` the SQLite file gives `tokens.meta`,
-- `jobs.payload`, `webhook_events.payload` and `audit_events.meta` is absent
-- here. Every insert in `pro-db` supplies all four explicitly, so the default
-- was never reached on either engine — but an insert added later that omits
-- one would pass on SQLite and fail here.

-- ---------------------------------------------------------------------------
-- Platform staff: the operator and whoever they trust. Not customers.
-- ---------------------------------------------------------------------------
CREATE TABLE staff (
    id              VARCHAR(24) NOT NULL PRIMARY KEY,
    email           VARCHAR(255) NOT NULL,
    name            VARCHAR(255) NOT NULL DEFAULT '',
    password_hash   VARCHAR(255) NOT NULL,
    -- 'support' | 'admin' | 'owner'. Only 'owner' may change how the platform
    -- takes money; see pro_core::models::StaffRole.
    role            VARCHAR(32) NOT NULL DEFAULT 'support',
    status          VARCHAR(32) NOT NULL DEFAULT 'active',
    created_at      VARCHAR(32) NOT NULL,
    last_login_at   VARCHAR(32) NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
-- Case-insensitive by collation, which is what the SQLite file spells
-- `lower(email)`. Otherwise Owner@example.com and owner@example.com are two
-- logins.
CREATE UNIQUE INDEX staff_email_unique ON staff (email);

-- ---------------------------------------------------------------------------
-- Accounts: paying customers of the platform.
-- ---------------------------------------------------------------------------
CREATE TABLE accounts (
    id                  VARCHAR(24) NOT NULL PRIMARY KEY,
    email               VARCHAR(255) NOT NULL,
    name                VARCHAR(255) NOT NULL DEFAULT '',
    password_hash       VARCHAR(255) NOT NULL,
    status              VARCHAR(32) NOT NULL DEFAULT 'active',
    email_verified_at   VARCHAR(32) NULL,
    -- Which provider this customer pays through, and their id there. Stored
    -- as a pair because an account created under one provider keeps paying
    -- there after the platform's default moves on.
    billing_provider    VARCHAR(32) NULL,
    billing_customer_id VARCHAR(255) NULL,
    created_at          VARCHAR(32) NOT NULL,
    last_login_at       VARCHAR(32) NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE UNIQUE INDEX accounts_email_unique ON accounts (email);
CREATE INDEX accounts_customer ON accounts (billing_provider, billing_customer_id);

-- ---------------------------------------------------------------------------
-- Sessions, for both populations.
--
-- The cookie value is never stored; only its SHA-256. A dump of this table is
-- then a list of useless hashes rather than a set of live logins.
-- ---------------------------------------------------------------------------
CREATE TABLE sessions (
    id              VARCHAR(64) NOT NULL PRIMARY KEY,
    -- 'staff' | 'account'
    subject_type    VARCHAR(16) NOT NULL,
    subject_id      VARCHAR(24) NOT NULL,
    -- SHA-256, hex: always 64 characters.
    token_hash      VARCHAR(64) NOT NULL,
    expires_at      VARCHAR(32) NOT NULL,
    created_at      VARCHAR(32) NOT NULL,
    last_seen_at    VARCHAR(32) NULL,
    ip              VARCHAR(45) NULL,
    user_agent      VARCHAR(512) NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE UNIQUE INDEX sessions_token ON sessions (token_hash);
CREATE INDEX sessions_subject ON sessions (subject_type, subject_id);
CREATE INDEX sessions_expiry ON sessions (expires_at);

-- ---------------------------------------------------------------------------
-- Single-use tokens: email verification, password reset, staff invites.
--
-- One table rather than three columns spread across accounts and staff,
-- because the rules are identical in all three cases — hashed, dated, used
-- once — and a token that can be replayed is the same bug each time.
-- ---------------------------------------------------------------------------
CREATE TABLE tokens (
    id              VARCHAR(24) NOT NULL PRIMARY KEY,
    -- 'verify_email' | 'reset_password' | 'staff_invite'
    kind            VARCHAR(32) NOT NULL,
    subject_type    VARCHAR(16) NOT NULL,
    subject_id      VARCHAR(24) NOT NULL,
    token_hash      VARCHAR(64) NOT NULL,
    expires_at      VARCHAR(32) NOT NULL,
    used_at         VARCHAR(32) NULL,
    meta            VARCHAR(4000) NOT NULL,
    created_at      VARCHAR(32) NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE UNIQUE INDEX tokens_hash ON tokens (token_hash);
CREATE INDEX tokens_subject ON tokens (subject_type, subject_id, kind);

-- ---------------------------------------------------------------------------
-- Tenants: one Ghost site each.
-- ---------------------------------------------------------------------------
CREATE TABLE tenants (
    id                  VARCHAR(24) NOT NULL PRIMARY KEY,
    slug                VARCHAR(63) NOT NULL,
    name                VARCHAR(255) NOT NULL DEFAULT '',
    account_id          VARCHAR(24) NOT NULL,
    plan_slug           VARCHAR(64) NOT NULL,
    -- provisioning | active | suspended | pending_deletion | deleted | failed
    status              VARCHAR(32) NOT NULL DEFAULT 'provisioning',
    -- The loopback port this tenant's process listens on. Assigned once and
    -- kept for life, so a restart does not move it and a stale proxy entry
    -- cannot point at another tenant.
    port                INT NOT NULL,
    -- The date periodic limits (the email allowance) are counted from.
    period_start        VARCHAR(32) NOT NULL,
    created_at          VARCHAR(32) NOT NULL,
    updated_at          VARCHAR(32) NOT NULL,
    -- Drives hibernation: the edge stamps this, a sweeper stops anything idle.
    last_request_at     VARCHAR(32) NULL,
    suspended_at        VARCHAR(32) NULL,
    suspended_reason    VARCHAR(500) NULL,
    delete_after        VARCHAR(32) NULL,
    -- BIGINT, not INT: a site's content passes 2 GiB long before this
    -- platform would want to stop counting it.
    storage_bytes       BIGINT NOT NULL DEFAULT 0,
    notes               VARCHAR(4000) NULL,
    CONSTRAINT tenants_account_fk FOREIGN KEY (account_id) REFERENCES accounts(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
-- A slug is a hostname label, so uniqueness is case-insensitive — by
-- collation here, by `lower(slug)` on SQLite. Deleted tenants keep their row
-- and therefore their slug, which stops an address being reissued the moment
-- a site is removed. 63 characters is the DNS label limit.
CREATE UNIQUE INDEX tenants_slug_unique ON tenants (slug);
CREATE UNIQUE INDEX tenants_port_unique ON tenants (port);
CREATE INDEX tenants_account ON tenants (account_id);
CREATE INDEX tenants_status ON tenants (status);
-- The hibernation sweeper's query: running tenants ordered by idleness.
CREATE INDEX tenants_idle ON tenants (status, last_request_at);

-- ---------------------------------------------------------------------------
-- Hostnames that route to a tenant.
-- ---------------------------------------------------------------------------
CREATE TABLE domains (
    id                  VARCHAR(24) NOT NULL PRIMARY KEY,
    tenant_id           VARCHAR(24) NOT NULL,
    domain              VARCHAR(253) NOT NULL,
    -- 'subdomain' (ours, instantly live) | 'custom' (theirs, must be proved)
    kind                VARCHAR(16) NOT NULL DEFAULT 'custom',
    -- pending | verified | active | failed
    status              VARCHAR(32) NOT NULL DEFAULT 'pending',
    -- The canonical hostname: the one the tenant's Ghost `url` is set to.
    is_primary          SMALLINT NOT NULL DEFAULT 0,
    verification_token  VARCHAR(255) NOT NULL DEFAULT '',
    verified_at         VARCHAR(32) NULL,
    last_checked_at     VARCHAR(32) NULL,
    last_error          VARCHAR(1000) NULL,
    created_at          VARCHAR(32) NOT NULL,
    CONSTRAINT domains_tenant_fk FOREIGN KEY (tenant_id)
        REFERENCES tenants(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
-- A hostname may belong to exactly one tenant across the whole platform.
-- Without this, one customer could attach a domain another is already serving
-- and silently take over its traffic. 253 is the maximum length of a DNS name.
CREATE UNIQUE INDEX domains_unique ON domains (domain);
CREATE INDEX domains_tenant ON domains (tenant_id);
CREATE INDEX domains_status ON domains (status);

-- ---------------------------------------------------------------------------
-- Platform subscriptions: what a customer pays *us*.
--
-- Not the member subscriptions inside a tenant's Ghost — those are that
-- site's own business and live in its own database.
-- ---------------------------------------------------------------------------
CREATE TABLE subscriptions (
    id                          VARCHAR(24) NOT NULL PRIMARY KEY,
    account_id                  VARCHAR(24) NOT NULL,
    -- Per-site, as Ghost Pro's are, so one account can hold several on
    -- different plans. Null while a checkout is in flight and the site does
    -- not exist yet.
    tenant_id                   VARCHAR(24) NULL,
    plan_slug                   VARCHAR(64) NOT NULL,
    -- stripe | paddle | lemonsqueezy | manual
    provider                    VARCHAR(32) NOT NULL,
    provider_subscription_id    VARCHAR(255) NULL,
    provider_customer_id        VARCHAR(255) NULL,
    -- Normalised across providers, which all name these differently:
    -- trialing | active | past_due | paused | canceled | expired
    status                      VARCHAR(32) NOT NULL,
    current_period_start        VARCHAR(32) NULL,
    current_period_end          VARCHAR(32) NULL,
    trial_ends_at               VARCHAR(32) NULL,
    cancel_at_period_end        SMALLINT NOT NULL DEFAULT 0,
    canceled_at                 VARCHAR(32) NULL,
    -- When dunning started, so the grace period is measured from it.
    past_due_since              VARCHAR(32) NULL,
    created_at                  VARCHAR(32) NOT NULL,
    updated_at                  VARCHAR(32) NOT NULL,
    CONSTRAINT subscriptions_account_fk FOREIGN KEY (account_id)
        REFERENCES accounts(id),
    CONSTRAINT subscriptions_tenant_fk FOREIGN KEY (tenant_id)
        REFERENCES tenants(id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
-- A provider's subscription id is the key webhooks arrive with, and it must
-- map to one row. Scoped by provider because two providers can mint the same
-- numeric id.
--
-- The SQLite file writes this as a partial index, `WHERE
-- provider_subscription_id IS NOT NULL`, and MySQL has no partial indexes.
-- The plain index is equivalent here rather than merely close: both engines
-- treat NULLs in a unique index as distinct from each other, so rows with no
-- provider id — a checkout still in flight — do not collide under either.
CREATE UNIQUE INDEX subscriptions_provider_id
    ON subscriptions (provider, provider_subscription_id);
CREATE INDEX subscriptions_account ON subscriptions (account_id);
CREATE INDEX subscriptions_tenant ON subscriptions (tenant_id);
CREATE INDEX subscriptions_status ON subscriptions (status);

-- ---------------------------------------------------------------------------
-- Operator-editable settings, which beat the config file.
--
-- Config is the seed; this table is the source of truth. If the file won
-- instead, an operator would switch payment provider in the console, see it
-- save, and watch it have no effect.
-- ---------------------------------------------------------------------------
CREATE TABLE platform_settings (
    -- `key` is a reserved word in MySQL and must be quoted, here and in every
    -- query that names it; see pro_db::dialect::quote.
    `key`       VARCHAR(191) NOT NULL PRIMARY KEY,
    -- JSON, so a setting can be a string, a number or a boolean without this
    -- table needing to know which. Held as TEXT rather than MySQL's native
    -- JSON type because sqlx's `Any` driver has no JSON type and the platform
    -- parses it in Rust either way.
    value       VARCHAR(14000) NOT NULL,
    updated_at  VARCHAR(32) NOT NULL,
    updated_by  VARCHAR(64) NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

-- ---------------------------------------------------------------------------
-- Webhook idempotency.
--
-- Every provider retries, and all three may deliver the same event twice.
-- Recording the id before acting is what stops a retry granting a second
-- subscription or cancelling a site twice.
-- ---------------------------------------------------------------------------
CREATE TABLE webhook_events (
    id              VARCHAR(24) NOT NULL PRIMARY KEY,
    provider        VARCHAR(32) NOT NULL,
    -- The provider's own event id.
    event_id        VARCHAR(255) NOT NULL,
    event_type      VARCHAR(128) NOT NULL,
    received_at     VARCHAR(32) NOT NULL,
    processed_at    VARCHAR(32) NULL,
    -- Kept so a failed event can be read and replayed by hand.
    payload         VARCHAR(14000) NOT NULL,
    error           VARCHAR(1000) NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE UNIQUE INDEX webhook_events_unique ON webhook_events (provider, event_id);
CREATE INDEX webhook_events_received ON webhook_events (received_at);

-- ---------------------------------------------------------------------------
-- Background work: provisioning, DNS checks, backups, deletions.
--
-- A table rather than an in-memory queue because provisioning a site takes
-- seconds and must survive a restart halfway through. A job that was running
-- when the process died is visible as such and can be retried.
-- ---------------------------------------------------------------------------
CREATE TABLE jobs (
    id              VARCHAR(24) NOT NULL PRIMARY KEY,
    -- provision_tenant | verify_domain | issue_cert | backup_tenant |
    -- delete_tenant | measure_storage
    kind            VARCHAR(64) NOT NULL,
    tenant_id       VARCHAR(24) NULL,
    payload         VARCHAR(4000) NOT NULL,
    -- pending | running | done | failed
    status          VARCHAR(32) NOT NULL DEFAULT 'pending',
    attempts        INT NOT NULL DEFAULT 0,
    -- Not before this time, which is how a retry backs off and how a nightly
    -- job is scheduled.
    run_after       VARCHAR(32) NOT NULL,
    last_error      VARCHAR(2000) NULL,
    created_at      VARCHAR(32) NOT NULL,
    started_at      VARCHAR(32) NULL,
    finished_at     VARCHAR(32) NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX jobs_claim ON jobs (status, run_after);
CREATE INDEX jobs_tenant ON jobs (tenant_id);

-- ---------------------------------------------------------------------------
-- Tenant snapshots. One tenant is one directory, so a backup is one archive.
-- ---------------------------------------------------------------------------
CREATE TABLE backups (
    id          VARCHAR(24) NOT NULL PRIMARY KEY,
    tenant_id   VARCHAR(24) NOT NULL,
    path        VARCHAR(1024) NOT NULL,
    size_bytes  BIGINT NOT NULL DEFAULT 0,
    -- 'auto' | 'manual' | 'pre_delete'
    reason      VARCHAR(32) NOT NULL DEFAULT 'auto',
    created_at  VARCHAR(32) NOT NULL,
    CONSTRAINT backups_tenant_fk FOREIGN KEY (tenant_id)
        REFERENCES tenants(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX backups_tenant ON backups (tenant_id, created_at);

-- ---------------------------------------------------------------------------
-- Audit trail.
--
-- Written for anything that changes money, routing or a tenant's existence.
-- Customers trust this platform with their publication, so "who suspended
-- this site, and when" has to be answerable later.
-- ---------------------------------------------------------------------------
CREATE TABLE audit_events (
    id          VARCHAR(24) NOT NULL PRIMARY KEY,
    -- 'staff:<id>' | 'account:<id>' | 'system' | 'webhook:<provider>'
    actor       VARCHAR(64) NOT NULL,
    action      VARCHAR(64) NOT NULL,
    tenant_id   VARCHAR(24) NULL,
    account_id  VARCHAR(24) NULL,
    meta        VARCHAR(4000) NOT NULL,
    created_at  VARCHAR(32) NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX audit_created ON audit_events (created_at);
CREATE INDEX audit_tenant ON audit_events (tenant_id, created_at);
CREATE INDEX audit_actor ON audit_events (actor, created_at);

-- ---------------------------------------------------------------------------
-- Daily usage, for the dashboard's charts and for enforcing the storage
-- quota Ghost knows nothing about.
--
-- Sampled rather than computed on demand: counting members and megabytes
-- across every tenant means opening every tenant database, which is far too
-- expensive to do when a page loads.
-- ---------------------------------------------------------------------------
CREATE TABLE usage_samples (
    id              VARCHAR(24) NOT NULL PRIMARY KEY,
    tenant_id       VARCHAR(24) NOT NULL,
    sampled_at      VARCHAR(32) NOT NULL,
    members         INT NOT NULL DEFAULT 0,
    posts           INT NOT NULL DEFAULT 0,
    storage_bytes   BIGINT NOT NULL DEFAULT 0,
    emails_sent     INT NOT NULL DEFAULT 0,
    CONSTRAINT usage_tenant_fk FOREIGN KEY (tenant_id)
        REFERENCES tenants(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX usage_tenant ON usage_samples (tenant_id, sampled_at);
