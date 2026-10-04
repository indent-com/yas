//! Where the web UI the edge serves comes from.
//!
//! In the repository it is `js/ui/dist`, which the UI's build fills (or
//! placeholders: `bin/dev-build`). A packaged crate has no repository around
//! it, so it carries a copy in `ui/`: `bin/package-crates` and
//! `bin/publish-crates` put the UI there before `cargo package`, and
//! Cargo.toml's `include` takes it although git ignores it.
//!
//! Without the `ui` feature (a program that carries the yas CLI and serves no
//! browser UI of its own) the edge serves a page saying this build has none,
//! made here, so nothing needs the UI built.

use std::io::Write;
use std::path::{Path, PathBuf};

const ASSETS: [&str; 2] = ["index.html.br", "sw.js.br"];

/// What the edge serves without the `ui` feature.
const PLACEHOLDERS: [(&str, &str); 2] = [
    (
        "index.html.br",
        "<!doctype html><meta charset=utf-8><title>YAS</title>\
         <p>This build of YAS carries no browser UI.</p>\n",
    ),
    ("sw.js.br", "// This build of YAS carries no browser UI.\n"),
];

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_UI");
    if std::env::var_os("CARGO_FEATURE_UI").is_none() {
        let dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("no-ui");
        std::fs::create_dir_all(&dir).unwrap();
        for (asset, text) in PLACEHOLDERS {
            let mut compressed = Vec::new();
            {
                let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 11, 22);
                writer.write_all(text.as_bytes()).unwrap();
            }
            std::fs::write(dir.join(asset), compressed).unwrap();
        }
        println!("cargo:rustc-env=YAS_UI_DIST={}", dir.display());
        return;
    }
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
             for placeholders) nor {} (a packaged crate's) holds {}; or build without the \
             `ui` feature",
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
