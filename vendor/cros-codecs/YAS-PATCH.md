# YAS changes

Based on cros-codecs 0.0.6, the published source archive identified by
crates.io checksum
`80f7441b4f31c17b6b6b7f57f6c202944aad11d0ab23739a9ff88d8d34dec621` and upstream
VCS commit `7a4211c57ac2e791412f40df2c22fd2cf81104f6`. YAS uses its `backend`
feature.

It is published to crates.io as `yas-cros-codecs` (`0.0.6-yas.N`), keeping the
`cros_codecs` library name, because crates.io keeps no path dependencies: a
published `yas-server` can only depend on this fork by a registry version.
`yas-server` pins it with `=`, and `yas-publish-crates` publishes it before the
workspace crates. Cargo's registry-unpack marker, generated VCS file, and
original manifest stay here for auditability but are excluded from the
republished crate, as Cargo reserves those filenames.

- Rename the package, and point its repository at YAS (the homepage stays
  upstream's).
- Refresh registry dependencies to current releases.
- Move the GBM/DRM dependencies out of `backend` into a `gbm` feature (which
  `vaapi` and `v4l2` enable), so the parser and stateless decoder subset YAS
  uses does not link libgbm.
- Add `StatelessDecoder::<H264, _>::end_access_unit`, which finishes the
  picture of a complete access unit without waiting for the next one.
- Silence dead-code and elided-lifetime warnings crate-wide.
- Adapt V4L2 descriptor operations to nix 0.31: pass borrowed descriptors to
  `fstat` and use the owned descriptor returned by `dup`.
- Keep the ARM NEON detiler on AArch64 and copy tiles directly on other
  architectures so the V4L2 library also builds on x86-64.
