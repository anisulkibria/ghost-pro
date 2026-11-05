//! Billing webhooks.
//!
//! One endpoint per provider, all following the same five steps, in this
//! order, for reasons each worth stating:
//!
//! 1. **Verify the signature over the raw body.** Before parsing, because
//!    re-serialised JSON does not match the signature.
//! 2. **Claim the event id.** Before acting, because every provider retries
//!    and acting twice grants a second subscription.
//! 3. **Interpret** it into a provider-neutral [`pro_billing::BillingEvent`].
//! 4. **Apply** it: update the subscription, and the tenant's plan if it
//!    changed.
//! 5. **Answer 200 even for an event we ignored.** A non-2xx makes the
//!    provider retry, and retrying something we deliberately do not handle is
//!    a loop that never ends.
//!
//! The one case that must *not* answer 200 is a failure we want retried — a
//! database blip. There, the claim is released so the provider's next attempt
//! is treated as fresh.
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use pro_billing::{BillingEvent, Provider, SubscriptionState, WebhookHeaders};
use pro_core::{ProError, Result};

use crate::reply;
use crate::state::AppState;

/// `POST /webhooks/:provider`
///
/// The query string is read as well as the headers, for a provider that signs
/// nothing: Gumroad's pings are authenticated by a token the operator puts in
/// the ping URL itself. See `pro_billing::WebhookHeaders::shared_secret`.
pub async fn receive(
    State(state): State<AppState>,
    Path(provider_name): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match handle(&state, &provider_name, &query, &headers, &body).await {
        Ok(response) => response,
        Err(e) => {
            // A verification failure is logged loudly: it is either a
            // misconfiguration or somebody trying to forge a billing event,
            // and an operator needs to be able to tell which.
            if e.status() == 403 || e.status() == 400 {
                tracing::warn!("rejected a {provider_name} webhook: {e}");
            } else {
                tracing::error!("failed to process a {provider_name} webhook: {e}");
            }
            reply::json_error(&e)
        }
    }
}

