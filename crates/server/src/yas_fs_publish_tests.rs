//! Unix native PUBLISH contracts; deliberately independent of the session transport.
use super::*;
use std::cell::Cell;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};

include!("yas_fs_publish_race_tests.rs");

struct Fixture {
    directory: tempfile::TempDir,
    root: Root,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(directory.path()).unwrap();
        let root = Root {
            handle: 1,
            canonical_path: os_path_bytes(&path),
            single_file: false,
            publication_dir: publication_directory(&path),
            path,
            revision: AtomicU64::new(1),
            entries: StdMutex::new(BTreeMap::new()),
            operation_echoes: StdMutex::new(BTreeMap::new()),
        };
        Self { directory, root }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn request(&self, bytes: &[u8]) -> wire::Publish {
        fs::write(self.file("source"), bytes).unwrap();
        wire::Publish {
            root_handle: self.root.handle,
            source: path(&[b"source"]),
            destination: path(&[b"destination"]),
            precondition: wire::Precondition::Any,
            operation_id: [23; 16],
            byte_len: bytes.len() as u64,
            content_hash: *blake3::hash(bytes).as_bytes(),
            extensions: Extensions::default(),
        }
    }

    fn assert_no_temps(&self) {
        for entry in fs::read_dir(self.directory.path()).unwrap() {
            assert!(
                !entry
                    .unwrap()
                    .file_name()
                    .as_bytes()
                    .starts_with(b".yas-write-"),
                "PUBLISH leaked an adjacent atomic-write temp"
            );
        }
    }

    fn rejected(&self, request: &wire::Publish) -> Error {
        let called = Cell::new(false);
        let error = publish_file(&self.root, request, |_| {
            called.set(true);
            Ok(())
        })
        .unwrap_err();
        assert!(!called.get(), "prepublication error called directory sync");
        self.assert_no_temps();
        error
    }
}

fn path(components: &[&[u8]]) -> wire::Path {
    wire::Path {
        components: components.iter().map(|value| value.to_vec()).collect(),
    }
}

fn hash_file(path: &OsPath) -> (u64, [u8; 32]) {
    let mut file = File::open(path).unwrap();
    let mut hash = blake3::Hasher::new();
    let mut count = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).unwrap();
        if n == 0 {
            return (count, *hash.finalize().as_bytes());
        }
        count += n as u64;
        hash.update(&buffer[..n]);
    }
}

