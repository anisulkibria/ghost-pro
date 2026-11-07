//! `pro-supervisor` — starting, stopping and hibernating tenant processes.
//!
//! Every tenant is its own `ghost` process, with its own working directory,
//! its own SQLite database and its own config file. The CMS is used exactly as
//! it ships: nothing here patches it, links against it, or reaches into its
//! internals. That is the single most important property of this design —
//! Vuuut can be rebuilt and dropped in, and the platform keeps working.
//!
//! ## Why a process each
//!
//! The alternative is one process serving every tenant, resolving the right
//! database per request. That would use less memory, and it would mean
//! rewriting the CMS: Vuuut builds its `AppState` once at boot — one
//! pool, one settings cache, one theme engine, one set of host limits — and
//! threading a tenant through all of it is a deep change with a
//! cross-tenant-leak bug behind every missed call site. A process boundary
//! gets the same isolation from the operating system for free, and a leak
//! becomes impossible rather than unlikely.
//!
//! The cost is memory per idle site, and that is what [`hibernation`] is for.
//!
//! ## Hibernation
//!
//! Most sites on a hosting platform are idle most of the time. A tenant with
//! no traffic for `idleTimeoutSecs` is stopped; the next request for it starts
//! it again, which takes a fraction of a second because a Rust Ghost has no
//! runtime to warm up. The visitor waits for the start, and nothing else
//! notices.
//!
//! This is what makes the density practical: a machine holds as many *running*
//! sites as it has memory for, and as many *total* sites as it has disk for.
mod process;

pub use process::{Running, StartOutcome, Supervisor};
