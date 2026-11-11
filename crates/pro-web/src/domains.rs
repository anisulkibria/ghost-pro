//! The Domains screens, in both panels, and the domain operations they share.
//!
//! A customer sees every address on every site they own, at
//! `/dashboard/domains`; an operator sees every address on the platform, at
//! `/console/domains`. Each domain has its own page in both panels, which is
//! where everything about one hostname lives: whether it is serving, the DNS
//! records it needs, its certificate, whether it is the site's primary
//! address, and what has happened to it.
//!
//! The operations themselves — check, make primary, remove — are here rather
//! than in either panel, because both panels perform them and the side
//! effects (rewriting the site's `url`, flushing the edge's host cache, the
//! audit trail) must not drift apart between the two.
use axum::extract::{Form, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use pro_core::{Domain, DomainKind, DomainStatus, ProError, Result, StaffRole, Tenant};
use pro_db::domains::DomainListing;
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth;
use crate::reply::{self, redirect_with};
use crate::state::AppState;
use crate::view::{self, attr, esc, BannerKind, Flash, Theme};

// ---------------------------------------------------------------------------
// Shared operations
// ---------------------------------------------------------------------------

/// A domain, checked to belong to this tenant.
///
/// 404 rather than 403 for somebody else's: an id that belongs to another
/// site is indistinguishable from one that was never issued.
pub(crate) async fn of_tenant(state: &AppState, tenant: &Tenant, domain_id: &str) -> Result<Domain> {
    let domain = pro_db::domains::get(&state.db, domain_id)
        .await
        .map_err(|_| ProError::NotFound("No such domain on this site.".into()))?;
    if domain.tenant_id != tenant.id {
        return Err(ProError::NotFound("No such domain on this site.".into()));
    }
    Ok(domain)
}

/// Check a domain's DNS now and redirect with what was found.
pub(crate) async fn check_and_report(
    state: &AppState,
    domain: &Domain,
    back: &str,
) -> Result<Response> {
    if domain.kind == DomainKind::Subdomain {
        // The platform's own zone: nothing to look up, and a lookup that
        // happened to fail must not demote the address of last resort.
        return Ok(redirect_with(
            back,
            Flash::ok(format!("{} is managed by the platform.", domain.domain)),
        ));
    }
    let verdict = crate::verify_domain_now(state, domain).await?;
    Ok(if verdict.is_servable() {
        redirect_with(back, Flash::ok(format!("{} is connected.", domain.domain)))
    } else {
        redirect_with(back, Flash::err(verdict.message()))
    })
}

/// Make a domain the site's canonical address.
pub(crate) async fn make_primary(
    state: &AppState,
    tenant: &Tenant,
    domain_id: &str,
    actor: &str,
) -> Result<()> {
    let domain = of_tenant(state, tenant, domain_id).await?;
    pro_db::domains::set_primary(&state.db, &tenant.id, domain_id).await?;
    // The tenant's `url` is what every link it emits is built from, so it has
    // to be rewritten and the process restarted.
    pro_provision::update_primary_domain(&state.db, &state.config, &state.supervisor, tenant)
        .await?;
    state.hosts.invalidate_all();

    pro_db::audit::record(
        &state.db,
        actor,
        "domain.primary_changed",
        Some(&tenant.id),
        Some(&tenant.account_id),
        serde_json::json!({"domain_id": domain.id, "domain": domain.domain}),
    )
    .await;
    Ok(())
}

/// Detach a custom domain from its site. Returns what was removed.
pub(crate) async fn detach(
    state: &AppState,
    tenant: &Tenant,
    domain_id: &str,
    actor: &str,
) -> Result<Domain> {
    let domain = of_tenant(state, tenant, domain_id).await?;
    pro_db::domains::remove(&state.db, &tenant.id, domain_id).await?;

    // Removing the canonical hostname moves the site back to its built-in
    // address, which means rewriting `url`.
    if domain.is_primary {
        pro_provision::update_primary_domain(&state.db, &state.config, &state.supervisor, tenant)
            .await?;
    }
    state.hosts.invalidate(&domain.domain);
    state.hosts.invalidate_all();

    pro_db::audit::record(
        &state.db,
        actor,
        "domain.removed",
        Some(&tenant.id),
        Some(&tenant.account_id),
        serde_json::json!({"domain_id": domain.id, "domain": domain.domain}),
    )
    .await;
    Ok(domain)
}

// ---------------------------------------------------------------------------
// Rendering shared by both panels
// ---------------------------------------------------------------------------

/// Which panel a page is being drawn for. Decides the URLs, and what is
/// shown: an operator sees the owner and the raw record, a customer does not.
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Customer,
    Console,
}

