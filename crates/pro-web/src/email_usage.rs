use crate::{view, AppState};
use pro_core::{ProError, Result, Tenant};
use serde_json::Value;

pub(crate) async fn fetch(state: &AppState, tenant: &Tenant, wake: bool) -> Result<Value> {
    if wake {
        state.supervisor.ensure_running(tenant).await?;
    }
    let (Some(id), Some(secret)) = (&tenant.platform_key_id, &tenant.platform_key_secret) else {
        return Err(ProError::Unavailable(
            "This site has no platform integration key for email reporting.".into(),
        ));
    };
    let token = crate::worker::mint_admin_token(id, secret)
        .ok_or_else(|| ProError::Unavailable("The site's reporting key is invalid.".into()))?;
    let response = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build()
        .map_err(ProError::internal)?.get(format!("http://127.0.0.1:{}/ghost/api/admin/email_usage/",tenant.port))
        .header("Authorization",format!("Ghost {token}")).send().await
        .map_err(|_| ProError::Unavailable("Could not reach this site's email report. Check the site is running the current Vuuut binary.".into()))?;
    if !response.status().is_success() {
        return Err(ProError::Unavailable("This site's email report is unavailable. Update Vuuut and check the platform integration key.".into()));
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|_| ProError::Unavailable("The site returned an invalid email report.".into()))?;
    let report = payload
        .get("email_usage")
        .filter(|v| v["sent"].is_i64())
        .cloned()
        .ok_or_else(|| {
            ProError::Unavailable("The site returned an invalid email report.".into())
        })?;
    pro_db::email_usage::record(&state.db, &tenant.id, &report).await?;
    Ok(report)
}

