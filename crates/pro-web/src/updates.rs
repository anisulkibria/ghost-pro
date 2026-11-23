//! Updating Vuuut from the console: `/owner/updates`.
//!
//! The work is split by privilege. This process runs as an unprivileged
//! user and can neither write the Vuuut checkout nor run the compiler, so it
//! only *asks*: it writes `request.json` into `<data>/updates/`. A root
//! systemd unit (`deploy/systemd/vuuut-update.path` → `vuuut-update.service`
//! → `deploy/vuuut-update.sh`) notices the file, fetches the branch from
//! GitHub, syncs it into the checkout, builds the binary and reports each
//! step in `status.json`.
//!
//! When the build reports `built`, [`run_updates`] here rolls it out: every
//! site is restarted one at a time on the new binary, which is what applies
//! the new database migrations — Vuuut migrates at boot. Sites that were
//! asleep are woken for it and put back to sleep, so no database is left on
//! the old schema waiting for its next visitor. A site that will not start
//! is named in the result, not hidden behind a total.
//!
//! The state machine, in `status.json`:
//! `requested` (here) → `fetching` → `syncing` → `building` → `built`
//! (the script) → `rolling_out` → `done` | `done_with_errors` (here), or
//! `failed` from any step.
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use pro_core::{ProError, Result, Staff, StaffRole, TenantStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::auth;
use crate::console::{shell, Tab};
use crate::reply::{self, redirect_with};
use crate::state::AppState;
use crate::view::{self, esc, BannerKind, Flash, PillKind, Theme};

/// One site's part of a rollout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SiteResult {
    pub slug: String,
    pub ok: bool,
    pub detail: String,
}

/// `status.json`: the last update, in progress or finished.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub id: String,
    pub state: String,
    /// What the script is doing, in words, while it works.
    pub step: String,
    /// Why it failed, or a one-line summary when it is done.
    pub message: String,
    pub repository: String,
    pub branch: String,
    pub commit: String,
    pub previous_commit: String,
    pub requested_by: String,
    pub requested_at: String,
    pub updated_at: String,
    pub finished_at: String,
    pub sites: Vec<SiteResult>,
}

impl Status {
    /// Anything but a finished or failed update holds the button.
    pub fn busy(&self) -> bool {
        matches!(self.state.as_str(), "requested" | "fetching" | "syncing" | "building" | "built" | "rolling_out")
    }

    fn label(&self) -> (&'static str, PillKind) {
        match self.state.as_str() {
            "requested" => ("Waiting for the update service", PillKind::Warn),
            "fetching" => ("Fetching from GitHub", PillKind::Warn),
            "syncing" => ("Copying the source", PillKind::Warn),
            "building" => ("Building", PillKind::Warn),
            "built" | "rolling_out" => ("Restarting sites and migrating", PillKind::Warn),
            "done" => ("Updated", PillKind::Ok),
            "done_with_errors" => ("Updated, some sites need attention", PillKind::Bad),
            "failed" => ("Failed", PillKind::Bad),
            _ => ("No update yet", PillKind::Idle),
        }
    }
}

/// `current.json`: what the sites are running, once an update has finished.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Installed {
    pub commit: String,
    pub repository: String,
    pub branch: String,
    pub installed_at: String,
}

/// `request.json`: what the root script is asked to install. Holds the token,
/// so the script deletes it as soon as it has read it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Request {
    id: String,
    repository: String,
    branch: String,
    token: String,
}

pub fn dir(state: &AppState) -> PathBuf {
    state.config.data_dir().join("updates")
}

fn read<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Write through a temporary file and a rename, so the script and this
/// process never see half a file.
fn write<T: Serialize>(path: &Path, value: &T, private: bool) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let body = serde_json::to_vec_pretty(value).map_err(|e| ProError::Internal(e.to_string()))?;
    std::fs::write(&tmp, body).map_err(|e| ProError::Internal(format!("could not write {}: {e}", tmp.display())))?;
    if private {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| ProError::Internal(format!("could not write {}: {e}", path.display())))
}

pub fn status(state: &AppState) -> Status {
    read(&dir(state).join("status.json"))
}

pub fn installed(state: &AppState) -> Installed {
    read(&dir(state).join("current.json"))
}

