//! Where the web UI the edge serves comes from.
//!
//! In the repository it is `js/ui/dist`, which the UI's build fills (or
//! placeholders: `bin/dev-build`). A packaged crate has no repository around
//! it, so it carries a copy in `ui/`: `bin/package-crates` and
//! `bin/publish-crates` put the UI there before `cargo package`, and
//! Cargo.toml's `include` takes it although git ignores it.

use std::path::{Path, PathBuf};

const ASSETS: [&str; 2] = ["index.html.br", "sw.js.br"];

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let repository = manifest.join("../../js/ui/dist");
    let packaged = manifest.join("ui");
    let complete = |dir: &Path| ASSETS.iter().all(|asset| dir.join(asset).is_file());
    let dist = if complete(&repository) {
        repository
    } else if complete(&packaged) {
        packaged
    } else {
        panic!(
            "no web UI to embed: neither {} (the repository's: build js/ui, or bin/dev-build \
             for placeholders) nor {} (a packaged crate's) holds {}",
            repository.display(),
            packaged.display(),
            ASSETS.join(" and ")
        );
    };
    for asset in ASSETS {
        println!("cargo:rerun-if-changed={}", dist.join(asset).display());
    }
    println!("cargo:rustc-env=YAS_UI_DIST={}", dist.display());
}
