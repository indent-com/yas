//! Descriptor-relative Unix publication. No guest program, global sync or mutable parent
//! pathname participates after admission. The caller-owned source is never renamed.
use super::*;
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd};

fn name(bytes: &[u8]) -> Result<CString, Error> {
    CString::new(bytes).map_err(|_| Error::Invalid("invalid PUBLISH component"))
}

fn open_at(parent: &File, name: &CStr, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    // SAFETY: parent is an owned directory fd; name is a live NUL-terminated component.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh descriptor, transferred exactly once to File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

struct Temp<'a> {
    parent: &'a File,
    name: CString,
    file: Option<File>,
    armed: bool,
}

impl Drop for Temp<'_> {
    fn drop(&mut self) {
        self.file.take();
        if self.armed {
            // SAFETY: both descriptors/name remain live; remove only our random temp.
            let _ = unsafe { libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0) };
        }
    }
}

fn temp(parent: &File) -> Result<Temp<'_>, Error> {
    for _ in 0..32 {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| Error::Internal)?;
        let name = name(format!(".yas-write-{:032x}.tmp", u128::from_le_bytes(random)).as_bytes())?;
        match open_at(
            parent,
            &name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        ) {
            Ok(file) => {
                return Ok(Temp {
                    parent,
                    name,
                    file: Some(file),
                    armed: true,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(os_io("open")(error)),
        }
    }
    Err(Error::ResourceExhausted)
}

pub(super) fn publish_file(
    root: &Root,
    request: &wire::Publish,
    sync_parent: impl FnOnce(&File) -> Result<(), Error>,
) -> Result<wire::PublishResult, Error> {
    publish_file_with_open_hook(root, request, sync_parent, || Ok(()))
}

pub(super) fn publish_file_with_open_hook(
    root: &Root,
    request: &wire::Publish,
    sync_parent: impl FnOnce(&File) -> Result<(), Error>,
    after_open: impl FnOnce() -> Result<(), Error>,
) -> Result<wire::PublishResult, Error> {
    publish_file_with_hooks(root, request, sync_parent, after_open, File::sync_all)
}

pub(super) fn publish_file_with_hooks(
    root: &Root,
    request: &wire::Publish,
    sync_parent: impl FnOnce(&File) -> Result<(), Error>,
    after_open: impl FnOnce() -> Result<(), Error>,
    sync_file: impl FnOnce(&File) -> io::Result<()>,
) -> Result<wire::PublishResult, Error> {
    // Validate paths even for direct native callers, not only decoded wire requests.
    joined_path(root, &request.source)?;
    joined_path(root, &request.destination)?;
    if root.single_file
        || request.byte_len > schema::fs::MAX_PUBLISH_BYTES
        || request.source == request.destination
        || request.source.components.is_empty()
        || request.destination.components.is_empty()
        || request.source.components[..request.source.components.len() - 1]
            != request.destination.components[..request.destination.components.len() - 1]
    {
        return Err(Error::Invalid(
            "PUBLISH requires distinct adjacent files within its size limit",
        ));
    }
    // Unlike legacy staged writes, PUBLISH does not promise hash/revision CAS against
    // arbitrary external writers. Any and atomic Absent require no unbounded target reads.
    if !matches!(
        request.precondition,
        wire::Precondition::Any | wire::Precondition::Absent
    ) {
        return Err(Error::Unsupported);
    }
    let mut parent = root
        .publication_dir
        .as_ref()
        .ok_or(Error::Unsupported)?
        .try_clone()
        .map_err(os_io("open"))?;
    // Every directory is opened relative to the previously pinned directory. Symlink
    // parents are explicitly unsupported, not canonicalized and reopened by pathname.
    for component in &request.source.components[..request.source.components.len() - 1] {
        parent = open_at(
            &parent,
            &name(component)?,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(os_io("open"))?;
    }
    let source_name = name(request.source.components.last().unwrap())?;
    let destination = name(request.destination.components.last().unwrap())?;
    let mut source = open_at(
        &parent,
        &source_name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )
    .map_err(os_io("open"))?;
    let metadata = source.metadata().map_err(os_io("fstat"))?;
    if !metadata.is_file() || metadata.len() != request.byte_len {
        return Err(Error::Invalid(
            "PUBLISH source is not a regular file of the expected size",
        ));
    }
    after_open()?;
    let mut temp = temp(&parent)?;
    let output = temp.file.as_mut().unwrap();
    let mut hash = blake3::Hasher::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let capacity = (request.byte_len - count).min(buffer.len() as u64) as usize;
        let n = source
            .read(&mut buffer[..capacity.max(1)])
            .map_err(os_io("read"))?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > request.byte_len {
            return Err(Error::Invalid("PUBLISH source size changed"));
        }
        hash.update(&buffer[..n]);
        output.write_all(&buffer[..n]).map_err(os_io("write"))?;
    }
    if count != request.byte_len || hash.finalize().as_bytes() != &request.content_hash {
        return Err(Error::Invalid("PUBLISH source integrity mismatch"));
    }
    output.flush().map_err(os_io("write"))?;
    sync_file(output).map_err(os_io("fsync"))?;
    // Close the writer before publication; the source remains the caller's cleanup duty.
    temp.file.take();
    let fd = parent.as_raw_fd();
    let result = if matches!(request.precondition, wire::Precondition::Absent) {
        // linkat is an atomic no-replace publication on both Linux and macOS. It never
        // overwrites an entry created by an external writer while the copy was in progress.
        // SAFETY: fd is a live directory and both names are live single components.
        unsafe { libc::linkat(fd, temp.name.as_ptr(), fd, destination.as_ptr(), 0) }
    } else {
        // SAFETY: as above. renameat atomically replaces only this pinned directory's entry.
        unsafe { libc::renameat(fd, temp.name.as_ptr(), fd, destination.as_ptr()) }
    };
    if result < 0 {
        // Never inspect/hash the destination to describe a conflict: that could follow a
        // concurrently substituted FIFO or read an unbounded unrelated file.
        let error = io::Error::last_os_error();
        if matches!(request.precondition, wire::Precondition::Absent)
            && error.kind() == io::ErrorKind::AlreadyExists
        {
            return Err(Error::PreconditionFailed);
        }
        return Err(os_io("rename")(error));
    }
    // Publication happened. Unlink the no-replace temporary link before syncing the directory.
    let cleanup_ok = if matches!(request.precondition, wire::Precondition::Absent) {
        // SAFETY: remove only our random name relative to the same live directory fd.
        unsafe { libc::unlinkat(fd, temp.name.as_ptr(), 0) == 0 }
    } else {
        true
    };
    temp.armed = !cleanup_ok;
    let durable = sync_parent(&parent).is_ok() && cleanup_ok;
    root.operation_echoes
        .lock()
        .unwrap()
        .insert(request.destination.clone(), request.operation_id);
    Ok(wire::PublishResult {
        durable,
        extensions: Default::default(),
    })
}
