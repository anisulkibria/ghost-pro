//! `pro-db` — the control plane's database.
//!
//! One SQLite file holding what the *platform* knows: that a site exists, who
//! pays for it, which hostnames reach it. It holds nothing a tenant publishes.
//! Each tenant's posts, members and staff live in that tenant's own Ghost
//! database, which this crate never opens.
//!
//! That boundary is the main structural decision in the whole platform, and it
//! is worth stating plainly: a bug in this crate can suspend a site or
//! misroute a domain, but it cannot leak one customer's drafts to another,
//! because the data is not here to leak.
//!
//! ## Module map
//!
//! | Module | Holds |
//! |---|---|
//! | [`pool`] | opening and migrating the database |
//! | [`settings`] | the operator's live settings, which beat the config file |
//! | [`staff`] | the operator's own logins and their roles |
//! | [`accounts`] | paying customers |
//! | [`sessions`] | signed-in state, kept strictly separate per population |
//! | [`tokens`] | verification, reset and invite links |
//! | [`tenants`] | sites, their ports and their lifecycle |
//! | [`domains`] | hostnames and their verification state |
//! | [`subscriptions`] | platform billing, normalised across providers |
//! | [`jobs`] | durable background work |
//! | [`audit`] | the audit trail and webhook idempotency |
//! | [`usage`] | sampled per-tenant usage, for quotas and charts |
//! | [`backups`] | snapshot archives, kept a few per tenant |
pub mod accounts;
pub mod audit;
pub mod backups;
pub mod domains;
pub mod jobs;
pub mod pool;
pub mod sessions;
pub mod settings;
pub mod staff;
pub mod subscriptions;
pub mod tenants;
pub mod tokens;
pub mod unique;
pub mod usage;

pub use pool::{open, open_in_memory};
pub use settings::SettingsStore;
pub use sqlx::SqlitePool;
