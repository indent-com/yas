# YAS changes

Based on cros-codecs 0.0.6. YAS uses its `backend` feature.

- Refresh registry dependencies to current releases.
- Adapt V4L2 descriptor operations to nix 0.31: pass borrowed descriptors to
  `fstat` and use the owned descriptor returned by `dup`.
- Keep the ARM NEON detiler on AArch64 and copy tiles directly on other
  architectures so the V4L2 library also builds on x86-64.
