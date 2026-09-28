#![cfg(unix)]

//! Choosing a runtime directory that belongs to this user alone.
//!
//! Two things in yas need a directory to put a Unix socket in: the native IPC
//! endpoint (`yas-webserver`'s `local_ipc`) and the Wayland compositor. Both
//! have the same requirement — the directory must be private to the effective
//! user — and the same hazard, which is that the conventional fallback,
//! `/tmp`, is shared with every other user on the machine.
//!
//! Sharing it is not merely untidy. A per-user socket in a world-writable
//! sticky directory collides by name with the same socket belonging to
//! somebody else, and the loser of that race cannot recover: with
//! `fs.protected_regular=1` an `O_CREAT` open of another user's file in a
//! sticky directory is `EACCES`, so even the companion lock file is
//! unreachable. The fix is never to hand out a shared directory in the first
//! place — a sticky base yields a `yas-{uid}` child of its own, created 0700
//! and verified, and a base that is neither private nor sticky is refused
//! rather than guessed at.
//!
//! This lives in its own crate because both callers need it and neither may
//! depend on the other: the compositor is a leaf crate, and the policy is too
//! easy to get subtly wrong to keep two copies of.

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// The effective UID of this process.
pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// What a candidate base directory is allowed to be.
#[derive(Clone, Copy)]
pub enum BasePolicy {
    /// Only a directory this user owns with no group or other access. Use for
    /// bases that name a per-user location already — `XDG_RUNTIME_DIR`,
    /// `/run/user/$uid` — where anything else means the environment is lying.
    PrivateOnly,
    /// Also accept a root-owned sticky world-writable directory, i.e. `/tmp`,
    /// in which case the result is a `yas-{uid}` child rather than the shared
    /// directory itself.
    PrivateOrSticky,
}

/// Resolve `base` to a mode-0700 runtime directory owned by `uid`, or `None`
/// when `base` cannot safely yield one.
///
/// A private base gets a `yas` child; a sticky shared base gets `yas-{uid}`,
/// which is what keeps two users' sockets from colliding in `/tmp`.
pub fn runtime_dir_for_base(base: &Path, uid: u32, policy: BasePolicy) -> Option<PathBuf> {
    if !normal_absolute_path(base) {
        return None;
    }
    let metadata = std::fs::symlink_metadata(base).ok()?;
    if !metadata.file_type().is_dir() {
        return None;
    }

    let private = metadata.uid() == uid && metadata.mode() & 0o077 == 0;
    let sticky_shared = matches!(policy, BasePolicy::PrivateOrSticky)
        && metadata.uid() == 0
        && u64::from(metadata.mode()) & u64::from(libc::S_ISVTX) != 0
        && metadata.mode() & 0o002 != 0;
    let runtime_dir = if private {
        base.join("yas")
    } else if sticky_shared {
        base.join(format!("yas-{uid}"))
    } else {
        return None;
    };
    prepare_private_runtime_dir(&runtime_dir, uid).ok()?;
    Some(runtime_dir)
}

/// Create `path` as a mode-0700 directory owned by `uid`, or verify that an
/// existing one is exactly that.
///
/// Rechecked after the `set_permissions` repair so a directory swapped under
/// us between the two is rejected rather than accepted on the strength of its
/// earlier metadata.
pub fn prepare_private_runtime_dir(path: &Path, uid: u32) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime path is not a directory",
        ));
    }
    if metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory is not owned by the effective user",
        ));
    }
    if metadata.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    let checked = std::fs::symlink_metadata(path)?;
    if !checked.file_type().is_dir() || checked.uid() != uid || checked.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory failed its owner-only check",
        ));
    }
    Ok(())
}