impl Panel {
    fn site_href(self, tenant_id: &str) -> String {
        match self {
            Panel::Customer => format!("/dashboard/sites/{tenant_id}"),
            Panel::Console => format!("/console/sites/{tenant_id}"),
        }
    }

    fn domain_href(self, domain_id: &str) -> String {
        match self {
            Panel::Customer => format!("/dashboard/domains/{domain_id}"),
            Panel::Console => format!("/console/domains/{domain_id}"),
        }
    }

    fn list_href(self) -> &'static str {
        match self {
            Panel::Customer => "/dashboard/domains",
            Panel::Console => "/console/domains",
        }
    }

    /// Where one of a domain's buttons posts.
    ///
    /// The customer's go through the site-scoped routes the site page already
    /// uses, with `back=domain` so they come back here afterwards.
    fn action(self, d: &Domain, verb: &str) -> String {
        match self {
            Panel::Customer => {
                format!("/dashboard/sites/{}/domains/{}/{verb}?back=domain", d.tenant_id, d.id)
            }
            Panel::Console => format!("/console/domains/{}/{verb}", d.id),
        }
    }
}

fn kind_label(kind: DomainKind) -> &'static str {
    match kind {
        DomainKind::Subdomain => "Built in",
        DomainKind::Custom => "Custom domain",
    }
}

fn pill_for(l: &DomainListing) -> String {
    if l.tenant_status.serves_public() {
        view::domain_pill(l.domain.status)
    } else {
        view::domain_pill_closed(l.domain.status)
    }
}

const PRIMARY_CHIP: &str = r#" <span class="chip tonal">Primary</span>"#;

/// An apex is `example.com`; anything with more labels is a subdomain and can
/// take a `CNAME`. Matches `pro_proxy::dns::instructions`.
fn is_apex(host: &str) -> bool {
    host.matches('.').count() <= 1
}

/// The sentence at the top of a domain's page: what state it is in, in words,
/// and what — if anything — its owner has to do about it.
fn status_banner(state: &AppState, panel: Panel, l: &DomainListing) -> String {
    let d = &l.domain;
    let last = d
        .last_error
        .as_deref()
        .map(|e| format!(" <span class=\"muted\">Last check: {}</span>", esc(e)))
        .unwrap_or_default();
    let scheme = if state.config.public_tls() { "https" } else { "http" };

    if !l.tenant_status.serves_public() && d.status == DomainStatus::Active {
        return view::banner(
            BannerKind::Warn,
            &format!(
                "This address is connected, but <a href=\"{site}\">{name}</a> is {status} \
                 and is not open to visitors right now.",
                site = attr(&panel.site_href(&d.tenant_id)),
                name = esc(&l.tenant_name),
                status = esc(&l.tenant_status.label().to_lowercase()),
            ),
        );
    }
    match d.status {
        DomainStatus::Active => view::banner(
            BannerKind::Ok,
            &format!(
                "Connected. Visitors reach <a href=\"{site}\">{name}</a> at \
                 <a class=\"mono\" href=\"{scheme}://{host}\" target=\"_blank\" rel=\"noopener\">{scheme}://{host}</a>.",
                site = attr(&panel.site_href(&d.tenant_id)),
                name = esc(&l.tenant_name),
                host = esc(&d.domain),
            ),
        ),
        DomainStatus::Pending => view::banner(
            BannerKind::Info,
            &format!(
                "Waiting for DNS. Add the records below at the company you bought {} \
                 from. We check automatically, and it goes live as soon as they are visible.{last}",
                esc(&d.domain)
            ),
        ),
        DomainStatus::Verified => view::banner(
            BannerKind::Info,
            &format!(
                "Ownership is confirmed. Point the address at the platform with the record \
                 below to start serving it.{last}"
            ),
        ),
        DomainStatus::Failed => view::banner(
            BannerKind::Error,
            &format!(
                "This domain does not point here yet, so it is not serving. Check the records \
                 below at your domain provider.{last}"
            ),
        ),
    }
}

