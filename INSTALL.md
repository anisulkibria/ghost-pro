# Installing vuuut-pro on another machine

vuuut-pro is the managed-hosting control plane: it sells plans, provisions
sites, and runs each customer's site as a child process behind one edge proxy.
This guide takes you from a `git clone` to a platform you can sign into.

If you have cloned the repository and `./run.sh start` did nothing useful,
[Step 3](#step-3--start-the-platform) is the part you want.

---

## Why there are two repositories

vuuut-pro does not contain a CMS. It *runs* one — Vuuut — once per
customer site, and it needs two things from that checkout:

| What | Where it comes from | Used for |
|---|---|---|
| The `ghost` binary | `vuuut/target/release/ghost` | Every tenant site is one of these processes |
| The starting themes | `vuuut/content/themes` | Copied into each new site when it is created |

So **install Vuuut first**. A vuuut-pro with no Vuuut beside it will
start, serve its dashboard, and then fail to provision the first site.

---

## What is not in the repository

Two large directories are deliberately not committed, which is why a fresh
clone is small and why it needs one setup step:

| Directory | Size here | What it is | How you get it back |
|---|---|---|---|
| `target/` | ~10 GB | Rust build output | `cargo build --release` |
| `run/` | grows with your sites | Deployment directory: the control-plane database, one directory per customer site, certificates, backups, logs, and your local `pro.json` | `./run.sh init`, or automatically on `./run.sh start` |

Nothing in either is authored by hand, so nothing is lost by leaving them out.
This mirrors Vuuut, which commits its themes and `routes.yaml` but not
`content/data`, `content/images` or `target/`.

> **A clone is an empty platform, not a copy of this one.** It starts with no
> customers, no sites and no operator login. To move an existing platform with
> its sites intact, see [Moving an existing platform](#moving-an-existing-platform).

---

## Before you start

Rust, and a C linker to produce a binary:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On macOS, `xcode-select --install`; on Debian or Ubuntu,
`sudo apt install build-essential`. Nothing else — no Node, no database
server, no system OpenSSL.

---

## Step 1 — Clone both repositories side by side

The directory names and the layout both matter: side-by-side is the layout
vuuut-pro looks for, so this is the version that needs no configuration.

```bash
mkdir -p ~/Ghost && cd ~/Ghost
git clone https://github.com/anisulkibria/rust-ghost.git vuuut
git clone https://github.com/anisulkibria/ghost-pro.git  vuuut-pro
```

You should end up with:

```
~/Ghost
├── vuuut           the CMS — one process per customer site
└── vuuut-pro       the platform — this repository
```

Any of `vuuut`, `ghost-rust` or `rust-ghost` is recognised, in either case,
as a sibling of `vuuut-pro` or up to three levels further out. Somewhere else
entirely is fine too — [Step 3](#step-3--start-the-platform) says how to point
at it.

You are not locked in by that choice. `pro.json` records where the checkout
was when `setup` ran, and the platform treats that record as a hint: if the
path no longer resolves — the directory was renamed, the pair was cloned onto
another machine under different names, the deployment directory was copied to
a new host — it searches again and runs from what it finds, saying so once in
the log and in `doctor`. Nothing has to be re-run by hand after a move.

## Step 2 — Build both

```bash
cd ~/Ghost/vuuut && cargo build --release
cd ~/Ghost/vuuut-pro  && cargo build --release
```

The first build of each takes a few minutes and is most of the time this
guide costs. Confirm both binaries exist before continuing:

```bash
ls -l ~/Ghost/vuuut/target/release/ghost ~/Ghost/vuuut-pro/target/release/ghost-pro
```

`./run.sh start` also builds a missing CMS binary when `paths.ghostBinary`
points to `target/release/ghost` inside a local Vuuut checkout. A missing custom
binary path is reported before the platform is launched. Startup failures show
only output from the current launch; older errors remain in `run/server.log`.

Stop the platform with `./run.sh stop` before moving or renaming its directory,
then start it from the new location. A running SQLite connection pool retains
its old absolute database path, so moving its directory while it is running can
produce `unable to open database file` until it is restarted.

## Step 3 — Start the platform

```bash
cd ~/Ghost/vuuut-pro
./run.sh start
```

On a fresh clone there is no `run/`, so the script creates it and writes a
local-development `pro.json` before starting — printing what it found:

```
no pro.json in /Users/you/Ghost/vuuut-pro/run — writing one for local development…

Wrote /Users/you/Ghost/vuuut-pro/run/pro.json

Found a Vuuut checkout at ../../vuuut (relative to this directory,
so this config stays valid on other machines).

started (pid 12345)
  dashboard  http://pro.localhost:8080/dashboard
  console    http://pro.localhost:8080/console
  log        /Users/you/Ghost/vuuut-pro/run/server.log
```

Run `./run.sh init` instead if you would rather read the config over before
anything starts with it. Both are safe to run twice: an existing `pro.json` is
never overwritten.

> **"No Vuuut checkout found nearby"** means the two repositories are not
> in a layout it recognises. The platform will still start; edit the two paths
> in `run/pro.json` to point at your checkout, then `./run.sh restart`. See
> [The config file](#the-config-file).

## Step 4 — Create an operator login

There is no self-service way in — an open "become the owner" page on a fresh
install is a race anybody on the internet can win — so the first login is made
on the command line:

```bash
./run.sh staff --email you@example.com --name "Your Name"
```

Sign in at **http://pro.localhost:8080/console**.

`pro.localhost` and `*.localhost` resolve to 127.0.0.1 on macOS and on most
Linux distributions with no `/etc/hosts` entry. If yours does not, add one:

```
127.0.0.1 pro.localhost
```

## Step 5 — Check it over

```bash
./run.sh doctor
```

This is the platform's own preflight — config, CMS binary, themes, database,
ports, mail, billing — run against the deployment directory as it stands:

```
  ✓ config             loaded for development mode
  ✓ ghost binary       found
  ✓ themes             2 available in .../vuuut/content/themes
  ✓ data directory     writable
  ✓ database           open and migrated
  ✓ operator login     1 configured
  ✓ billing            no provider — sites work, nothing is charged
  ✗ mail               mail.transport is "log", so messages are written to the
                       platform log instead of being sent. Customers receive no
                       email — set this in the console.
  ✓ edge port          0.0.0.0:8080 is free
```

On a local install, `mail` is the only expected complaint: with no SMTP server
configured, mail is written to the log instead of sent — including the
confirmation link a new account needs, so a signup can still be walked end to
end on a machine with no mail server. Billing reporting "no provider" is also
fine — sites work, nothing is charged. Set both in the console when you need
them.

Mail counts as configured only when **Transport** is SMTP and both **SMTP
host** and **From address** are filled in. A half-filled SMTP setup is
reported here by the field it is missing, rather than passing this check and
then failing on every send.

Two checks are worth stopping for:

- **`✗ ghost binary`** — Vuuut is not built, or the path is wrong.
  Re-run [Step 2](#step-2--build-both), or fix `paths.ghostBinary`.
- **`✗ themes`** — `paths.themeSource` points at a directory that is not
  there. New sites would be created with no theme installed.

## Step 6 — Create a site

In the console, or through the customer-facing signup at
`http://pro.localhost:8080`. The first site takes a moment: the platform
creates its directory, copies the themes in, starts a `ghost` process on a
loopback port and proxies to it. Watch it happen with:

```bash
./run.sh log        # the platform's own log
./run.sh tenants    # the sites currently running
```

A site at slug `acme` is then served at **http://acme.localhost:8080**.

---

## The config file

`run/pro.json`, read from the deployment directory. It is not in the
repository — it is per-machine, and on a real platform it holds credentials.
`./run.sh init` writes this:

```json
{
  "url": "http://pro.localhost:8080",
  "subdomainBase": "localhost",
  "paths": {
    "dataDir": "data",
    "ghostBinary": "../../vuuut/target/release/ghost",
    "themeSource": "../../vuuut/content/themes"
  },
  "ssl": { "enabled": false },
  "billing": { "provider": "none" },
  "mail": { "transport": "log" }
}
```

The paths are **relative to the deployment directory** (`run/`), and that is
the point: this config is valid on every machine where the two checkouts sit
side by side, where an absolute path is valid on exactly one. Absolute paths
still work if you prefer them, or if your checkout is somewhere unrelated:

```json
"ghostBinary": "/opt/ghost-rust/target/release/ghost",
"themeSource": "/opt/ghost-rust/content/themes"
```

`dataDir` is resolved the same way, so the default `"data"` means
`run/data`. Point it at another disk when sites outgrow this one.

Every value can also be set from the environment, with `__` to nest —
`PRO_SERVER__PORT=9000`, `PRO_BILLING__PROVIDER=stripe`. The environment wins
over the file, which is what makes one checkout serve both a development and a
production deployment.

### Changing the defaults `init` writes

```bash
PRO_URL=http://pro.test:8080 PRO_SUBDOMAIN_BASE=test ./run.sh init
```

### Two deployments on one machine

`PRO_DIR` is the deployment directory, and everything — config, database,
sites, pidfile, log — lives inside it, so two are independent:

```bash
PRO_DIR=$PWD/run-staging PRO_SERVER__PORT=8099 ./run.sh start
```

---

## Moving an existing platform

A clone gives you an empty platform. To move one *with its customers and
sites*, copy the deployment directory — one directory per tenant is the
storage design, so a move is one copy:

1. Stop the platform on the old machine, so every site closes its database
   cleanly. A copy taken while they are running can catch a half-written one.

   ```bash
   ./run.sh stop
   ```

2. Copy `run/data` across:

   ```bash
   rsync -a ~/Ghost/vuuut-pro/run/data/ newmachine:~/Ghost/vuuut-pro/run/data/
   ```

3. On the new machine, follow Steps 1–3 above. `init` writes a fresh
   `pro.json` and leaves the copied `data/` alone. Then set `url` and
   `subdomainBase` to whatever the new machine answers on — the sites' own
   addresses are derived from them.

4. `./run.sh doctor`, then `./run.sh start`.

Your operator login came across with the database, so Step 4 is not needed
again.

---

## Production

The local install above is plain HTTP on a hostname that needs no DNS. A real
platform differs in four ways:

```bash
cd ~/Ghost/vuuut-pro
mkdir -p run
(cd run && ../target/release/ghost-pro setup --force \
    --url https://pro.example.com \
    --subdomain-base sites.example.com \
    --ghost-binary ../../vuuut/target/release/ghost \
    --theme-source ../../vuuut/content/themes \
    --ssl --email you@example.com)
```

`--force` overwrites an existing `pro.json`; run interactively it asks before
it does. Leave off any flag and it prompts for that value instead.

1. **DNS.** `pro.example.com` for the platform, and a wildcard
   `*.sites.example.com` for customer sites — every new site has to be
   reachable without a DNS change. Keep the two zones separate so no customer
   slug can collide with a platform route.
2. **TLS.** `--ssl` gets certificates from Let's Encrypt for the platform and
   for every site, including customer domains. Add `--staging` while testing:
   Let's Encrypt rate-limits failures on the real CA. `ssl.email` is where
   renewal warnings go, so set it.
3. **Port 443.** Binding below 1024 needs privileges:
   `sudo setcap 'cap_net_bind_service=+ep' target/release/ghost-pro`.
   Open 80 and 443 on the firewall — 80 is where ACME's HTTP-01 challenge is
   answered, so certificates do not issue without it.
4. **`publicAddresses`.** The IPs customers point an `A`/`AAAA` record at when
   they attach their own domain, and what the platform's DNS check compares
   against. It cannot reliably detect these itself behind NAT or a load
   balancer, so set them.

Then `./run.sh doctor` — it checks each of these and names what is missing —
followed by `./run.sh staff` for the first login, and the console's Plans and
Mail screens, which `doctor` will keep complaining about until they are set.

On the Mail screen, **From address** is as necessary as the host and the
credentials, and some providers accept only one specific value for it: Gmail,
for instance, rejects any From address other than the account that
authenticated. If a send fails, the reason is in the platform log — the
platform never reports an email as sent when it was not.

Run it under a service manager rather than `run.sh` on a real server, so it
comes back after a reboot. `SIGTERM` is the correct stop signal: the platform
stops each site in turn so every site closes its SQLite database properly,
which `SIGKILL` skips.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `no deployment directory at .../run` | An older `run.sh` that would not bootstrap | `git pull`, then `./run.sh start` |
| `paths.ghostBinary points at … which does not exist` | Vuuut not built, or wrong path | `cargo build --release` in Vuuut; check `paths.ghostBinary` |
| `could not find the "ghost" binary on PATH` | No checkout was found at setup, so the config has a bare name | Set `paths.ghostBinary` to your `target/release/ghost` |
| `paths.themeSource … does not exist; skipping` | Wrong path; sites are created with no theme | Point it at `vuuut/content/themes`, then recreate the site |
| `port 8080 is held by something that is not this platform` | Another process, often a second deployment | `PRO_SERVER__PORT=8099 ./run.sh start` |
| Started, but nothing on the port | Startup failed after the fork | `./run.sh log` — a bad config or a failed migration says so there |
| A site 502s or hangs on first load | The site's process is starting, or failed to | `./run.sh tenants`, then `./run.sh log` |
| Sites still running after the platform stopped | Forced stop or a crash; they hold their databases open | `./run.sh reap` |

`./run.sh` with no arguments lists every command.


## Managed email with Amazon SES or Mailgun

Open **Console → Settings → Amazon SES**, enter the AWS region and SES
SMTP username/password, and save. For credentials downloaded through Mail Manager,
also paste the CSV's **SMTP Endpoint** into **SES SMTP endpoint**. Use the plain
**Username** and **Password** columns; SMTP handles Base64 encoding itself.
The endpoint is optional for standard regional SES credentials. In **Settings → Email**, set **From address** to an identity verified in
that region. Choose **Amazon SES (SMTP)** as the platform transport to use SES
for account verification, password resets, and other platform messages.

For tenant newsletters, choose **Platform Amazon SES** or **Platform Mailgun**
under **Settings → Email → Tenant email provider**. Mailgun requires a sending API key,
sending domain, region, and a verified From address. Its webhook signing key is
optional for sending and required for authenticated Mailgun event ingestion.
The platform's transactional transport remains separately selectable when
Mailgun handles newsletters.

Save the provider credentials first, then select tenant delivery on the Email
page. An incomplete managed provider is rejected before settings are saved. A durable job applies the latest configuration to every
existing site and restarts running tenant processes; sleeping sites read it on
their next start. New sites inherit the same policy. Individual site failures
retry independently through the jobs queue. Saving **Tenant configures their
own** removes the policy and restores each site's previous mail settings.

When managed delivery is active, both SES and Mailgun configuration are locked
in tenant screens and through the tenant Admin API. Platform credentials stay
out of the tenant settings database and downloadable backups. Backups retain
plan settings, and restoring a backup applies the current platform credentials.
Plan changes and suspensions preserve the managed email policy.

The same settings can be seeded in `pro.json` (console settings take priority):

```json
{
  "mail": {
    "transport": "ses",
    "tenantProvider": "ses",
    "fromAddress": "news@example.com",
    "fromName": "Your publication",
    "ses": {
      "region": "us-east-1",
      "maxSendRate": 1,
      "user": "SES_SMTP_USERNAME",
      "pass": "SES_SMTP_PASSWORD"
    }
  }
}
```

To use managed Mailgun instead, set `mail.tenantProvider` to `mailgun` and fill
`mail.mailgun.apiKey`, `mail.mailgun.domain`, and `mail.mailgun.baseUrl`
(`https://api.mailgun.net/v3` for US or `https://api.eu.mailgun.net/v3` for EU).
Set `mail.mailgun.webhookSigningKey` for Mailgun event verification. Configure
`mail.transport` and its credentials separately for transactional messages.

SES uses mandatory STARTTLS on port 587. Generate **regional SMTP credentials**
in SES; the password differs from an AWS API secret access key. Verify the
sender in the same region and request production access if needed. See
[Amazon's SMTP setup guide](https://docs.aws.amazon.com/ses/latest/dg/send-email-smtp.html).
This integration sends through SMTP; SES delivery, bounce and complaint events
are not ingested through SNS. Mailgun's existing event handling remains available.


SES newsletters reuse connections across batches and send one personalized message
per subscriber, with a `List-Unsubscribe` header. Explicit temporary SMTP refusals
are retried with backoff; permanent refusals are recorded as failed recipients.
Use **Amazon SES → Newsletter messages per second** (`mail.ses.maxSendRate`) to
set the per-newsletter rate (default 1). Allocate your SES account rate across
concurrent tenant sends and reserve capacity for transactional email; this setting
is not a shared account-wide quota coordinator. AWS enforces account quotas.


## Console settings pages

`/console/settings` is a searchable directory, matching the CMS settings layout.
Open General settings, Signup & security, Billing, a payment provider, Email,
Amazon SES, or Mailgun to edit that section. Each page has a Save button and a
back link. Saving returns to the same section and leaves other sections alone.
Provider credentials remain write-only, and role restrictions still apply.
Search matches setting names and descriptions and links to individual fields.
Staff, Plans, and History remain available from the directory.


## Monthly email allowances and usage

Every monthly and yearly plan includes custom integrations and API access,
social web and custom fields, paid memberships via Stripe, and analytics.

Every public plan includes a monthly allowance: Starter **5,000**, Creator
**25,000**, and Business **100,000** emails. The same monthly volumes apply to
yearly subscriptions. Edit individual plan volumes at **Settings → Plans**;
these changes are queued for existing sites as well as used for new sites.
Old catalogues without an email ceiling adopt the tier defaults.

One newsletter recipient or one transactional message counts as one email.
The allowance counts provider-accepted submissions; inbox delivery is a separate
provider event. Failed and blocked attempts do not consume volume. Sends reserve
capacity atomically before contacting SES, Mailgun, SMTP, or direct delivery,
so concurrent newsletters and regular mail share one budget. Pending or
interrupted reservations retain capacity conservatively. The volume renews on
the subscription's monthly anniversary, even if its invoice is annual.

**Settings → Email → Stop sending at the email allowance** defaults to on.
A site's console detail page can inherit that default, stop at its allowance,
or continue counting and sending beyond the allowance. Changing this policy
queues a tenant configuration update. Suspending a site retains its suspended
plan restrictions; no site is restored or reopened by usage reporting.

The site's **Email sending** section shows sent, remaining and pending volume,
newsletter and transactional totals, failed/blocked attempts, daily activity,
and sending providers. A running site is queried on page load. Sleeping sites
show a dated cached report; **Refresh usage** wakes the site to collect a new
report. The platform uses its existing Admin API integration key and keeps
report data private. Update both binaries for reporting and enforcement.

Known historical newsletter submissions are imported into the usage ledger.
Transactional mail sent before this feature was installed has no historic
ledger and cannot be reconstructed; its usage is tracked from installation.

For optional native analytics provisioning, staff access and CMS reports, see
[NATIVE_CLICK.md](NATIVE_CLICK.md). Enable it only after registering this
platform at the independent Click service and deploying the matching CMS build.
