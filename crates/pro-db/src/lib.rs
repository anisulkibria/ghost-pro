//! `pro-db` — the control plane's database.
//!
//! One database holding what the *platform* knows: that a site exists, who
//! pays for it, which hostnames reach it. It holds nothing a tenant publishes.
//! Each tenant's posts, members and staff live in that tenant's own Ghost
//! database, which this crate never opens.
//!
//! That boundary is the main structural decision in the whole platform, and it
//! is worth stating plainly: a bug in this crate can suspend a site or
//! misroute a domain, but it cannot leak one customer's drafts to another,
//! because the data is not here to leak.
//!
//! ## Two engines, one set of queries
//!
//! The same build runs on SQLite — a file, the local default, needing no setup
//! — and on MySQL, which is what lets a production platform be served from
//! more than one machine. Which one is decided by
//! [`pro_core::Config::db_target`] from a URL, at startup, and nothing in this
//! crate's query code knows the answer.
//!
//! That works because the schema was already portable in the ways that matter:
//! every timestamp is an ISO-8601 `TEXT` string, every id is generated in Rust
//! rather than by the database, and both engines take `?` placeholders. The
//! three constructs where the engines genuinely disagree live in
//! [`dialect`], and the pragmas and session settings each one needs are in
//! [`pool`]. There is no third place.
//!
//! ## Module map
//!
//! | Module | Holds |
//! |---|---|
//! | [`pool`] | opening and migrating the database, on either engine |
//! | [`dialect`] | the handful of statements the two engines spell differently |
//! | [`unique`] | recognising a duplicate-key error, whichever engine raised it |
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
mod click;
pub mod audit;
pub mod backups;
pub mod dialect;
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
pub mod email_usage;

pub use pool::{open, open_in_memory, Db};
pub use settings::SettingsStore;