/// The DNS card: what to publish, or, for a built-in address, that there is
/// nothing to publish.
fn dns_section(state: &AppState, panel: Panel, l: &DomainListing) -> String {
    let d = &l.domain;
    let mut out = view::section("DNS");

    if d.kind == DomainKind::Subdomain {
        out.push_str(&view::rows(&view::row(
            "dns",
            "blue",
            "Managed by the platform",
            "This address is part of the platform's own zone, so there is nothing to set up at a \
             domain provider. It cannot be removed: it keeps the site reachable if a custom \
             domain's DNS ever stops working.",
            "",
        )));
        return out;
    }

    let expect = crate::domain_expectation(state);
    let dns = pro_proxy::dns::instructions(&d.domain, &d.verification_token, &expect);

    // An apex needs A records, and they can only be given if the platform knows
    // its own public address. Without one the table would list only the TXT
    // record and the customer would be left with nothing that routes traffic.
    if is_apex(&d.domain) && expect.addresses.is_empty() {
        out.push_str(&view::banner(
            BannerKind::Warn,
            &match panel {
                Panel::Customer => format!(
                    "{} is a root domain, which needs an A record, and the platform's address \
                     is not published yet. Connect a subdomain such as <code>www.{}</code> \
                     instead, or contact support.",
                    esc(&d.domain),
                    esc(&d.domain)
                ),
                Panel::Console => "Root domains need A records, and <code>publicAddresses</code> \
                                   is not set in the platform config, so none can be shown. Set \
                                   it to the server's public IP addresses."
                    .to_string(),
            },
        ));
    }

    let records: String = dns
        .records
        .iter()
        .map(|r| {
            format!(
                r#"<tr><td><span class="chip outline">{}</span></td><td class="mono">{}</td>
<td class="mono wrap">{}</td><td class="muted wrap">{}</td></tr>"#,
                esc(r.kind),
                esc(&r.name),
                esc(&r.value),
                esc(r.note),
            )
        })
        .collect();

    let hint = if d.status == DomainStatus::Active {
        "Keep these records in place. Removing them takes this address offline."
    } else {
        "Add these at your domain provider. DNS changes can take up to an hour to be visible."
    };
    let check = if d.status != DomainStatus::Active {
        view::action_button(&panel.action(d, "check"), "Check now", "tonal small", None)
    } else {
        String::new()
    };
    out.push_str(&format!(
        r#"<div class="card padded">
<div class="inline-form" style="justify-content:space-between;align-items:center;margin-bottom:12px">
<p class="hint" style="margin:0">{hint}</p>{check}</div>
<div class="tbl-wrap"><table class="tbl">
<thead><tr><th>Type</th><th>Name</th><th>Value</th><th>Note</th></tr></thead>
<tbody>{records}</tbody></table></div></div>"#,
        hint = esc(hint),
        check = check,
        records = records,
    ));
    out
}