async fn handle(
    state: &AppState,
    provider_name: &str,
    query: &std::collections::HashMap<String, String>,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response> {
    let live = state.live();

    // Built by *name*, not from the active provider: events keep arriving
    // from a provider long after new signups have moved elsewhere, and
    // dropping them would mean missing renewals for every existing customer.
    let provider = pro_billing::build(provider_name, &live).ok_or_else(|| {
        ProError::NotFound(format!("{provider_name} is not configured on this platform"))
    })?;

    let header_bag = WebhookHeaders {
        stripe_signature: header(headers, "stripe-signature"),
        paddle_signature: header(headers, "paddle-signature"),
        x_signature: header(headers, "x-signature"),
        // `?token=…`, which is all Gumroad can be authenticated by. Named
        // `token` rather than `secret` because it is what an operator types
        // into Gumroad's ping field and sees in their own browser history.
        shared_secret: query.get("token").cloned(),
    };

    // (1) Over the raw bytes, before anything parses them.
    provider.verify_webhook(body, &header_bag)?;

    // (1a) Only now is the body parsed, and by the provider — Gumroad posts
    // form encoding rather than JSON.
    let parsed = provider.parse_body(body)?;

    let event_id = provider.event_id(&parsed);
    let event_type = event_type_of(provider_name, &parsed);

    // (2) Claim before acting.
    let claim = pro_db::audit::claim_webhook(
        &state.db,
        provider_name,
        &event_id,
        &event_type,
        &String::from_utf8_lossy(body),
    )
    .await?;
    if claim == pro_db::audit::WebhookClaim::Duplicate {
        tracing::debug!("{provider_name} event {event_id} was already handled");
        return Ok(reply::json(
            StatusCode::OK,
            serde_json::json!({"status": "already_processed"}),
        ));
    }

    // (3) + (4)
    let outcome = apply(state, provider.as_ref(), &parsed).await;

    match outcome {
        Ok(summary) => {
            let _ = pro_db::audit::finish_webhook(&state.db, provider_name, &event_id, None).await;
            Ok(reply::json(
                StatusCode::OK,
                serde_json::json!({"status": "ok", "handled": summary}),
            ))
        }
        // Worth retrying: let the provider's next attempt through by giving up
        // the claim. Without this, a momentary database failure would
        // permanently swallow a subscription change.
        Err(e) if e.status() >= 500 || e.status() == 503 => {
            let _ = pro_db::audit::release_webhook(&state.db, provider_name, &event_id).await;
            Err(e)
        }
        // Not worth retrying: record why and accept it, so the provider stops
        // resending something that will never succeed. It shows up in the
        // console's Activity screen for an operator to look at.
        Err(e) => {
            tracing::error!("{provider_name} event {event_id} could not be applied: {e}");
            let _ = pro_db::audit::finish_webhook(
                &state.db,
                provider_name,
                &event_id,
                Some(&e.to_string()),
            )
            .await;
            Ok(reply::json(
                StatusCode::OK,
                serde_json::json!({"status": "accepted_with_error"}),
            ))
        }
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// The event's own name, for the idempotency record. Each provider puts it
/// somewhere different.
fn event_type_of(provider: &str, body: &serde_json::Value) -> String {
    let field = match provider {
        "stripe" => body["type"].as_str(),
        "paddle" => body["event_type"].as_str(),
        "lemonsqueezy" => body.pointer("/meta/event_name").and_then(|v| v.as_str()),
        _ => None,
    };
    field.unwrap_or("unknown").to_string()
}

/// Act on an interpreted event.
/// Re-run one stored event, on an operator's say-so.
///
/// The signature is not re-checked: this is the same bytes that passed it on
/// arrival, kept verbatim for exactly this purpose, and an operator replaying
/// an event is not an occasion to re-litigate whether the provider sent it.
/// The claim is not re-taken either — it is already ours.
///
/// Returns the same summary the live path produced, so the operator sees what
/// the replay did rather than a bare "done".
pub async fn replay_event(
    state: &AppState,
    provider_name: &str,
    event_id: &str,
) -> Result<String> {
    let live = state.live();
    let provider = pro_billing::build(provider_name, &live).ok_or_else(|| {
        ProError::NotFound(format!("{provider_name} is not configured on this platform"))
    })?;

    let payload = pro_db::audit::webhook_payload(&state.db, provider_name, event_id)
        .await?
        .ok_or_else(|| ProError::NotFound("No such webhook event.".into()))?;
    let parsed: serde_json::Value = serde_json::from_str(&payload)
        .map_err(|e| ProError::BadRequest(format!("stored webhook body is not JSON: {e}")))?;

    match apply(state, provider.as_ref(), &parsed).await {
        Ok(summary) => {
            let _ = pro_db::audit::finish_webhook(&state.db, provider_name, event_id, None).await;
            pro_db::audit::record(
                &state.db,
                "console",
                "webhook.replayed",
                None,
                None,
                serde_json::json!({"provider": provider_name, "event": event_id}),
            )
            .await;
            Ok(summary)
        }
        Err(e) => {
            let _ = pro_db::audit::finish_webhook(
                &state.db,
                provider_name,
                event_id,
                Some(&e.to_string()),
            )
            .await;
            Err(e)
        }
    }
}

async fn apply(
    state: &AppState,
    provider: &dyn Provider,
    parsed: &serde_json::Value,
) -> Result<String> {
    let live = state.live();
    let event = provider.interpret(parsed, &live.plans())?;

    match event {
        BillingEvent::Ignored { event_type } => Ok(format!("ignored {event_type}")),

        BillingEvent::PaymentFailed { provider_subscription_id } => {
            let Some(subscription) = pro_db::subscriptions::find_by_provider_id(
                &state.db,
                provider.name(),
                &provider_subscription_id,
            )
            .await?
            else {
                // A payment for a subscription this platform has no record of:
                // worth noting, not worth retrying.
                return Ok(format!("no subscription {provider_subscription_id}"));
            };

            pro_db::subscriptions::upsert(
                &state.db,
                pro_db::subscriptions::Upsert {
                    account_id: &subscription.account_id,
                    provider: provider.name(),
                    provider_subscription_id: Some(&provider_subscription_id),
                    status: Some("past_due"),
                    ..Default::default()
                },
            )
            .await?;

            // Told once, now, with the date their site would go offline. The
            // suspension itself happens when the grace period expires.
            if let Ok(account) = pro_db::accounts::get(&state.db, &subscription.account_id).await {
                let _ = state
                    .mailer()
                    .send(pro_mail::templates::payment_failed(
                        &live.platform_name(),
                        &account.email,
                        &account.name,
                        &state.config.billing_url(),
                        live.dunning_grace_days(),
                    ))
                    .await;
            }

            pro_db::audit::record(
                &state.db,
                &format!("webhook:{}", provider.name()),
                "subscription.payment_failed",
                subscription.tenant_id.as_deref(),
                Some(&subscription.account_id),
                serde_json::json!({"subscription": provider_subscription_id}),
            )
            .await;
            Ok("payment failed".into())
        }

        BillingEvent::PaymentSucceeded { provider_subscription_id } => {
            let Some(subscription) = pro_db::subscriptions::find_by_provider_id(
                &state.db,
                provider.name(),
                &provider_subscription_id,
            )
            .await?
            else {
                return Ok(format!("no subscription {provider_subscription_id}"));
            };

            pro_db::subscriptions::upsert(
                &state.db,
                pro_db::subscriptions::Upsert {
                    account_id: &subscription.account_id,
                    provider: provider.name(),
                    provider_subscription_id: Some(&provider_subscription_id),
                    status: Some("active"),
                    ..Default::default()
                },
            )
            .await?;

            // A payment arriving is what brings a suspended site back, and it
            // has to happen here rather than on a timer — a customer who has
            // just paid expects their site back now.
            if let Some(tenant_id) = &subscription.tenant_id {
                if let Ok(tenant) = pro_db::tenants::get(&state.db, tenant_id).await {
                    // Billing's own suspensions only — a site an operator shut
                    // for abuse must not reopen because a card was charged.
                    // This is also what opens a site created on a
                    // payment-first platform: its suspension is
                    // `AWAITING_PAYMENT`, and this is the payment.
                    if tenant.is_suspended_for_billing() {
                        if let Some(plan) = live.plan(&tenant.plan_slug) {
                            pro_provision::unsuspend_site(
                                &state.db,
                                &state.config,
                                &state.supervisor,
                                &tenant,
                                &plan,
                            )
                            .await?;
                            pro_db::audit::record(
                                &state.db,
                                &format!("webhook:{}", provider.name()),
                                "tenant.unsuspended",
                                Some(&tenant.id),
                                Some(&subscription.account_id),
                                serde_json::json!({"reason": "payment received"}),
                            )
                            .await;
                        }
                    }
                    // A renewal resets the window periodic limits count from,
                    // so the email allowance refreshes on the customer's own
                    // billing anniversary.
                    let _ = pro_db::tenants::set_period_start(
                        &state.db,
                        &tenant.id,
                        &pro_core::now_iso(),
                    )
                    .await;
                }
            }
            Ok("payment succeeded".into())
        }

        BillingEvent::SubscriptionChanged(incoming) => {
            apply_subscription(state, provider, incoming).await
        }
    }
}

/// Record a subscription change, and move the tenant's plan if it moved.
async fn apply_subscription(
    state: &AppState,
    provider: &dyn Provider,
    incoming: SubscriptionState,
) -> Result<String> {
    let live = state.live();

    // Which account this belongs to. The metadata set at checkout is the
    // first choice; the provider's customer id is the fallback, which is what
    // makes a renewal years later still resolvable even if metadata was lost.
    let account_id = match &incoming.account_id {
        Some(id) if pro_db::accounts::get(&state.db, id).await.is_ok() => id.clone(),
        _ => {
            // Already known under this provider's subscription id?
            if let Some(existing) = pro_db::subscriptions::find_by_provider_id(
                &state.db,
                provider.name(),
                &incoming.provider_subscription_id,
            )
            .await?
            {
                existing.account_id
            } else if let Some(customer) = &incoming.provider_customer_id {
                match pro_db::accounts::find_by_customer(&state.db, provider.name(), customer)
                    .await?
                {
                    Some(account) => account.id,
                    None => {
                        return Err(ProError::BadRequest(format!(
                            "subscription {} names no account this platform knows",
                            incoming.provider_subscription_id
                        )))
                    }
                }
            } else {
                return Err(ProError::BadRequest(format!(
                    "subscription {} carries no account reference",
                    incoming.provider_subscription_id
                )));
            }
        }
    };

    // Remember the customer id, so later events resolve even without metadata.
    if let Some(customer) = &incoming.provider_customer_id {
        let _ = pro_db::accounts::set_billing_customer(
            &state.db,
            &account_id,
            provider.name(),
            customer,
        )
        .await;
    }

    // Which site this pays for. A checkout started before the site existed
    // carries no tenant; in that case the account's only site is the
    // unambiguous answer, and more than one is left for an operator rather
    // than guessed at.
    let tenant_id = match &incoming.tenant_id {
        Some(id) => Some(id.clone()),
        None => {
            let existing = pro_db::subscriptions::find_by_provider_id(
                &state.db,
                provider.name(),
                &incoming.provider_subscription_id,
            )
            .await?;
            match existing.and_then(|s| s.tenant_id) {
                Some(id) => Some(id),
                None => {
                    let sites =
                        pro_db::tenants::list_for_account(&state.db, &account_id).await?;
                    if sites.len() == 1 {
                        Some(sites[0].id.clone())
                    } else {
                        None
                    }
                }
            }
        }
    };

    let stored = pro_db::subscriptions::upsert(
        &state.db,
        pro_db::subscriptions::Upsert {
            account_id: &account_id,
            tenant_id: tenant_id.as_deref(),
            plan_slug: incoming.plan_slug.as_deref(),
            provider: provider.name(),
            provider_subscription_id: Some(&incoming.provider_subscription_id),
            provider_customer_id: incoming.provider_customer_id.as_deref(),
            status: Some(&incoming.status),
            current_period_start: incoming.current_period_start.as_deref(),
            current_period_end: incoming.current_period_end.as_deref(),
            trial_ends_at: incoming.trial_ends_at.as_deref(),
            cancel_at_period_end: incoming.cancel_at_period_end,
            canceled_at: incoming.canceled_at.as_deref(),
        },
    )
    .await?;

    let mut summary = format!("subscription {}", stored.status);
    if let Some(tenant_id) = &tenant_id {
        if let Ok(tenant) = pro_db::tenants::get(&state.db, tenant_id).await {
            // Apply the plan to the site, if the subscription names one and it
            // differs.
            if let Some(plan_slug) = &incoming.plan_slug {
                if &tenant.plan_slug != plan_slug && stored.is_entitled() {
                    if let Some(plan) = live.plan(plan_slug) {
                        pro_provision::apply_plan(
                            &state.db,
                            &state.config,
                            &state.supervisor,
                            &tenant,
                            &plan,
                        )
                        .await?;
                        summary = format!("moved {} to {}", tenant.slug, plan.name);
                    }
                }
            }

            // An entitling subscription opens a site billing had closed: a
            // lapsed one that has now been paid for, or — on a platform that
            // takes payment before the site opens — one created suspended and
            // waiting for its first payment. Billing's own suspensions only,
            // so an operator's survives being paid for.
            //
            // This cannot wait for `PaymentSucceeded`. Stripe's
            // `checkout.session.completed` arrives as a subscription change,
            // not a payment, so a site that opened only on the payment event
            // would sit closed until an invoice event happened to follow.
            if stored.is_entitled() && tenant.is_suspended_for_billing() {
                let was_awaiting = tenant.suspended_reason.as_deref()
                    == Some(pro_core::suspension::AWAITING_PAYMENT);
                // The plan this is paying for, which on a plan change is the
                // incoming one rather than the row's.
                let slug = incoming.plan_slug.as_deref().unwrap_or(&tenant.plan_slug);
                if let Some(plan) = live.plan(slug) {
                    pro_provision::unsuspend_site(
                        &state.db,
                        &state.config,
                        &state.supervisor,
                        &tenant,
                        &plan,
                    )
                    .await?;
                    summary = format!("opened {}", tenant.slug);
                    pro_db::audit::record(
                        &state.db,
                        &format!("webhook:{}", provider.name()),
                        "tenant.unsuspended",
                        Some(&tenant.id),
                        Some(&account_id),
                        serde_json::json!({"reason": "subscription entitled"}),
                    )
                    .await;

                    // The site creation deliberately sent no "ready" mail,
                    // because it was not. This is the moment it is.
                    if was_awaiting {
                        if let Ok(account) =
                            pro_db::accounts::get(&state.db, &account_id).await
                        {
                            let _ = state
                                .mailer()
                                .send(pro_mail::templates::site_ready(
                                    &live.platform_name(),
                                    &account.email,
                                    &account.name,
                                    &state.config.tenant_url(&tenant.slug),
                                ))
                                .await;
                        }
                    }
                }
            }

            // A subscription that has stopped entitling takes the site
            // offline — but only once, and only after the grace period for a
            // *failed payment*. An outright cancellation is immediate,
            // because the customer asked for it.
            if !stored.is_entitled() && tenant.status == pro_core::TenantStatus::Active {
                pro_provision::suspend_site(
                    &state.db,
                    &state.config,
                    &state.supervisor,
                    &tenant,
                    pro_core::suspension::SUBSCRIPTION_ENDED,
                )
                .await?;
                summary = format!("suspended {}", tenant.slug);

                if let Ok(account) = pro_db::accounts::get(&state.db, &account_id).await {
                    let _ = state
                        .mailer()
                        .send(pro_mail::templates::site_suspended(
                            &live.platform_name(),
                            &account.email,
                            &account.name,
                            &state.config.billing_url(),
                        ))
                        .await;
                }
            }
        }
    }

    pro_db::audit::record(
        &state.db,
        &format!("webhook:{}", provider.name()),
        "subscription.updated",
        tenant_id.as_deref(),
        Some(&account_id),
        serde_json::json!({
            "status": stored.status,
            "plan": stored.plan_slug,
            "subscription": incoming.provider_subscription_id,
        }),
    )
    .await;

    Ok(summary)
}
