-- The control plane's own database.
--
-- This holds what the *platform* knows: that a site exists, who pays for it,
-- what plan it is on, which hostnames route to it. It deliberately holds
-- nothing a tenant publishes — no posts, no members, no tenant staff. Each
-- tenant's Ghost keeps all of that in its own SQLite file, and the only thing
-- that crosses the boundary is the `hostSettings` block this platform writes
-- into that tenant's config.
--
-- Timestamps are ISO-8601 UTC strings throughout, which sort correctly as
-- text and stay readable in a dump.

-- ---------------------------------------------------------------------------
-- Platform staff: the operator and whoever they trust. Not customers.
-- ---------------------------------------------------------------------------
CREATE TABLE staff (
    id              TEXT PRIMARY KEY,
    email           TEXT NOT NULL,
    name            TEXT NOT NULL DEFAULT '',
    password_hash   TEXT NOT NULL,
    -- 'support' | 'admin' | 'owner'. Only 'owner' may change how the platform
    -- takes money; see pro_core::models::StaffRole.
    role            TEXT NOT NULL DEFAULT 'support',
    status          TEXT NOT NULL DEFAULT 'active',
    created_at      TEXT NOT NULL,
    last_login_at   TEXT
);
-- Addresses are compared case-insensitively, so uniqueness has to be too —
-- otherwise Owner@example.com and owner@example.com are two logins.
CREATE UNIQUE INDEX staff_email_unique ON staff (lower(email));

-- ---------------------------------------------------------------------------
-- Accounts: paying customers of the platform.
-- ---------------------------------------------------------------------------
CREATE TABLE accounts (
    id                  TEXT PRIMARY KEY,
    email               TEXT NOT NULL,
    name                TEXT NOT NULL DEFAULT '',
    password_hash       TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'active',
    email_verified_at   TEXT,
    -- Which provider this customer pays through, and their id there. Stored
    -- as a pair because an account created under one provider keeps paying
    -- there after the platform's default moves on.
    billing_provider    TEXT,
    billing_customer_id TEXT,
    created_at          TEXT NOT NULL,
    last_login_at       TEXT
);
CREATE UNIQUE INDEX accounts_email_unique ON accounts (lower(email));
CREATE INDEX accounts_customer ON accounts (billing_provider, billing_customer_id);

-- ---------------------------------------------------------------------------
-- Sessions, for both populations.
--
-- The cookie value is never stored; only its SHA-256. A dump of this table is
-- then a list of useless hashes rather than a set of live logins.
-- ---------------------------------------------------------------------------
CREATE TABLE sessions (
    id              TEXT PRIMARY KEY,
    -- 'staff' | 'account'
    subject_type    TEXT NOT NULL,
    subject_id      TEXT NOT NULL,
    token_hash      TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    last_seen_at    TEXT,
    ip              TEXT,
    user_agent      TEXT
);
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
    id              TEXT PRIMARY KEY,
    -- 'verify_email' | 'reset_password' | 'staff_invite'
    kind            TEXT NOT NULL,
    subject_type    TEXT NOT NULL,
    subject_id      TEXT NOT NULL,
    token_hash      TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    used_at         TEXT,
    meta            TEXT NOT NULL DEFAULT '{}',
    created_at      TEXT NOT NULL
);
CREATE UNIQUE INDEX tokens_hash ON tokens (token_hash);
CREATE INDEX tokens_subject ON tokens (subject_type, subject_id, kind);

-- ---------------------------------------------------------------------------
-- Tenants: one Ghost site each.
-- ---------------------------------------------------------------------------
CREATE TABLE tenants (
    id                  TEXT PRIMARY KEY,
    slug                TEXT NOT NULL,
    name                TEXT NOT NULL DEFAULT '',
    account_id          TEXT NOT NULL REFERENCES accounts(id),
    plan_slug           TEXT NOT NULL,
    -- provisioning | active | suspended | pending_deletion | deleted | failed
    status              TEXT NOT NULL DEFAULT 'provisioning',
    -- The loopback port this tenant's process listens on. Assigned once and
    -- kept for life, so a restart does not move it and a stale proxy entry
    -- cannot point at another tenant.
    port                INTEGER NOT NULL,
    -- The date periodic limits (the email allowance) are counted from.
    period_start        TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    -- Drives hibernation: the edge stamps this, a sweeper stops anything idle.
    last_request_at     TEXT,
    suspended_at        TEXT,
    suspended_reason    TEXT,
    delete_after        TEXT,
    storage_bytes       INTEGER NOT NULL DEFAULT 0,
    notes               TEXT
);
-- A slug is a hostname label, so uniqueness is case-insensitive. Deleted
-- tenants keep their row and therefore their slug, which stops an address
-- being reissued the moment a site is removed.
CREATE UNIQUE INDEX tenants_slug_unique ON tenants (lower(slug));
CREATE UNIQUE INDEX tenants_port_unique ON tenants (port);
CREATE INDEX tenants_account ON tenants (account_id);
CREATE INDEX tenants_status ON tenants (status);
-- The hibernation sweeper's query: running tenants ordered by idleness.
CREATE INDEX tenants_idle ON tenants (status, last_request_at);