/// The certificate, as far as the platform can say without reading it.
fn https_section(state: &AppState, panel: Panel, l: &DomainListing) -> String {
    let d = &l.domain;
    let ssl = &state.config.ssl;
    let (icon, tint, title, detail): (&str, &str, &str, String) = if !state.config.public_tls() {
        (
            "lock",
            "slate",
            "HTTPS is not enabled",
            "This platform serves plain HTTP, so there is no certificate to issue.".into(),
        )
    } else if !ssl.enabled {
        (
            "lock",
            "blue",
            "HTTPS is handled in front of the platform",
            "Certificates are managed by the proxy or load balancer the platform runs behind."
                .into(),
        )
    } else if d.status == DomainStatus::Active {
        (
            "lock",
            "green",
            "Certificate issued automatically",
            "From Let's Encrypt, and renewed before it expires. There is nothing to do.".into(),
        )
    } else {
        (
            "clock",
            "amber",
            "Certificate pending",
            "A certificate is requested automatically as soon as this domain points here."
                .into(),
        )
    };
    let staging = if panel == Panel::Console && ssl.enabled && ssl.staging {
        view::banner(
            BannerKind::Warn,
            "<code>ssl.staging</code> is on: certificates come from Let's Encrypt's test CA and \
             browsers will not trust them.",
        )
    } else {
        String::new()
    };
    format!(
        "{}{}{}",
        view::section("HTTPS"),
        staging,
        view::rows(&view::row(icon, tint, title, &esc(&detail), ""))
    )
}

/// Primary address and removal.
fn manage_section(panel: Panel, l: &DomainListing, can_write: bool) -> String {
    let d = &l.domain;
    let mut rows = String::new();

    rows.push_str(&if d.is_primary {
        view::row(
            "check",
            "green",
            "Primary address",
            "The site's links, sitemap and redirects all use this address.",
            r#"<span class="chip tonal">Primary</span>"#,
        )
    } else if d.status == DomainStatus::Active {
        view::row(
            "globe",
            "blue",
            "Make this the primary address",
            "Links and redirects move to it, and the site restarts for a moment to pick it up.",
            &if can_write {
                view::action_button(
                    &panel.action(d, "primary"),
                    "Make primary",
                    "outlined small",
                    Some("Make this the main address?|Existing links will redirect to it."),
                )
            } else {
                String::new()
            },
        )
    } else {
        view::row(
            "globe",
            "slate",
            "Not the primary address",
            "It can become the primary address once it is connected.",
            "",
        )
    });

    if d.kind == DomainKind::Custom && can_write {
        let detail = if d.is_primary {
            format!(
                "Visitors using {} stop reaching the site, and the built-in address becomes \
                 primary again.",
                esc(&d.domain)
            )
        } else {
            format!("Visitors using {} stop reaching the site.", esc(&d.domain))
        };
        rows.push_str(&view::row(
            "delete",
            "red",
            "Remove this domain",
            &detail,
            &view::action_button(
                &panel.action(d, "remove"),
                "Remove",
                "text danger small",
                Some("Remove this domain?|Visitors using it will stop reaching the site."),
            ),
        ));
    }

    format!("{}{}", view::section("Manage"), view::rows(&rows))
}

/// What has happened to this domain, from the audit trail.
async fn history_section(state: &AppState, panel: Panel, l: &DomainListing) -> Result<String> {
    let d = &l.domain;
    let events = pro_db::audit::list(&state.db, Some(&d.tenant_id), None, 500, 0).await?;
    let mine = events.into_iter().filter(|e| {
        e.action.starts_with("domain.")
            && (e.meta.get("domain_id").and_then(|v| v.as_str()) == Some(d.id.as_str())
                || e.meta.get("domain").and_then(|v| v.as_str()) == Some(d.domain.as_str()))
    });

    let mut staff_names: HashMap<String, String> = HashMap::new();
    let mut rows = String::new();
    for e in mine.take(50) {
        let what = match e.action.as_str() {
            "domain.added" => "Added",
            "domain.verified" => "Connected — DNS verified",
            "domain.primary_changed" => "Made the primary address",
            "domain.removed" => "Removed",
            other => other,
        };
        let who = if e.actor == "system" {
            "Automatic check".to_string()
        } else if e.actor.starts_with("account:") {
            match panel {
                Panel::Customer => "You".into(),
                Panel::Console => "Customer".into(),
            }
        } else if let Some(id) = e.actor.strip_prefix("staff:") {
            match panel {
                Panel::Customer => "Platform support".into(),
                Panel::Console => {
                    if !staff_names.contains_key(id) {
                        let name = pro_db::staff::get(&state.db, id)
                            .await
                            .map(|s| s.name)
                            .unwrap_or_else(|_| "Staff".into());
                        staff_names.insert(id.to_string(), name);
                    }
                    staff_names[id].clone()
                }
            }
        } else {
            e.actor.clone()
        };
        rows.push_str(&format!(
            r#"<tr><td class="muted">{when}</td><td>{what}</td><td class="muted">{who}</td></tr>"#,
            when = esc(&view::when(Some(&e.created_at))),
            what = esc(what),
            who = esc(&who),
        ));
    }
    if rows.is_empty() {
        rows = view::empty_row(3, "Nothing recorded for this domain yet.");
    }
    Ok(format!(
        "{}{}",
        view::section("History"),
        view::table(&["When", "Event", "By"], &rows)
    ))
}