/// Whether `path` is usable as a runtime directory exactly as given: absolute,
/// component-normal, a directory, owned by `uid`, and closed to group and
/// other.
///
/// This is the check for a base that is already per-user by construction —
/// `XDG_RUNTIME_DIR`, `/run/user/$uid` — where the answer is the directory
/// itself rather than a child of it. Callers that would otherwise namespace a
/// child into it need that: [`runtime_dir_for_base`] is not idempotent, so
/// anything that re-resolves a directory it previously resolved (a compositor
/// that exports its choice through the environment and is then started again
/// in the same process) would nest one level deeper every time.
pub fn is_usable_private_dir(path: &Path, uid: u32) -> bool {
    if !normal_absolute_path(path) {
        return false;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.file_type().is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0
}

/// Absolute and free of `.`, `..`, and prefix components.
pub fn normal_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn malicious_prebound_runtime_symlink_is_rejected() {
        let base = tempfile::tempdir().unwrap();
        let target = base.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let runtime = base.path().join("yas");
        symlink(&target, &runtime).unwrap();
        assert!(prepare_private_runtime_dir(&runtime, effective_uid()).is_err());
    }

    #[test]
    fn prebound_runtime_directory_with_wrong_expected_owner_is_rejected() {
        let base = tempfile::tempdir().unwrap();
        let runtime = base.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        assert!(prepare_private_runtime_dir(&runtime, effective_uid().wrapping_add(1)).is_err());
    }

    /// The whole point of the crate: a shared sticky base never resolves to
    /// itself, so two users asking the same question get different answers
    /// and never contend for one socket name.
    #[test]
    fn sticky_shared_base_yields_a_per_uid_child() {
        let uid = effective_uid();
        let Some(dir) = runtime_dir_for_base(Path::new("/tmp"), uid, BasePolicy::PrivateOrSticky)
        else {
            // A host whose /tmp is not root-owned-sticky has nothing to assert.
            return;
        };
        assert_eq!(dir, Path::new("/tmp").join(format!("yas-{uid}")));
        assert_ne!(
            dir,
            runtime_dir_for_base(Path::new("/tmp"), uid + 1, BasePolicy::PrivateOrSticky)
                .unwrap_or_else(|| Path::new("/tmp").join(format!("yas-{}", uid + 1)))
        );
    }

    /// `PrivateOnly` is what an `XDG_RUNTIME_DIR` claim is checked against, so
    /// a shared directory offered there is refused rather than trusted.
    #[test]
    fn sticky_shared_base_is_refused_under_private_only() {
        assert!(
            runtime_dir_for_base(Path::new("/tmp"), effective_uid(), BasePolicy::PrivateOnly)
                .is_none()
        );
    }

    /// `is_usable_private_dir` answers with the directory itself, so feeding
    /// it its own previous answer changes nothing. `runtime_dir_for_base`
    /// appends on every call, which is why a caller that re-resolves must use
    /// the former.
    #[test]
    fn private_dir_acceptance_is_idempotent() {
        let base = tempfile::tempdir().unwrap();
        let uid = effective_uid();
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(is_usable_private_dir(base.path(), uid));

        let once = runtime_dir_for_base(base.path(), uid, BasePolicy::PrivateOnly).unwrap();
        let twice = runtime_dir_for_base(&once, uid, BasePolicy::PrivateOnly).unwrap();
        assert_ne!(once, twice, "resolving a resolved directory nests");
        assert!(is_usable_private_dir(&once, uid));
        assert!(is_usable_private_dir(&twice, uid));
    }

    #[test]
    fn shared_and_unowned_directories_are_not_usable_as_given() {
        assert!(!is_usable_private_dir(Path::new("/tmp"), effective_uid()));
        assert!(!is_usable_private_dir(
            Path::new("relative"),
            effective_uid()
        ));
        assert!(!is_usable_private_dir(
            Path::new("/nonexistent/yas-none"),
            effective_uid()
        ));
    }

    #[test]
    fn relative_and_dotted_bases_are_refused() {
        assert!(!normal_absolute_path(Path::new("relative/path")));
        assert!(!normal_absolute_path(Path::new("/has/../dotdot")));
        assert!(normal_absolute_path(Path::new("/run/user/1000")));
    }
}