-- ---------------------------------------------------------------------------
-- Hostnames that route to a tenant.
-- ---------------------------------------------------------------------------
CREATE TABLE domains (
    id                  TEXT PRIMARY KEY,
    tenant_id           TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    domain              TEXT NOT NULL,
    -- 'subdomain' (ours, instantly live) | 'custom' (theirs, must be proved)
    kind                TEXT NOT NULL DEFAULT 'custom',
    -- pending | verified | active | failed
    status              TEXT NOT NULL DEFAULT 'pending',
    -- The canonical hostname: the one the tenant's Ghost `url` is set to.
    is_primary          INTEGER NOT NULL DEFAULT 0,
    verification_token  TEXT NOT NULL DEFAULT '',
    verified_at         TEXT,
    last_checked_at     TEXT,
    last_error          TEXT,
    created_at          TEXT NOT NULL
);
-- A hostname may belong to exactly one tenant across the whole platform.
-- Without this, one customer could attach a domain another is already serving
-- and silently take over its traffic.
CREATE UNIQUE INDEX domains_unique ON domains (lower(domain));
CREATE INDEX domains_tenant ON domains (tenant_id);
CREATE INDEX domains_status ON domains (status);

-- ---------------------------------------------------------------------------
-- Platform subscriptions: what a customer pays *us*.
--
-- Not the member subscriptions inside a tenant's Ghost — those are that
-- site's own business and live in its own database.
-- ---------------------------------------------------------------------------
CREATE TABLE subscriptions (
    id                          TEXT PRIMARY KEY,
    account_id                  TEXT NOT NULL REFERENCES accounts(id),
    -- Per-site, as Ghost Pro's are, so one account can hold several on
    -- different plans. Null while a checkout is in flight and the site does
    -- not exist yet.
    tenant_id                   TEXT REFERENCES tenants(id) ON DELETE SET NULL,
    plan_slug                   TEXT NOT NULL,
    -- stripe | paddle | lemonsqueezy | manual
    provider                    TEXT NOT NULL,
    provider_subscription_id    TEXT,
    provider_customer_id        TEXT,
    -- Normalised across providers, which all name these differently:
    -- trialing | active | past_due | paused | canceled | expired
    status                      TEXT NOT NULL,
    current_period_start        TEXT,
    current_period_end          TEXT,
    trial_ends_at               TEXT,
    cancel_at_period_end        INTEGER NOT NULL DEFAULT 0,
    canceled_at                 TEXT,
    -- When dunning started, so the grace period is measured from it.
    past_due_since              TEXT,
    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL
);
-- A provider's subscription id is the key webhooks arrive with, and it must
-- map to one row. Scoped by provider because two providers can mint the same
-- numeric id.
CREATE UNIQUE INDEX subscriptions_provider_id
    ON subscriptions (provider, provider_subscription_id)
    WHERE provider_subscription_id IS NOT NULL;
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
    key         TEXT PRIMARY KEY,
    -- JSON, so a setting can be a string, a number or a boolean without this
    -- table needing to know which.
    value       TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    updated_by  TEXT
);

-- ---------------------------------------------------------------------------
-- Webhook idempotency.
--
-- Every provider retries, and all three may deliver the same event twice.
-- Recording the id before acting is what stops a retry granting a second
-- subscription or cancelling a site twice.
-- ---------------------------------------------------------------------------
CREATE TABLE webhook_events (
    id              TEXT PRIMARY KEY,
    provider        TEXT NOT NULL,
    -- The provider's own event id.
    event_id        TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    received_at     TEXT NOT NULL,
    processed_at    TEXT,
    -- Kept so a failed event can be read and replayed by hand.
    payload         TEXT NOT NULL DEFAULT '{}',
    error           TEXT
);
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
    id              TEXT PRIMARY KEY,
    -- provision_tenant | verify_domain | issue_cert | backup_tenant |
    -- delete_tenant | measure_storage
    kind            TEXT NOT NULL,
    tenant_id       TEXT,
    payload         TEXT NOT NULL DEFAULT '{}',
    -- pending | running | done | failed
    status          TEXT NOT NULL DEFAULT 'pending',
    attempts        INTEGER NOT NULL DEFAULT 0,
    -- Not before this time, which is how a retry backs off and how a nightly
    -- job is scheduled.
    run_after       TEXT NOT NULL,
    last_error      TEXT,
    created_at      TEXT NOT NULL,
    started_at      TEXT,
    finished_at     TEXT
);
CREATE INDEX jobs_claim ON jobs (status, run_after);
CREATE INDEX jobs_tenant ON jobs (tenant_id);

-- ---------------------------------------------------------------------------
-- Tenant snapshots. One tenant is one directory, so a backup is one archive.
-- ---------------------------------------------------------------------------
CREATE TABLE backups (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    path        TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL DEFAULT 0,
    -- 'auto' | 'manual' | 'pre_delete'
    reason      TEXT NOT NULL DEFAULT 'auto',
    created_at  TEXT NOT NULL
);
CREATE INDEX backups_tenant ON backups (tenant_id, created_at);

-- ---------------------------------------------------------------------------
-- Audit trail.
--
-- Written for anything that changes money, routing or a tenant's existence.
-- Customers trust this platform with their publication, so "who suspended
-- this site, and when" has to be answerable later.
-- ---------------------------------------------------------------------------
CREATE TABLE audit_events (
    id          TEXT PRIMARY KEY,
    -- 'staff:<id>' | 'account:<id>' | 'system' | 'webhook:<provider>'
    actor       TEXT NOT NULL,
    action      TEXT NOT NULL,
    tenant_id   TEXT,
    account_id  TEXT,
    meta        TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL
);
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
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    sampled_at      TEXT NOT NULL,
    members         INTEGER NOT NULL DEFAULT 0,
    posts           INTEGER NOT NULL DEFAULT 0,
    storage_bytes   INTEGER NOT NULL DEFAULT 0,
    emails_sent     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX usage_tenant ON usage_samples (tenant_id, sampled_at);