/// The raw record, for an operator debugging a stuck domain.
fn record_section(l: &DomainListing) -> String {
    let d = &l.domain;
    let line = |k: &str, v: &str| {
        format!(
            r#"<tr><td class="muted">{}</td><td class="mono wrap">{}</td></tr>"#,
            esc(k),
            esc(if v.is_empty() { "—" } else { v })
        )
    };
    let rows = [
        line("Domain id", &d.id),
        line("Site id", &d.tenant_id),
        line("Status", d.status.as_str()),
        line("Kind", d.kind.as_str()),
        line("Verification token", &d.verification_token),
        line("Added", &d.created_at),
        line("Verified", d.verified_at.as_deref().unwrap_or("")),
        line("Last checked", d.last_checked_at.as_deref().unwrap_or("")),
        line("Last error", d.last_error.as_deref().unwrap_or("")),
    ]
    .concat();
    format!("{}{}", view::section("Record"), view::table(&["Field", "Value"], &rows))
}

/// Everything on a domain's page below the shell.
async fn detail_body(
    state: &AppState,
    panel: Panel,
    l: &DomainListing,
    can_write: bool,
) -> Result<String> {
    let d = &l.domain;
    let scheme = if state.config.public_tls() { "https" } else { "http" };

    let owner = match panel {
        Panel::Customer => String::new(),
        Panel::Console => format!(
            r#" · owned by <a href="/console/customers/{}">{}</a>"#,
            attr(&l.account_id),
            esc(&l.account_email)
        ),
    };
    let sub = format!(
        r#"{pill}{primary} · {kind} on <a href="{site}">{name}</a>{owner}"#,
        pill = pill_for(l),
        primary = if d.is_primary { PRIMARY_CHIP } else { "" },
        kind = esc(kind_label(d.kind)),
        site = attr(&panel.site_href(&d.tenant_id)),
        name = esc(&l.tenant_name),
        owner = owner,
    );
    let visit = if d.status == DomainStatus::Active && l.tenant_status.serves_public() {
        format!(
            r#"<a class="btn outlined" href="{scheme}://{host}" target="_blank" rel="noopener">{open} Visit</a>"#,
            host = attr(&d.domain),
            open = view::svg("open"),
        )
    } else {
        String::new()
    };
    let actions = format!(
        r#"<a class="btn text" href="{}">All domains</a>{visit}"#,
        panel.list_href()
    );

    let mut body = view::page_head(&d.domain, &sub, &actions);
    body.push_str(&status_banner(state, panel, l));

    body.push_str(r#"<div class="tiles">"#);
    // Times only: the state and the kind are already in the chip and the line
    // under the title, and words that long wrap badly in a figure tile.
    body.push_str(&view::metric("Added", &view::when(Some(&d.created_at)), None));
    body.push_str(&view::metric(
        "Last checked",
        &view::when(d.last_checked_at.as_deref()),
        match (d.kind, d.last_checked_at.is_none()) {
            (DomainKind::Subdomain, _) => Some("Managed by the platform"),
            (_, true) => Some("Not yet"),
            _ => None,
        },
    ));
    body.push_str(&view::metric(
        "Connected",
        &view::when(d.verified_at.as_deref()),
        if d.verified_at.is_none() { Some("Not yet") } else { None },
    ));
    body.push_str("</div>");

    body.push_str(&dns_section(state, panel, l));
    body.push_str(&https_section(state, panel, l));
    body.push_str(&manage_section(panel, l, can_write));
    body.push_str(&history_section(state, panel, l).await?);
    if panel == Panel::Console {
        body.push_str(&record_section(l));
    }
    Ok(body)
}

// ---------------------------------------------------------------------------
// Customer: /dashboard/domains
// ---------------------------------------------------------------------------

pub async fn customer_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/dashboard/signin",
        &chrome,
        customer_list_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn customer_list_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let account = auth::account(state, headers).await?;
    let listings = pro_db::domains::list_for_account(&state.db, &account.id).await?;
    let tenants = pro_db::tenants::list_for_account(&state.db, &account.id).await?;
    let live = state.live();
    let panel = Panel::Customer;

    let sites = listings
        .iter()
        .map(|l| l.domain.tenant_id.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    let mut body = view::page_head(
        "Domains",
        &format!(
            "{} address{} across {} site{}",
            view::number(listings.len() as i64),
            if listings.len() == 1 { "" } else { "es" },
            view::number(sites as i64),
            if sites == 1 { "" } else { "s" },
        ),
        "",
    );

    if listings.is_empty() {
        body.push_str(&view::empty(
            "No domains yet",
            "Every site gets its own address the moment it is created. Create a site, then \
             connect your own domain to it here.",
            Some(("Create a site", "/dashboard/new")),
        ));
    } else {
        let mut rows = String::new();
        for l in &listings {
            let d = &l.domain;
            rows.push_str(&format!(
                r#"<tr><td class="wrap"><a class="lead mono" href="{href}">{domain}</a>{primary}</td>
<td><a href="{site}">{name}</a></td><td>{status}</td><td class="muted">{kind}</td>
<td class="end"><a class="btn text small" href="{href}">Manage</a></td></tr>"#,
                href = attr(&panel.domain_href(&d.id)),
                domain = esc(&d.domain),
                primary = if d.is_primary { PRIMARY_CHIP } else { "" },
                site = attr(&panel.site_href(&d.tenant_id)),
                name = esc(&l.tenant_name),
                status = pill_for(l),
                kind = esc(kind_label(d.kind)),
            ));
        }
        body.push_str(&view::table(&["Domain", "Site", "Status", "Type", "end:"], &rows));
    }

    // ---- connect a domain -------------------------------------------------
    // Offered only for sites whose plan has room for another custom domain,
    // so the form never invites an add that the plan will refuse.
    let mut options = String::new();
    let mut any_site = false;
    for t in tenants.iter().filter(|t| {
        !matches!(t.status, pro_core::TenantStatus::Deleted | pro_core::TenantStatus::PendingDeletion)
    }) {
        any_site = true;
        let allowance = live.plan(&t.plan_slug).map(|p| p.max_custom_domains).unwrap_or(0);
        let used = listings
            .iter()
            .filter(|l| l.domain.tenant_id == t.id && l.domain.kind == DomainKind::Custom)
            .count() as i64;
        if used < allowance {
            options.push_str(&format!(
                r#"<option value="{}">{}</option>"#,
                attr(&t.id),
                esc(&t.name)
            ));
        }
    }
    if any_site {
        body.push_str(&view::section("Connect your own domain"));
        if options.is_empty() {
            body.push_str(&view::banner(
                BannerKind::Info,
                r#"Your sites have used every custom domain their plans include.
<a href="/dashboard/billing">Upgrade</a> to connect another."#,
            ));
        } else {
            body.push_str(&view::card(&view::form(
                "/dashboard/domains",
                &format!(
                    r#"<div class="inline-form">{site}{field}
<button class="btn" type="submit">{add} Add domain</button></div>
<p class="hint">For example <code>blog.yourcompany.com</code>, or <code>yourcompany.com</code>.
You will be shown the DNS records to add next.</p>"#,
                    site = view::select("site", "Site", &options, None, false),
                    field = view::field("domain", "Your own domain", "text", "", None, true),
                    add = view::svg("add"),
                ),
            )));
        }
    }

    Ok(crate::dashboard::shell(
        state,
        theme,
        "Domains",
        crate::dashboard::Tab::Domains,
        &account,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

#[derive(Deserialize)]
pub struct ConnectForm {
    pub site: String,
    pub domain: String,
}

/// `POST /dashboard/domains` — the Domains screen's add form.
pub async fn customer_connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ConnectForm>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect("/dashboard/domains", &chrome, async {
        let added =
            crate::dashboard::add_domain_inner(&state, &headers, &form.site, &form.domain).await?;
        Ok(redirect_with(
            &format!("/dashboard/domains/{}", added.id),
            Flash::ok(format!("{} added. Now point its DNS here.", added.domain)),
        ))
    }.await)
}

pub async fn customer_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/dashboard/signin",
        &chrome,
        async {
            let account = auth::account(&state, &headers).await?;
            let listing = pro_db::domains::listing(&state.db, &id).await?;
            if listing.account_id != account.id {
                return Err(ProError::NotFound("No such domain.".into()));
            }
            let body = detail_body(&state, Panel::Customer, &listing, true).await?;
            Ok(crate::dashboard::shell(
                &state,
                chrome.theme,
                &listing.domain.domain,
                crate::dashboard::Tab::Domains,
                &account,
                Flash::from_query(&query).as_ref(),
                &body,
            ))
        }
        .await,
    )
}

// ---------------------------------------------------------------------------
// Console: /console/domains
// ---------------------------------------------------------------------------

pub async fn console_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin",
        &chrome,
        console_list_inner(&state, chrome.theme, &headers, &query).await,
    )
}

