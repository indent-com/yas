#[test]
fn publish_parent_swap_never_redirects_temp_rename_or_sync_outside_root() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(fixture.file("sub")).unwrap();
    let mut request = fixture.request(b"pinned bytes");
    fs::rename(fixture.file("source"), fixture.file("sub/source")).unwrap();
    request.source = path(&[b"sub", b"source"]);
    request.destination = path(&[b"sub", b"destination"]);
    fs::write(outside.path().join("destination"), b"outside intact").unwrap();
    let original = fs::metadata(fixture.file("sub")).unwrap();
    let result = publish_file_with_open_hook(
        &fixture.root,
        &request,
        |parent| {
            let actual = parent.metadata().unwrap();
            assert_eq!(
                (actual.dev(), actual.ino()),
                (original.dev(), original.ino())
            );
            parent.sync_all().map_err(os_io("fsync"))
        },
        || {
            fs::rename(fixture.file("sub"), fixture.file("saved-sub")).unwrap();
            symlink(outside.path(), fixture.file("sub")).unwrap();
            Ok(())
        },
    )
    .unwrap();
    assert!(result.durable);
    assert_eq!(
        fs::read(fixture.file("saved-sub/destination")).unwrap(),
        b"pinned bytes"
    );
    assert_eq!(
        fs::read(outside.path().join("destination")).unwrap(),
        b"outside intact"
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
    assert_eq!(fs::read_dir(fixture.file("saved-sub")).unwrap().count(), 2);
}

#[test]
fn publish_absent_does_not_overwrite_a_destination_created_while_copying() {
    let fixture = Fixture::new();
    let mut request = fixture.request(b"new");
    request.precondition = wire::Precondition::Absent;
    let result = publish_file_with_open_hook(
        &fixture.root,
        &request,
        |_| panic!("must not sync on conflict"),
        || {
            fs::write(fixture.file("destination"), b"concurrent writer").unwrap();
            Ok(())
        },
    );
    assert_eq!(result.unwrap_err().kind(), &Error::PreconditionFailed);
    assert_eq!(
        fs::read(fixture.file("destination")).unwrap(),
        b"concurrent writer"
    );
    fixture.assert_no_temps();
}

#[test]
fn publish_absent_conflict_does_not_open_or_hash_an_existing_fifo() {
    let fixture = Fixture::new();
    let mut request = fixture.request(b"new");
    request.precondition = wire::Precondition::Absent;
    let fifo = CString::new(fixture.file("destination").as_os_str().as_bytes()).unwrap();
    // SAFETY: fifo is a valid NUL-terminated pathname in this test's private directory.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert_eq!(
        fixture.rejected(&request).kind(),
        &Error::PreconditionFailed
    );
    assert!(
        fs::symlink_metadata(fixture.file("destination"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
}

#[test]
fn publish_hash_revision_preconditions_fail_before_source_open_or_destination_read() {
    let fixture = Fixture::new();
    let mut request = fixture.request(b"new");
    fs::remove_file(fixture.file("source")).unwrap();
    let fifo = CString::new(fixture.file("destination").as_os_str().as_bytes()).unwrap();
    // SAFETY: as above; no writer exists, so a blocking destination open would hang.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    for precondition in [
        wire::Precondition::Hash([1; 32]),
        wire::Precondition::Revision(1),
    ] {
        request.precondition = precondition;
        assert_eq!(fixture.rejected(&request).kind(), &Error::Unsupported);
    }
}

#[test]
fn publish_data_fsync_error_keeps_original_and_cleans_its_private_temp() {
    let fixture = Fixture::new();
    let request = fixture.request(b"new");
    fs::write(fixture.file("destination"), b"original").unwrap();
    let called = Cell::new(false);
    let error = publication::publish_file_with_hooks(
        &fixture.root,
        &request,
        |_| panic!("parent sync cannot run before data sync succeeds"),
        || Ok(()),
        |file| {
            called.set(true);
            assert_eq!(file.metadata().unwrap().len(), 3);
            Err(io::Error::from_raw_os_error(libc::EIO))
        },
    )
    .unwrap_err();
    assert!(called.get());
    assert_eq!(error.os_error().unwrap().name, "EIO");
    assert_eq!(fs::read(fixture.file("destination")).unwrap(), b"original");
    fixture.assert_no_temps();
}