#[test]
fn publish_replaces_privately_keeps_source_and_syncs_after_visibility() {
    let fixture = Fixture::new();
    let request = fixture.request(b"authenticated new content");
    fs::write(fixture.file("destination"), b"old").unwrap();
    fs::set_permissions(fixture.file("source"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(
        fixture.file("destination"),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let calls = Cell::new(0);
    let result = publish_file(&fixture.root, &request, |parent| {
        calls.set(calls.get() + 1);
        let pinned = parent.metadata().unwrap();
        let named = fs::metadata(&fixture.root.path).unwrap();
        assert_eq!((pinned.dev(), pinned.ino()), (named.dev(), named.ino()));
        assert_eq!(
            fs::read(fixture.file("destination")).unwrap(),
            b"authenticated new content"
        );
        Ok(())
    })
    .unwrap();
    assert!(result.durable);
    assert_eq!(calls.get(), 1);
    assert_eq!(
        fs::read(fixture.file("source")).unwrap(),
        b"authenticated new content"
    );
    assert_eq!(
        fs::metadata(fixture.file("destination"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    fixture.assert_no_temps();
}

#[test]
fn publish_rejects_source_symlink_even_when_its_bytes_match() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let request = fixture.request(b"matching bytes");
    let target = outside.path().join("outside");
    fs::write(&target, b"matching bytes").unwrap();
    fs::remove_file(fixture.file("source")).unwrap();
    symlink(&target, fixture.file("source")).unwrap();
    fs::write(fixture.file("destination"), b"original").unwrap();
    fixture.rejected(&request);
    assert_eq!(fs::read(&target).unwrap(), b"matching bytes");
    assert_eq!(fs::read(fixture.file("destination")).unwrap(), b"original");
}

#[test]
fn publish_rejects_fifo_without_waiting_for_a_writer() {
    let fixture = Fixture::new();
    let request = fixture.request(b"");
    fs::remove_file(fixture.file("source")).unwrap();
    let fifo = CString::new(fixture.file("source").as_os_str().as_bytes()).unwrap();
    // No writer is opened: a blocking open here would hang, rather than reject the FIFO.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    fixture.rejected(&request);
    assert!(!fixture.file("destination").exists());
}

#[test]
fn publish_rejects_directory_source() {
    let fixture = Fixture::new();
    let request = fixture.request(b"");
    fs::remove_file(fixture.file("source")).unwrap();
    fs::create_dir(fixture.file("source")).unwrap();
    fixture.rejected(&request);
    assert!(fixture.file("source").is_dir());
    assert!(!fixture.file("destination").exists());
}

#[test]
fn publish_integrity_failures_preserve_destination_and_remove_temps() {
    for failure in ["hash", "short", "long"] {
        let fixture = Fixture::new();
        let mut request = fixture.request(b"verified payload");
        fs::write(fixture.file("destination"), b"original").unwrap();
        match failure {
            "hash" => request.content_hash[0] ^= 1,
            "short" => request.byte_len -= 1,
            "long" => request.byte_len += 1,
            _ => unreachable!(),
        }
        fixture.rejected(&request);
        assert_eq!(fs::read(fixture.file("destination")).unwrap(), b"original");
        assert_eq!(
            fs::read(fixture.file("source")).unwrap(),
            b"verified payload"
        );
    }
}

#[test]
fn publish_destination_preconditions_are_checked_before_publication() {
    for precondition in [
        wire::Precondition::Absent,
        wire::Precondition::Hash(*blake3::hash(b"not the destination").as_bytes()),
        wire::Precondition::Revision(1),
    ] {
        let fixture = Fixture::new();
        let mut request = fixture.request(b"new");
        fs::write(fixture.file("destination"), b"original").unwrap();
        let absent = matches!(precondition, wire::Precondition::Absent);
        request.precondition = precondition;
        let error = fixture.rejected(&request);
        assert_eq!(
            error.kind(),
            if absent {
                &Error::PreconditionFailed
            } else {
                &Error::Unsupported
            }
        );
        assert_eq!(fs::read(fixture.file("destination")).unwrap(), b"original");
    }
}

#[test]
fn publish_rejects_raw_parent_components_on_either_path() {
    for which in ["source", "destination", "both"] {
        let fixture = Fixture::new();
        let mut request = fixture.request(b"new");
        fs::write(fixture.file("destination"), b"original").unwrap();
        if which != "destination" {
            request.source = path(&[b"..", b"source"]);
        }
        if which != "source" {
            request.destination = path(&[b"..", b"destination"]);
        }
        fixture.rejected(&request);
        assert_eq!(fs::read(fixture.file("destination")).unwrap(), b"original");
    }
}

#[test]
fn publish_rejects_adjacent_paths_through_an_outside_symlinked_parent() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let mut request = fixture.request(b"new");
    fs::write(outside.path().join("source"), b"new").unwrap();
    fs::write(outside.path().join("destination"), b"outside original").unwrap();
    symlink(outside.path(), fixture.file("escape")).unwrap();
    request.source = path(&[b"escape", b"source"]);
    request.destination = path(&[b"escape", b"destination"]);
    fixture.rejected(&request);
    assert_eq!(
        fs::read(outside.path().join("destination")).unwrap(),
        b"outside original"
    );
    assert_eq!(fs::read(outside.path().join("source")).unwrap(), b"new");
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 2);
}

#[test]
fn publish_accepts_sparse_source_above_old_64_mib_limit_with_bounded_hashing() {
    let fixture = Fixture::new();
    let mut request = fixture.request(b"");
    let len = 65 * 1024 * 1024;
    let mut sparse = File::options()
        .write(true)
        .open(fixture.file("source"))
        .unwrap();
    sparse.set_len(len).unwrap();
    sparse.write_all(b"sparse header").unwrap();
    std::io::Seek::seek(&mut sparse, io::SeekFrom::End(-4)).unwrap();
    sparse.write_all(b"tail").unwrap();
    drop(sparse);
    (request.byte_len, request.content_hash) = hash_file(&fixture.file("source"));
    assert_eq!(request.byte_len, len);
    let result = publish_file(&fixture.root, &request, |_| Ok(())).unwrap();
    assert!(result.durable);
    assert_eq!(
        hash_file(&fixture.file("destination")),
        (len, request.content_hash)
    );
    assert_eq!(fs::metadata(fixture.file("source")).unwrap().len(), len);
    fixture.assert_no_temps();
}

#[test]
fn publish_rejects_above_one_gib_without_allocating_or_reading_the_source() {
    let fixture = Fixture::new();
    let mut request = fixture.request(b"");
    assert_eq!(schema::fs::MAX_PUBLISH_BYTES, 1024 * 1024 * 1024);
    request.byte_len = schema::fs::MAX_PUBLISH_BYTES + 1;
    File::options()
        .write(true)
        .open(fixture.file("source"))
        .unwrap()
        .set_len(request.byte_len)
        .unwrap();
    fs::write(fixture.file("destination"), b"original").unwrap();
    fixture.rejected(&request);
    assert_eq!(fs::read(fixture.file("destination")).unwrap(), b"original");
}

#[test]
fn publish_parent_fsync_failure_reports_nondurable_but_keeps_published_bytes() {
    let fixture = Fixture::new();
    let request = fixture.request(b"new");
    fs::write(fixture.file("destination"), b"original").unwrap();
    let called = Cell::new(false);
    let result = publish_file(&fixture.root, &request, |parent| {
        called.set(true);
        let pinned = parent.metadata().unwrap();
        let named = fs::metadata(&fixture.root.path).unwrap();
        assert_eq!((pinned.dev(), pinned.ino()), (named.dev(), named.ino()));
        assert_eq!(fs::read(fixture.file("destination")).unwrap(), b"new");
        Err(os_io("fsync")(io::Error::from_raw_os_error(libc::EIO)))
    })
    .unwrap();
    assert!(called.get());
    assert!(!result.durable);
    assert_eq!(fs::read(fixture.file("destination")).unwrap(), b"new");
    assert_eq!(fs::read(fixture.file("source")).unwrap(), b"new");
    fixture.assert_no_temps();
}

#[test]
fn publish_rename_syscall_failure_does_not_sync_or_leave_a_temp() {
    let fixture = Fixture::new();
    let request = fixture.request(b"new");
    fs::create_dir(fixture.file("destination")).unwrap();
    fs::write(fixture.file("destination/child"), b"original").unwrap();
    fixture.rejected(&request);
    assert_eq!(
        fs::read(fixture.file("destination/child")).unwrap(),
        b"original"
    );
    assert_eq!(fs::read(fixture.file("source")).unwrap(), b"new");
}

#[test]
fn publish_pins_source_identity_before_pathname_is_swapped_to_outside_symlink() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let request = fixture.request(b"pinned original");
    let external = outside.path().join("outside");
    fs::write(&external, b"outside must stay untouched").unwrap();
    let result = publish_file_with_open_hook(
        &fixture.root,
        &request,
        |_| Ok(()),
        || {
            fs::rename(fixture.file("source"), fixture.file("pinned-inode")).unwrap();
            symlink(&external, fixture.file("source")).unwrap();
            Ok(())
        },
    )
    .unwrap();
    assert!(result.durable);
    assert_eq!(
        fs::read(fixture.file("destination")).unwrap(),
        b"pinned original"
    );
    assert_eq!(
        fs::read(fixture.file("pinned-inode")).unwrap(),
        b"pinned original"
    );
    assert!(
        fs::symlink_metadata(fixture.file("source"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&external).unwrap(), b"outside must stay untouched");
    fixture.assert_no_temps();
}

#[test]
fn publish_rejects_same_size_mutation_of_pinned_source_inode() {
    let fixture = Fixture::new();
    let request = fixture.request(b"original");
    fs::write(fixture.file("destination"), b"destination intact").unwrap();
    let synced = Cell::new(false);
    let result = publish_file_with_open_hook(
        &fixture.root,
        &request,
        |_| {
            synced.set(true);
            Ok(())
        },
        || {
            // Truncate/write the same inode, not an atomic pathname replacement.
            fs::write(fixture.file("source"), b"tampered").unwrap();
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(!synced.get());
    assert_eq!(
        fs::read(fixture.file("destination")).unwrap(),
        b"destination intact"
    );
    fixture.assert_no_temps();
}