async fn console_list_inner(
    state: &AppState,
    theme: Theme,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Response> {
    let member = auth::staff(state, headers).await?;
    let panel = Panel::Console;

    let status_filter = query.get("status").and_then(|s| DomainStatus::parse(s));
    let kind_filter = query.get("kind").and_then(|s| DomainKind::parse(s));
    let search = query.get("q").map(String::as_str).filter(|s| !s.trim().is_empty());
    let listings =
        pro_db::domains::list_all(&state.db, status_filter, kind_filter, search, 500, 0).await?;
    let by_status = pro_db::domains::count_by_status(&state.db).await?;
    let count = |name: &str| by_status.iter().find(|(s, _)| s == name).map(|(_, n)| *n).unwrap_or(0);

    // Filter links keep the other filter, so narrowing by type does not throw
    // away the status tab and vice versa.
    let href = |status: &str, kind: &str| {
        let mut params = Vec::new();
        if !status.is_empty() {
            params.push(format!("status={status}"));
        }
        if !kind.is_empty() {
            params.push(format!("kind={kind}"));
        }
        if let Some(q) = search {
            params.push(format!("q={}", view::urlencode(q)));
        }
        if params.is_empty() {
            "/console/domains".to_string()
        } else {
            format!("/console/domains?{}", params.join("&"))
        }
    };
    let current_status = status_filter.map(|s| s.as_str()).unwrap_or("");
    let current_kind = kind_filter.map(|k| k.as_str()).unwrap_or("");

    let status_tabs: Vec<(String, String, Option<i64>, bool)> = [
        ("", "All", by_status.iter().map(|(_, n)| *n).sum::<i64>()),
        ("active", DomainStatus::Active.label(), count("active")),
        ("pending", DomainStatus::Pending.label(), count("pending")),
        ("verified", DomainStatus::Verified.label(), count("verified")),
        ("failed", DomainStatus::Failed.label(), count("failed")),
    ]
    .iter()
    .map(|(value, label, n)| (href(value, current_kind), label.to_string(), Some(*n), *value == current_status))
    .collect();
    let kind_tabs: Vec<(String, String, Option<i64>, bool)> =
        [("", "Any type"), ("custom", "Custom domains"), ("subdomain", "Built in")]
            .iter()
            .map(|(value, label)| (href(current_status, value), label.to_string(), None, *value == current_kind))
            .collect();

    let mut rows = String::new();
    for l in &listings {
        let d = &l.domain;
        rows.push_str(&format!(
            r#"<tr><td class="wrap"><a class="lead mono" href="{href}">{domain}</a>{primary}</td>
<td><a href="{site}">{name}</a><span class="sub mono">{slug}</span></td>
<td><a href="/console/customers/{account}">{email}</a></td>
<td>{status}</td><td class="muted">{kind}</td><td class="muted">{checked}</td></tr>"#,
            href = attr(&panel.domain_href(&d.id)),
            domain = esc(&d.domain),
            primary = if d.is_primary { PRIMARY_CHIP } else { "" },
            site = attr(&panel.site_href(&d.tenant_id)),
            name = esc(&l.tenant_name),
            slug = esc(&l.tenant_slug),
            account = attr(&l.account_id),
            email = esc(&l.account_email),
            status = pill_for(l),
            kind = esc(kind_label(d.kind)),
            checked = esc(&view::when(d.last_checked_at.as_deref())),
        ));
    }

    let body = format!(
        "{head}{status_tabs}{kind_tabs}{search}{table}",
        head = view::page_head(
            "Domains",
            &format!("{} shown", view::number(listings.len() as i64)),
            ""
        ),
        status_tabs = view::seg_bar(&status_tabs),
        kind_tabs = view::seg_bar(&kind_tabs),
        search = view::search_form(
            "/console/domains",
            search.unwrap_or(""),
            "Search by domain, site or owner email"
        ),
        table = if listings.is_empty() {
            view::empty("No domains match", "Try a different filter or search.", None)
        } else {
            view::table(&["Domain", "Site", "Owner", "Status", "Type", "Last checked"], &rows)
        },
    );

    Ok(crate::console::shell(
        state,
        theme,
        "Domains",
        crate::console::Tab::Domains,
        &member,
        Flash::from_query(query).as_ref(),
        &body,
    ))
}

pub async fn console_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin(
        "/console/signin",
        &chrome,
        async {
            let member = auth::staff(&state, &headers).await?;
            let listing = pro_db::domains::listing(&state.db, &id).await?;
            let can_write = member.can(StaffRole::Admin);
            let body = detail_body(&state, Panel::Console, &listing, can_write).await?;
            Ok(crate::console::shell(
                &state,
                chrome.theme,
                &listing.domain.domain,
                crate::console::Tab::Domains,
                &member,
                Flash::from_query(&query).as_ref(),
                &body,
            ))
        }
        .await,
    )
}

