//! Finding the Vuuut checkout tenant sites are run from.
//!
//! The platform spawns one CMS process per tenant, so it needs the `ghost`
//! binary and — for a new site — the themes to seed it with. Both live in a
//! Vuuut checkout, normally the sibling of this one, and `pro.json` records
//! where it was at setup time.
//!
//! A recorded path is a guess about the future, and the future invalidates
//! it: the checkout is renamed, the pair is cloned onto another machine under
//! different names, a deployment directory is copied to a new host, or the
//! directory is simply moved. None of that is a misconfiguration the operator
//! should have to repair by hand — the checkout is still right there, next to
//! this one, under one of a handful of names.
//!
//! So the recorded path is treated as a hint. When it does not resolve, the
//! same search that produced it at setup runs again, and the platform
//! continues with what it finds. This module is that search, in one place,
//! used by `setup` when it writes the config and by `Config` when it reads
//! one back — a single list of names, rather than two that drift.
use std::path::{Path, PathBuf};

/// Directory names a Vuuut checkout is known by, nearest-intent first.
///
/// The project has been renamed more than once, and a clone is named by
/// whoever cloned it. Case matters on Linux even where it does not on macOS,
/// so both spellings of each are listed rather than assuming either.
pub const CHECKOUT_NAMES: [&str; 8] = [
    "vuuut",
    "Vuuut",
    "VUUUT",
    "ghost-rust",
    "Ghost-rust",
    "Ghost-Rust",
    "rust-ghost",
    "Rust-Ghost",
];

/// How far up to look for a sibling checkout.
///
/// `..` is the usual layout — the two repositories side by side, with the
/// deployment directory one level inside this one. The deeper prefixes cover
/// a deployment directory nested further in, which `PRO_DIR` allows.
const PREFIXES: [&str; 4] = ["..", "../..", "../../..", "../../../.."];

/// Does this directory look like a Vuuut checkout?
///
/// Either half is sufficient. The themes are committed, so they are there
/// straight after a clone; the binary appears once it has been built. A
/// directory with the right name and neither is some other directory.
fn looks_like_checkout(dir: &Path) -> bool {
    dir.join("content/themes").is_dir() || dir.join("target/release/ghost").is_file()
}

/// Find a checkout near `base`, returned relative to it.
///
/// Relative deliberately: the whole point is a `pro.json` that survives being
/// cloned onto another machine, and an absolute path baked in at setup time
/// is the one thing that guarantees it will not.
///
/// `None` when no checkout is found, which is not an error — an operator may
/// be setting up before cloning or building Vuuut, and `doctor` says so.
pub fn discover(base: &Path) -> Option<String> {
    for prefix in PREFIXES {
        for name in CHECKOUT_NAMES {
            let rel = format!("{prefix}/{name}");
            if looks_like_checkout(&base.join(&rel)) {
                return Some(rel);
            }
        }
    }
    None
}

/// Find the `ghost` binary near `base`, when a checkout nearby has one built.
pub fn discover_binary(base: &Path) -> Option<String> {
    for prefix in PREFIXES {
        for name in CHECKOUT_NAMES {
            let rel = format!("{prefix}/{name}/target/release/ghost");
            if base.join(&rel).is_file() {
                return Some(rel);
            }
        }
    }
    None
}

/// Find a `content/themes` near `base`.
pub fn discover_themes(base: &Path) -> Option<String> {
    for prefix in PREFIXES {
        for name in CHECKOUT_NAMES {
            let rel = format!("{prefix}/{name}/content/themes");
            if base.join(&rel).is_dir() {
                return Some(rel);
            }
        }
    }
    None
}

/// The checkout a path like `<checkout>/target/release/ghost` belongs to.
///
/// Used to report what to build, and by `run.sh` to build it.
pub fn checkout_of_binary(binary: &Path) -> Option<PathBuf> {
    let checkout = binary.parent()?.parent()?.parent()?;
    (checkout.join("Cargo.toml").is_file() && checkout.join("crates/ghost/Cargo.toml").is_file())
        .then(|| checkout.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_checkout(dir: &Path, name: &str, with_binary: bool) {
        let checkout = dir.join(name);
        std::fs::create_dir_all(checkout.join("content/themes")).unwrap();
        if with_binary {
            std::fs::create_dir_all(checkout.join("target/release")).unwrap();
            std::fs::write(checkout.join("target/release/ghost"), b"#!/bin/sh\n").unwrap();
        }
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pro-checkout-{}", crate::ids::object_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The rename that broke a working deployment: the same checkout, a
    /// different case. Both spellings have to be found.
    #[test]
    fn either_spelling_of_the_name_is_found() {
        for name in ["vuuut", "Vuuut"] {
            let root = scratch().join(name);
            std::fs::create_dir_all(&root).unwrap();
            make_checkout(&root, name, true);
            let base = root.join("vuuut-pro/run");
            std::fs::create_dir_all(&base).unwrap();
            // Compared by where they land, not how they are spelled: on a
            // case-insensitive filesystem the first name in the list that
            // matches wins, and `vuuut` and `Vuuut` are the same directory.
            let found = discover(&base).expect("no checkout found");
            assert_eq!(
                base.join(&found).canonicalize().unwrap(),
                root.join(name).canonicalize().unwrap()
            );
            let binary = discover_binary(&base).expect("no binary found");
            assert_eq!(
                base.join(&binary).canonicalize().unwrap(),
                root.join(name).join("target/release/ghost").canonicalize().unwrap()
            );
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// An unbuilt clone still counts: the themes are committed, so a new site
    /// can be seeded, and `run.sh` builds the binary.
    #[test]
    fn a_checkout_with_no_binary_is_still_a_checkout() {
        let root = scratch().join("unbuilt");
        std::fs::create_dir_all(&root).unwrap();
        make_checkout(&root, "vuuut", false);
        let base = root.join("vuuut-pro/run");
        std::fs::create_dir_all(&base).unwrap();
        assert_eq!(discover(&base), Some("../../vuuut".to_string()));
        assert_eq!(discover_binary(&base), None);
        assert_eq!(discover_themes(&base), Some("../../vuuut/content/themes".to_string()));
        assert!(base.join("../../vuuut/content/themes").is_dir());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_directory_with_the_right_name_and_nothing_in_it_is_not_a_checkout() {
        let root = scratch().join("empty");
        std::fs::create_dir_all(root.join("vuuut")).unwrap();
        let base = root.join("vuuut-pro/run");
        std::fs::create_dir_all(&base).unwrap();
        assert_eq!(discover(&base), None);
        std::fs::remove_dir_all(&root).ok();
    }
}
