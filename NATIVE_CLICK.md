# Optional native Vuuut Click

Register an operator platform at your own Click server:
`click-server native-platform vuuut-pro sites.example.com`, supplying a random
`NATIVE_PLATFORM_TOKEN` privately in the environment. The suffix delegates only
its exact domain/subdomains; custom domains need independent Click TXT proof.

Add a private `click` block to `pro.json` (or matching `PRO_CLICK__*` variables):

```json
{
  "click": {
    "baseUrl": "https://click.example.com",
    "platformKey": "<operator-issued-private-key>",
    "defaultPlan": "free",
    "planMap": {"creator":"pro","team":"pro","business":"pro"},
    "consentRequired": true,
    "engagement": false,
    "persistentIdentity": false
  }
}
```

Restart to reconcile existing tenants. The background worker is independent of
provisioning/backups, stores exact snapshots and monotonic revisions, retries
uncertain responses and records errors/dead state. Site detail pages expose
state/revisions, TXT instructions, retry, rotation, disconnect and reconnect.
The tenant's native report link opens `/ghost/analytics/click/` in its own Admin.
No Click configuration means no remote analytics dependency.

SQLite/MySQL migrations 0004–0005 capture tenant/domain changes transactionally.
Active sites provision once; plan changes map to Click entitlements; suspension
and pending deletion pause analytics; final deletion durably erases its separate
analytics workspace. A restore after actual erasure increments an incarnation
and creates a fresh mapping, retaining the old terminal tombstone. File/restart
failures retain retryable work even after remote acceptance.

Only website-scoped credentials enter private tenant `hostSettings.click`.
Platform credentials never enter the tenant, its browser, public APIs or
report HTML. Downloadable backups scrub native credentials and managed mail, retaining a
disabled hosting marker so restored stale standalone CMS settings cannot
reactivate analytics. Hosted policy overrides any old standalone connection,
including while suspended or disconnected.
The CMS build must include migrations 0038–0040 and native staff/tracker support.
Click must include PostgreSQL migrations 0010–0011 and ClickHouse 0003. After platform-token or plan-map
changes, restart the worker to apply new revisions. Disabling a platform at
Click blocks collection and native access without erasing data.

MySQL's initial tables explicitly use `utf8mb4_unicode_ci`; before migration the
runner aligns the database's *future-table default* with that collation so the
unchanged older 0003 foreign keys work on MySQL 8+. Existing columns and migration
checksums are untouched. The migration account needs ALTER DATABASE if the
default differs; provisioning the DB with that default avoids that prerequisite.

The full contract and optional real three-repository/browser integration suite
live in the adjacent Vuuut-click repository (`docs/GHOST_INTEGRATION.md` and
`tests/native-bridge`). Local tests do not establish production readiness;
public TLS/DNS, provider/SMTP validation, independent SSO review and operational
release gates remain separate.


Optional `engagement` enables consent-aware visible-time/scroll events, which
consume the normal allowance; it defaults false. `persistentIdentity` also
defaults false and is rejected unless `consentRequired` is true. These options
enter only the tenant-scoped settings; public pages receive the public flags.
Restart hosting after changing them to reconcile and restart affected tenants.
The updated CMS provides native retention, content, newsletter, campaign-cost
and performance views. Deploy Click first, then the CMS binary and hosting.
Historical identity/engagement/edition data is not reconstructed.

The Click owner console now provides **Connections → Vuuut Pro automatic
analytics**, including private credential generation and a copyable `pro.json`
block. This is one operator setup for the platform; tenants never connect
individually. The CMS completes registration in its background worker and uses
Click directly on its normal Analytics and post analytics pages. Keep installed
CMS executables outside build-cache directories; idle tenants need their runtime
binary again whenever the proxy wakes them.