/// `POST /console/domains/{id}/check`. Open to support staff: it changes
/// nothing the sweeper would not, and "check it again" is the first thing a
/// customer stuck on DNS asks for.
pub async fn console_check(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/domains/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        auth::staff_with(&state, &headers, StaffRole::Support).await?;
        let domain = pro_db::domains::get(&state.db, &id).await?;
        check_and_report(&state, &domain, &back).await
    }.await)
}

pub async fn console_primary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/domains/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = auth::staff_with(&state, &headers, StaffRole::Admin).await?;
        let domain = pro_db::domains::get(&state.db, &id).await?;
        let tenant = pro_db::tenants::get(&state.db, &domain.tenant_id).await?;
        make_primary(&state, &tenant, &id, &format!("staff:{}", member.id)).await?;
        Ok(redirect_with(&back, Flash::ok("Primary address updated.")))
    }.await)
}

pub async fn console_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let chrome = state.chrome(&headers);
    let back = format!("/console/domains/{id}");
    reply::or_redirect(&back, &chrome, async {
        auth::require_same_origin(&headers, &state.config.host())?;
        let member = auth::staff_with(&state, &headers, StaffRole::Admin).await?;
        let domain = pro_db::domains::get(&state.db, &id).await?;
        let tenant = pro_db::tenants::get(&state.db, &domain.tenant_id).await?;
        let removed = detach(&state, &tenant, &id, &format!("staff:{}", member.id)).await?;
        Ok(redirect_with(
            "/console/domains",
            Flash::ok(format!("{} removed from {}.", removed.domain, tenant.name)),
        ))
    }.await)
}
