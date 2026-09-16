# Source provenance

This directory is the Apache-2.0 Alacritty terminal-library fork used by YAS.
It preserves the published source archive identified by crates.io checksum
`d27f91ac05f3c641d43ba60179a7307b3a73e5fe6862a3fdb42b92e762405a1a` and
upstream VCS commit `ee991a565c6c6dfdc5e75ba6de3c76fddb206f4b`.

The package name, prerelease suffix, description, repository metadata, and
local readme path were changed for the standalone YAS package. The Rust
sources were then normalized once with the repository's stable rustfmt 1.9.0
so the required workspace-wide format check remains deterministic.
`LICENSE-APACHE`, tests, changelog, authors, and the upstream
source provenance recorded above are retained. Cargo's registry-unpack marker,
generated VCS file, and original manifest stay in this vendor directory for
auditability but are excluded from the republished crate payload because Cargo
reserves those filenames.

Dependency requirements and the lockfile are updated by YAS independently of
the retained upstream source archive.
The Windows child-exit callback uses `bool` to match windows-sys 0.61.

YAS fixes Kitty keyboard stack overflow to evict the oldest keyboard entry
without touching the title stack. Set/union/difference keyboard operations
also update the current stack entry so queries and screen restoration agree.