pub(crate) async fn card(state: &AppState, tenant: &Tenant, can_write: bool) -> Result<String> {
    if state.supervisor.is_running(&tenant.id) {
        let _ = fetch(state, tenant, false).await;
    }
    let sample = pro_db::email_usage::latest(&state.db, &tenant.id).await?;
    let report = sample.as_ref().map(|s| s.0.clone()).unwrap_or(Value::Null);
    let policy = pro_db::email_usage::policy(&state.db, &tenant.id).await?;
    let global = state.live().mail().auto_stop_at_limit;
    let auto_stop = policy.unwrap_or(global);
    let limit = report["limit"].as_i64().unwrap_or_else(|| {
        state
            .live()
            .plan(&tenant.plan_slug)
            .map(|p| p.email_allowance())
            .unwrap_or(0)
    });
    let number = |key: &str| {
        report[key]
            .as_i64()
            .map(view::number)
            .unwrap_or_else(|| "—".into())
    };
    let percent = report["sent"]
        .as_i64()
        .map(|n| {
            if limit > 0 {
                ((n + report["pending"].as_i64().unwrap_or(0)).saturating_mul(100) / limit)
                    .clamp(0, 100)
            } else {
                100
            }
        })
        .unwrap_or(0);
    let status = if tenant.status == pro_core::TenantStatus::Suspended {
        "Site suspended — sending disabled by its plan"
    } else if (report["stopped"].as_bool().unwrap_or(false)
        || report["sent"].as_i64().is_some_and(|n| n >= limit))
        && auto_stop
    {
        "Sending stopped at allowance"
    } else if auto_stop {
        "Auto-stop enabled"
    } else {
        "Continue beyond allowance"
    };
    let dates = match (
        &report["period_start"].as_str(),
        &report["period_end"].as_str(),
    ) {
        (Some(start), Some(end)) => format!(
            "Period: {} to {}. Allowance resets automatically.",
            view::when(Some(start)),
            view::when(Some(end))
        ),
        _ => "Refresh usage to collect this site's current monthly period.".into(),
    };
    let freshness = sample
        .as_ref()
        .map(|s| format!("Last updated {}.", view::when(Some(&s.1))))
        .unwrap_or_else(|| {
            "No email report collected yet. Refresh usage to start this site and read its report."
                .into()
        });
    let select = [
        ("inherit", "Use platform default"),
        ("stop", "Stop at allowance"),
        ("continue", "Continue beyond allowance"),
    ]
    .iter()
    .map(|(value, label)| {
        let selected = match policy {
            None => *value == "inherit",
            Some(true) => *value == "stop",
            Some(false) => *value == "continue",
        };
        format!(
            r#"<option value="{value}"{}>{label}</option>"#,
            view::flag("selected", selected)
        )
    })
    .collect::<String>();
    let action = if can_write {
        format!(
            r#"<form method="post" action="/console/sites/{id}/email-usage"><button class="btn outlined small" type="submit">Refresh usage</button></form>"#,
            id = view::attr(&tenant.id)
        )
    } else {
        String::new()
    };
    let mut body = view::section("Email sending");
    body.push_str(&format!(r#"<div class="card padded"><div class="toggle-row"><div class="label"><b>{status}</b><span>{dates}</span></div>{action}</div>
<div class="tiles c3" style="margin-top:16px">{sent}{allowance}{remaining}</div>
<div class="bar {full}" role="progressbar" aria-label="Monthly email allowance used" aria-valuenow="{percent}" aria-valuemin="0" aria-valuemax="100"><span style="width:{percent}%"></span></div>
<p class="hint">{freshness} Newsletter recipients and transactional messages share one monthly allowance. Failed or blocked sends do not use the allowance.</p>
<form method="post" action="/console/sites/{id}/email-policy" style="margin-top:20px"><div class="inline-form">{policy}<button class="btn small" type="submit"{disabled}>Save sending policy</button></div>
<p class="hint">Platform default: {default}. Turning auto-stop off keeps counting and permits sends above the plan allowance. Policy changes restart running sites.</p></form></div>"#,
        dates=view::esc(&dates),freshness=view::esc(&freshness),
        sent=view::metric("Sent this period",&number("sent"),None),allowance=view::metric("Monthly allowance",&view::number(limit),None),remaining=view::metric("Remaining",&number("remaining"),None),
        full=if percent>=100 {"full"}else{""},id=view::attr(&tenant.id),policy=view::select("policy","Auto-stop",&select,None,!can_write),disabled=view::flag("disabled",!can_write),default=if global {"stop at allowance"}else{"continue beyond allowance"},
    ));
    if !report.is_null() {
        body.push_str(&format!(
            r#"<div class="tiles">{}{}{}{}</div>"#,
            view::metric("Newsletter sends", &number("newsletter_sent"), None),
            view::metric("Transactional sends", &number("transactional_sent"), None),
            view::metric("Failed attempts", &number("failed"), None),
            view::metric("Blocked attempts", &number("blocked"), None)
        ));
        body.push_str(&view::rows(&view::row(
            "mail",
            "blue",
            "Send accounting",
            &view::esc(&format!(
                "{} known sends in total · {} reserved or awaiting confirmation. {}",
                number("total_sent"),
                number("pending"),
                report["history_note"].as_str().unwrap_or("")
            )),
            "",
        )));
        let daily = report["daily"].as_array().map(|rows| rows.iter().map(|row| format!("<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",view::esc(row["date"].as_str().unwrap_or("")),row["sent"].as_i64().unwrap_or(0),row["failed"].as_i64().unwrap_or(0),row["blocked"].as_i64().unwrap_or(0))).collect::<String>()).filter(|s|!s.is_empty()).unwrap_or_else(||view::empty_row(4,"No email activity in this period."));
        body.push_str(&view::section("Daily email activity"));
        body.push_str(&view::table(
            &["Date", "num:Sent", "num:Failed", "num:Blocked"],
            &daily,
        ));
        let providers = report["providers"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        format!(
                            "<tr><td>{}</td><td class=\"num\">{}</td></tr>",
                            view::esc(row["provider"].as_str().unwrap_or("")),
                            row["sent"].as_i64().unwrap_or(0)
                        )
                    })
                    .collect::<String>()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| view::empty_row(2, "No provider submissions yet."));
        body.push_str(&view::section("Sending providers"));
        body.push_str(&view::table(
            &["Provider", "num:Accepted sends"],
            &providers,
        ));
    }
    Ok(body)
}
