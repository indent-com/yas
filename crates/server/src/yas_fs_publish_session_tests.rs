//! Session-level PUBLISH replay and admission contracts. Native syscall failures live in
//! yas_fs_publish_tests.rs; these tests exercise the real async Session::publish adapter.
use super::*;

fn runtime() -> Runtime {
    let app = crate::tests::process_transport::test_state(crate::process::Server::new(false, true));
    let mut runtime = Runtime::new(app);
    // Do not change process environment: tests may run alongside env-sensitive suites.
    let inner = Arc::get_mut(&mut runtime.inner).unwrap();
    inner.enabled = true;
    inner.writes_enabled = true;
    runtime
}

struct Fixture {
    directory: tempfile::TempDir,
    session: Session,
    request: wire::Publish,
}

impl Fixture {
    async fn new(mut runtime: Runtime, flags: u16, writes_enabled: bool) -> Self {
        Arc::get_mut(&mut runtime.inner).unwrap().writes_enabled = writes_enabled;
        let directory = tempfile::tempdir().unwrap();
        let root_path = fs::canonicalize(directory.path()).unwrap();
        let session = runtime.session([91; 16]).unwrap();
        let opened = session
            .open(&wire::Open {
                source: wire::RootSource::PlatformPath(os_path_bytes(&root_path)),
                flags,
                extensions: Extensions::default(),
            })
            .await
            .unwrap();
        let bytes = b"verified publication";
        fs::write(directory.path().join("source"), bytes).unwrap();
        let request = wire::Publish {
            root_handle: opened.root_handle,
            source: path(b"source"),
            destination: path(b"destination"),
            precondition: wire::Precondition::Absent,
            operation_id: [42; 16],
            byte_len: bytes.len() as u64,
            content_hash: *blake3::hash(bytes).as_bytes(),
            extensions: Extensions::default(),
        };
        Self {
            directory,
            session,
            request,
        }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn assert_not_published(&self) {
        assert_eq!(
            fs::read(self.file("source")).unwrap(),
            b"verified publication"
        );
        assert!(!self.file("destination").exists());
        assert_eq!(fs::read_dir(self.directory.path()).unwrap().count(), 1);
        assert!(self.session.inner.replay.lock().unwrap().values.is_empty());
    }
}

fn path(component: &[u8]) -> wire::Path {
    wire::Path {
        components: vec![component.to_vec()],
    }
}

#[test]
fn publish_capability_matches_native_directory_durability_support() {
    let runtime = runtime();
    assert_eq!(
        runtime.limits().supports(schema::fs::CAPABILITY_PUBLISH),
        cfg!(unix)
    );
    assert_eq!(
        runtime.limits().capabilities & !(schema::fs::CAPABILITY_PUBLISH as u32),
        wire::Limits::HARD.capabilities & !(schema::fs::CAPABILITY_PUBLISH as u32),
        "platform gating must not remove unrelated capabilities",
    );
}

#[tokio::test]
async fn publish_without_advertised_capability_is_rejected_before_touching_files() {
    let mut runtime = runtime();
    Arc::get_mut(&mut runtime.inner)
        .unwrap()
        .limits
        .capabilities &= !(schema::fs::CAPABILITY_PUBLISH as u32);
    let fixture = Fixture::new(runtime, 0, true).await;
    let error = fixture.session.publish(&fixture.request).await.unwrap_err();
    assert_eq!(error.kind(), &Error::Unsupported);
    fixture.assert_not_published();
}

#[cfg(not(unix))]
#[tokio::test]
async fn publish_is_unsupported_on_platforms_without_parent_directory_sync() {
    let fixture = Fixture::new(runtime(), 0, true).await;
    assert_eq!(
        fixture
            .session
            .publish(&fixture.request)
            .await
            .unwrap_err()
            .kind(),
        &Error::Unsupported
    );
    fixture.assert_not_published();
}

#[cfg(unix)]
#[tokio::test]
async fn publish_read_only_open_and_server_write_policy_both_deny_without_replay() {
    for (flags, writes_enabled) in [(schema::fs::OPEN_READ_ONLY as u16, true), (0, false)] {
        let fixture = Fixture::new(runtime(), flags, writes_enabled).await;
        let error = fixture.session.publish(&fixture.request).await.unwrap_err();
        assert_eq!(error.kind(), &Error::Permission);
        fixture.assert_not_published();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_publish_replays_one_operation_even_with_absent_precondition() {
    let fixture = Fixture::new(runtime(), 0, true).await;
    // The first call yields at spawn_blocking. The other calls must wait for its replay
    // recording, not publish again or fail the now-unsatisfied Absent precondition.
    let (first, second, third, fourth) = tokio::join!(
        fixture.session.publish(&fixture.request),
        fixture.session.publish(&fixture.request),
        fixture.session.publish(&fixture.request),
        fixture.session.publish(&fixture.request),
    );
    let first = first.unwrap();
    for replay in [second, third, fourth] {
        assert_eq!(replay.unwrap().encode().unwrap(), first.encode().unwrap());
    }
    assert_eq!(
        fs::read(fixture.file("destination")).unwrap(),
        b"verified publication"
    );
    assert_eq!(
        fs::read(fixture.file("source")).unwrap(),
        b"verified publication"
    );
    assert_eq!(fs::read_dir(fixture.directory.path()).unwrap().count(), 2);
    let replay = fixture.session.inner.replay.lock().unwrap();
    assert_eq!(replay.values.len(), 1);
    assert_eq!(replay.order.len(), 1);
    assert_eq!(replay.order.front(), Some(&fixture.request.operation_id));
}

#[cfg(unix)]
#[tokio::test]
async fn publish_replay_preserves_later_destination_and_does_not_reopen_source() {
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new(runtime(), 0, true).await;
    let mut request = fixture.request.clone();
    request.precondition = wire::Precondition::Any;
    let first = fixture.session.publish(&request).await.unwrap();
    fs::write(fixture.file("destination"), b"later independent edit").unwrap();
    let before = fs::metadata(fixture.file("destination")).unwrap();
    fs::remove_file(fixture.file("source")).unwrap();
    let replay = fixture.session.publish(&request).await.unwrap();
    assert_eq!(replay.encode().unwrap(), first.encode().unwrap());
    assert_eq!(
        fs::read(fixture.file("destination")).unwrap(),
        b"later independent edit"
    );
    let after = fs::metadata(fixture.file("destination")).unwrap();
    assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
    assert!(!fixture.file("source").exists());
    assert_eq!(fs::read_dir(fixture.directory.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn changed_publish_request_cannot_reuse_operation_id_or_replace_destination() {
    let fixture = Fixture::new(runtime(), 0, true).await;
    let original = fixture.session.publish(&fixture.request).await.unwrap();
    for field in [
        "destination",
        "source",
        "precondition",
        "length",
        "hash",
        "root",
    ] {
        let mut changed = fixture.request.clone();
        match field {
            "destination" => changed.destination = path(b"other"),
            "source" => changed.source = path(b"other-source"),
            "precondition" => changed.precondition = wire::Precondition::Any,
            "length" => changed.byte_len += 1,
            "hash" => changed.content_hash[0] ^= 1,
            "root" => {
                // Use a real open handle so rejection reaches request fingerprint checking.
                let second_root = tempfile::tempdir().unwrap();
                let opened = fixture
                    .session
                    .open(&wire::Open {
                        source: wire::RootSource::PlatformPath(os_path_bytes(second_root.path())),
                        flags: 0,
                        extensions: Extensions::default(),
                    })
                    .await
                    .unwrap();
                changed.root_handle = opened.root_handle;
            }
            _ => unreachable!(),
        }
        let error = fixture.session.publish(&changed).await.unwrap_err();
        assert_eq!(
            error.kind(),
            &Error::Invalid("FS operation ID reused with a different request"),
            "{field}"
        );
        assert_eq!(
            fs::read(fixture.file("destination")).unwrap(),
            b"verified publication"
        );
        assert!(!fixture.file("other").exists());
    }
    assert_eq!(
        fixture.session.publish(&fixture.request).await.unwrap(),
        original
    );
    assert_eq!(fixture.session.inner.replay.lock().unwrap().values.len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn nondurable_publish_result_is_replayed_exactly_without_retrying_publication() {
    let fixture = Fixture::new(runtime(), 0, true).await;
    // Native sync failure injection is tested separately. Seed precisely the replay value
    // Session::publish records after publication with a failed parent-directory sync.
    let result = wire::PublishResult {
        durable: false,
        extensions: Extensions::default(),
    };
    fixture.session.remember_replay(
        fixture.request.operation_id,
        request_fingerprint(&fixture.request).unwrap(),
        ReplayValue::Publish(result.clone()),
    );
    fs::remove_file(fixture.file("source")).unwrap();
    for _ in 0..2 {
        assert_eq!(
            fixture.session.publish(&fixture.request).await.unwrap(),
            result
        );
    }
    assert_eq!(fs::read_dir(fixture.directory.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn fresh_open_does_not_reuse_a_directory_replaced_at_the_same_path() {
    let runtime = runtime();
    let base = tempfile::tempdir().unwrap();
    let current = base.path().join("work");
    let old = base.path().join("work-old");
    fs::create_dir(&current).unwrap();
    fs::write(current.join("source"), b"same bytes").unwrap();
    let first = runtime.session([81; 16]).unwrap();
    let open = || wire::Open {
        source: wire::RootSource::PlatformPath(os_path_bytes(&current)),
        flags: 0,
        extensions: Extensions::default(),
    };
    let a = first.open(&open()).await.unwrap();
    fs::rename(&current, &old).unwrap();
    fs::create_dir(&current).unwrap();
    fs::write(current.join("source"), b"same bytes").unwrap();
    let second = runtime.session([82; 16]).unwrap();
    let b = second.open(&open()).await.unwrap();
    assert_ne!(a.root_handle, b.root_handle);
    let request = |root_handle, operation_id| wire::Publish {
        root_handle,
        source: path(b"source"),
        destination: path(b"destination"),
        precondition: wire::Precondition::Absent,
        operation_id,
        byte_len: 10,
        content_hash: *blake3::hash(b"same bytes").as_bytes(),
        extensions: Extensions::default(),
    };
    assert!(
        second
            .publish(&request(b.root_handle, [83; 16]))
            .await
            .unwrap()
            .durable
    );
    assert_eq!(
        fs::read(current.join("destination")).unwrap(),
        b"same bytes"
    );
    assert!(!old.join("destination").exists());
    // Existing handles still retain the original authorized directory identity.
    assert!(
        first
            .publish(&request(a.root_handle, [84; 16]))
            .await
            .unwrap()
            .durable
    );
    assert_eq!(fs::read(old.join("destination")).unwrap(), b"same bytes");
    // A later OPEN of the unchanged current directory can reuse its actual identity.
    let again = second.open(&open()).await.unwrap();
    assert_eq!(again.root_handle, b.root_handle);
}