fn save_status(state: &AppState, status: &Status) -> Result<()> {
    write(&dir(state).join("status.json"), status, false)
}

/// `owner/name`, letters, digits, `-`, `_` and `.` only.
pub fn valid_repository(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| !p.is_empty() && p != "." && p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// A git branch name the script can be handed safely.
pub fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 100
        && !branch.starts_with(['-', '/', '.'])
        && !branch.ends_with(['/', '.'])
        && !branch.contains("..")
        && !branch.contains("//")
        && branch.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

// ---------------------------------------------------------------------------
// asking for an update
// ---------------------------------------------------------------------------

/// Hand the root script an update to install.
pub fn request(state: &AppState, member: &Staff) -> Result<Status> {
    let current = status(state);
    if current.busy() {
        return Err(ProError::BadRequest("An update is already running.".into()));
    }
    let live = state.live();
    let repository = live.updates_repository();
    let branch = live.updates_branch();
    if !valid_repository(&repository) {
        return Err(ProError::BadRequest("Set the Vuuut repository (owner/name) in Settings → Updates first.".into()));
    }
    if !valid_branch(&branch) {
        return Err(ProError::BadRequest("The update branch in Settings → Updates is not a valid branch name.".into()));
    }
    let dir = dir(state);
    std::fs::create_dir_all(&dir).map_err(|e| ProError::Internal(format!("could not create {}: {e}", dir.display())))?;
    let id = pro_core::ids::object_id();
    let now = pro_core::now_iso();
    let next = Status {
        id: id.clone(),
        state: "requested".into(),
        step: "Waiting for the update service to pick this up".into(),
        repository: repository.clone(),
        branch: branch.clone(),
        previous_commit: installed(state).commit,
        requested_by: member.email.clone(),
        requested_at: now.clone(),
        updated_at: now,
        ..Default::default()
    };
    save_status(state, &next)?;
    // Last: the file's appearance is what starts the root script.
    write(
        &dir.join("request.json"),
        &Request { id, repository, branch, token: live.updates_token().unwrap_or_default() },
        true,
    )?;
    Ok(next)
}

// ---------------------------------------------------------------------------
// rolling a finished build out
// ---------------------------------------------------------------------------

/// Watches `status.json` and rolls out each build the script finishes.
pub async fn run_updates(state: AppState) {
    loop {
        if let Err(e) = tick(&state).await {
            tracing::error!("update rollout failed: {e}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Seconds since an RFC 3339 / `YYYY-MM-DD HH:MM:SS` timestamp.
fn age_secs(at: &str) -> i64 {
    let parsed = chrono::DateTime::parse_from_rfc3339(at)
        .map(|d| d.with_timezone(&chrono::Utc))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S").map(|d| d.and_utc()));
    parsed.map(|d| (chrono::Utc::now() - d).num_seconds()).unwrap_or(0)
}

/// One look at `status.json`; [`run_updates`] calls this every few seconds.
pub async fn tick(state: &AppState) -> Result<()> {
    let mut st = status(state);
    match st.state.as_str() {
        // A request nobody picked up means the root units are not installed.
        "requested" if age_secs(&st.updated_at) > 120 => {
            let _ = std::fs::remove_file(dir(state).join("request.json"));
            fail(state, &mut st, "The update service did not pick up the request in two minutes. Is vuuut-update.path installed and enabled on this server?")
        }
        // The script reports at every step; an hour of silence is a hang.
        "fetching" | "syncing" | "building" if age_secs(&st.updated_at) > 3600 => {
            fail(state, &mut st, "The update service stopped reporting for an hour. See the log below.")
        }
        // A rollout interrupted by this process restarting starts over:
        // restarting a site twice is harmless, skipping one is not.
        "built" | "rolling_out" => roll_out(state, &mut st).await,
        _ => Ok(()),
    }
}

fn fail(state: &AppState, st: &mut Status, message: &str) -> Result<()> {
    st.state = "failed".into();
    st.message = message.into();
    st.updated_at = pro_core::now_iso();
    st.finished_at = st.updated_at.clone();
    save_status(state, st)
}

async fn roll_out(state: &AppState, st: &mut Status) -> Result<()> {
    st.state = "rolling_out".into();
    st.step = "Restarting each site on the new build".into();
    st.sites.clear();
    st.updated_at = pro_core::now_iso();
    save_status(state, st)?;

    let tenants = pro_db::tenants::list(&state.db, None, None, 100_000, 0).await?;
    for tenant in tenants.iter().filter(|t| !matches!(t.status, TenantStatus::Deleted | TenantStatus::Provisioning)) {
        st.step = format!("Migrating {}", tenant.slug);
        st.updated_at = pro_core::now_iso();
        save_status(state, st)?;
        let was_running = state.supervisor.is_running(&tenant.id);
        let outcome = if was_running {
            state.supervisor.restart(tenant).await.map(|_| "restarted on the new version".to_string())
        } else {
            // Woken only to migrate, then back to sleep.
            match state.supervisor.ensure_running(tenant).await {
                Ok(_) => state.supervisor.stop(&tenant.id).await.map(|_| "migrated while asleep".to_string()),
                Err(e) => Err(e),
            }
        };
        st.sites.push(match outcome {
            Ok(detail) => SiteResult { slug: tenant.slug.clone(), ok: true, detail },
            Err(e) => {
                tracing::error!(tenant = %tenant.slug, "did not start on the new build: {e}");
                SiteResult { slug: tenant.slug.clone(), ok: false, detail: format!("did not start: {e}") }
            }
        });
    }

    let failed = st.sites.iter().filter(|s| !s.ok).count();
    st.state = if failed == 0 { "done".into() } else { "done_with_errors".into() };
    st.step.clear();
    st.message = match (st.sites.len(), failed) {
        (0, _) => "Built. There are no sites to migrate yet.".into(),
        (n, 0) => format!("All {n} site(s) restarted and migrated."),
        (n, f) => format!("{f} of {n} site(s) did not start on the new build."),
    };
    st.updated_at = pro_core::now_iso();
    st.finished_at = st.updated_at.clone();
    save_status(state, st)?;
    write(
        &dir(state).join("current.json"),
        &Installed { commit: st.commit.clone(), repository: st.repository.clone(), branch: st.branch.clone(), installed_at: st.finished_at.clone() },
        false,
    )?;
    pro_db::audit::record(
        &state.db,
        "system",
        "platform.updated",
        None,
        None,
        serde_json::json!({"commit": st.commit, "previous": st.previous_commit, "sites": st.sites.len(), "failed": failed}),
    )
    .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// what GitHub has
// ---------------------------------------------------------------------------

struct Upstream {
    commit: String,
    title: String,
    date: String,
    /// Commits on GitHub since the installed one, newest first; `None` when
    /// nothing is recorded as installed.
    ahead: Option<Vec<(String, String)>>,
}

async fn upstream(state: &AppState, installed: &Installed) -> std::result::Result<Upstream, String> {
    let live = state.live();
    let repo = live.updates_repository();
    let branch = live.updates_branch();
    if !valid_repository(&repo) || !valid_branch(&branch) {
        return Err("Set the repository and branch in Settings → Updates.".into());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .user_agent("vuuut-pro-updates")
        .build()
        .map_err(|e| e.to_string())?;
    let get = |url: String| {
        let mut req = client.get(url).header("accept", "application/vnd.github+json");
        if let Some(token) = live.updates_token() {
            req = req.bearer_auth(token);
        }
        req
    };
    let res = get(format!("https://api.github.com/repos/{repo}/commits/{branch}")).send().await.map_err(|e| format!("GitHub did not answer: {e}"))?;
    match res.status().as_u16() {
        200 => {}
        401 | 403 => return Err("GitHub refused the token. Check it in Settings → Updates.".into()),
        404 => return Err(format!("GitHub has no branch {branch} in {repo} that this token can see. A private repository needs a token in Settings → Updates.")),
        code => return Err(format!("GitHub answered {code}.")),
    }
    let head: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    let commit = head["sha"].as_str().unwrap_or_default().to_string();
    let message = head["commit"]["message"].as_str().unwrap_or_default();
    let ahead = if installed.commit.is_empty() || installed.commit == commit {
        (!installed.commit.is_empty()).then(Vec::new)
    } else {
        let cmp: Option<serde_json::Value> = match get(format!("https://api.github.com/repos/{repo}/compare/{}...{commit}", installed.commit)).send().await {
            Ok(r) if r.status().is_success() => r.json().await.ok(),
            _ => None,
        };
        cmp.map(|c| {
            let mut list: Vec<(String, String)> = c["commits"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|x| (x["sha"].as_str().unwrap_or_default().to_string(), x["commit"]["message"].as_str().unwrap_or_default().lines().next().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            list.reverse();
            list
        })
    };
    Ok(Upstream {
        commit,
        title: message.lines().next().unwrap_or_default().to_string(),
        date: head["commit"]["committer"]["date"].as_str().unwrap_or_default().to_string(),
        ahead,
    })
}

// ---------------------------------------------------------------------------
// the page
// ---------------------------------------------------------------------------

pub async fn page(State(state): State<AppState>, headers: HeaderMap, Query(query): Query<HashMap<String, String>>) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_page_or_signin("/owner/signin", &chrome, page_inner(&state, chrome.theme, &headers, &query).await)
}

fn log_tail(state: &AppState, lines: usize) -> String {
    let text = std::fs::read_to_string(dir(state).join("update.log")).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

async fn page_inner(state: &AppState, theme: Theme, headers: &HeaderMap, query: &HashMap<String, String>) -> Result<Response> {
    let member = auth::staff_with(state, headers, StaffRole::Owner).await?;
    let st = status(state);
    let inst = installed(state);
    let live = state.live();
    // GitHub is asked only on request: a page that waited on an API for
    // every visit would be slow, and would fail whenever GitHub does.
    let checking = query.contains_key("check");
    let upstream = if checking { Some(upstream(state, &inst).await) } else { None };

    let row = |icon: &str, tint: &str, title: &str, detail: &str| {
        format!(
            r#"<div class="row"><span class="ic tint-{tint}">{icon}</span><div class="body"><span class="title">{title}</span><span class="detail">{detail}</span></div></div>"#,
            icon = view::svg(icon),
        )
    };
    let installed_row = if inst.commit.is_empty() {
        row("history", "slate", "Not recorded", "Installed by hand before updates were managed here. The first update from this page records it.")
    } else {
        row(
            "check",
            "green",
            &format!("<code>{}</code>", esc(short(&inst.commit))),
            &format!("Installed {} from {} · {}", esc(&inst.installed_at), esc(&inst.repository), esc(&inst.branch)),
        )
    };
    let (upstream_row, news) = match &upstream {
        None => (
            row("globe", "blue", r#"<a href="/owner/updates?check=1">Check for updates</a>"#, &format!(r#"Compares {} · {} on GitHub with what the sites run · <a href="/owner/settings/updates">Change</a>"#, esc(&live.updates_repository()), esc(&live.updates_branch()))),
            String::new(),
        ),
        Some(Ok(u)) => {
            let news = match &u.ahead {
                Some(list) if list.is_empty() => view::banner(BannerKind::Ok, "The sites are on the latest commit."),
                Some(list) => format!(
                    r#"{}<div class="card rows">{}</div>"#,
                    view::section(&format!("{} new commit(s)", list.len())),
                    list.iter().take(15).map(|(sha, msg)| row("info", "blue", &esc(msg), &format!("<code>{}</code>", esc(short(sha))))).collect::<String>()
                ),
                None => String::new(),
            };
            (
                row("globe", "blue", &format!("<code>{}</code> {}", esc(short(&u.commit)), esc(&u.title)), &format!("Latest on {} · {} · {}", esc(&live.updates_repository()), esc(&live.updates_branch()), esc(&u.date))),
                news,
            )
        }
        Some(Err(e)) => (row("warn", "amber", "Cannot read GitHub", &esc(e)), String::new()),
    };

    let action = if st.busy() {
        r#"<p class="muted">An update is running. This page refreshes itself until it is done.</p>"#.to_string()
    } else {
        view::action_button(
            "/owner/updates/run",
            "Update now",
            "btn",
            Some("Update Vuuut now?|Fetches the branch from GitHub, builds it on this server, then restarts every site one at a time so each applies its database migrations. Each site is unavailable for a few seconds while it restarts."),
        )
    };

    let progress = if st.state.is_empty() {
        String::new()
    } else {
        let (label, kind) = st.label();
        let sites: String = st
            .sites
            .iter()
            .map(|s| format!(r#"<tr><td>{}</td><td>{}</td><td>{}</td></tr>"#, esc(&s.slug), view::status_pill(if s.ok { "Migrated" } else { "Failed" }, if s.ok { PillKind::Ok } else { PillKind::Bad }), esc(&s.detail)))
            .collect();
        let log = log_tail(state, 80);
        format!(
            r#"{section}<div class="card padded">
<p>{pill} {step}</p>
{message}
<p class="muted">Requested by {by} at {at}{commit}{finished}</p>
{sites}
{log}
</div>"#,
            section = view::section("Last update"),
            pill = view::status_pill(label, kind),
            step = esc(&st.step),
            message = if st.message.is_empty() { String::new() } else { format!("<p>{}</p>", esc(&st.message)) },
            by = esc(&st.requested_by),
            at = esc(&st.requested_at),
            commit = if st.commit.is_empty() { String::new() } else { format!(" · commit <code>{}</code>", esc(short(&st.commit))) },
            finished = if st.finished_at.is_empty() { String::new() } else { format!(" · finished {}", esc(&st.finished_at)) },
            sites = if sites.is_empty() { String::new() } else { format!(r#"<table class="table"><thead><tr><th>Site</th><th>Result</th><th>Detail</th></tr></thead><tbody>{sites}</tbody></table>"#) },
            log = if log.is_empty() { String::new() } else { format!(r#"<details><summary>Build log</summary><pre style="max-height:420px;overflow:auto;font-size:12px;white-space:pre-wrap">{}</pre></details>"#, esc(&log)) },
        )
    };

    // No scripts on these pages; a refresh is how progress moves.
    let refresh = if st.busy() { r#"<meta http-equiv="refresh" content="4">"# } else { "" };
    let body = format!(
        r#"{refresh}{head}{section_version}<div class="card rows">{installed_row}{upstream_row}</div>
{news}{action}{progress}"#,
        head = view::page_head("Updates", "Keep every site on the latest Vuuut: build it here, then migrate each site's database.", ""),
        section_version = view::section("Version"),
    );
    Ok(shell(state, theme, "Updates", Tab::Updates, &member, Flash::from_query(query).as_ref(), &body))
}

pub async fn run(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let chrome = state.chrome(&headers);
    reply::or_redirect(
        "/owner/updates",
        &chrome,
        async {
            auth::require_same_origin(&headers, &state.config.host())?;
            let member = auth::staff_with(&state, &headers, StaffRole::Owner).await?;
            let st = request(&state, &member)?;
            pro_db::audit::record(
                &state.db,
                &format!("staff:{}", member.id),
                "platform.update_requested",
                None,
                None,
                serde_json::json!({"repository": st.repository, "branch": st.branch}),
            )
            .await;
            Ok(redirect_with("/owner/updates", Flash::ok("Update started.")))
        }
        .await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_repository_and_branch_names_reach_the_script() {
        assert!(valid_repository("anisulkibria/rust-ghost"));
        assert!(valid_repository("a.b/c_d-1"));
        for bad in ["", "x", "a/b/c", "../x", "a/..", "a b/c", "a/b;rm", "https://github.com/a/b"] {
            assert!(!valid_repository(bad), "{bad}");
        }
        assert!(valid_branch("main"));
        assert!(valid_branch("release/1.2"));
        for bad in ["", "-x", "a..b", "a b", "a;b", "/a", "a/", "$(x)", "a//b"] {
            assert!(!valid_branch(bad), "{bad}");
        }
    }

    #[test]
    fn a_finished_or_failed_update_frees_the_button() {
        for s in ["requested", "fetching", "syncing", "building", "built", "rolling_out"] {
            assert!(Status { state: s.into(), ..Default::default() }.busy(), "{s}");
        }
        for s in ["", "done", "done_with_errors", "failed"] {
            assert!(!Status { state: s.into(), ..Default::default() }.busy(), "{s}");
        }
    }
}
