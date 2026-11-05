//! `pro-provision` — creating, reconfiguring and removing tenant sites.
//!
//! This is the crate that turns "a customer clicked Create site" into a
//! running Ghost. The sequence is deliberately ordered so that every failure
//! leaves something an operator can see and retry, and nothing is charged for
//! until it exists:
//!
//! 1. claim the slug and a port (one transaction — [`pro_db::tenants::create`])
//! 2. build the directory tree and copy in the themes
//! 3. write `config.<env>.json`, including the plan's `hostSettings`
//! 4. start the process and wait for it to listen
//! 5. complete the site's own setup over loopback, through the CMS's own API
//! 6. mark the tenant active
//!
//! Step 5 is worth dwelling on. The platform does **not** write into the
//! tenant's database to create its owner user. It calls
//! `POST /ghost/api/admin/authentication/setup` on the tenant's own loopback
//! port — the same endpoint a human would hit in a browser. That endpoint
//! refuses once setup is complete, so it cannot be replayed, and using it
//! means the platform needs no knowledge of the CMS's user schema. When
//! Ghost-rust changes how users are stored, this keeps working.
pub mod backup;
pub mod config_file;
mod lifecycle;
mod storage;

pub use lifecycle::{
    apply_plan, create_site, delete_site_files, restore_site, suspend_site, unsuspend_site,
    update_primary_domain, NewSite,
};
pub use backup::{pack, snapshot_database, unpack};
pub use storage::{measure_tenant, tenant_dir_size};
