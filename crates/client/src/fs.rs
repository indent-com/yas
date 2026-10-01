//! Files (the FS family).
//!
//! Every FS operation happens under a **root**: a directory (or single file)
//! opened by platform path. Paths below it are relative, `/`-separated,
//! without `..` (the server confines every operation to the root, including
//! symlink resolution for writes). Contents are identified by their BLAKE3
//! hash, which is also the currency of compare-and-swap writes.
//!
//! ```no_run
//! # async fn demo(client: yas_client::Client) -> yas_client::Result<()> {
//! use yas_client::fs::{Precondition, WriteOptions};
//!
//! let root = client.open_root("/home/me/project", true).await?;
//! let file = root.read("src/main.rs").await?;
//! let mut text = String::from_utf8_lossy(&file.bytes).into_owned();
//! text.push_str("\n// edited\n");
//! // Fails with a CONFLICT error if someone changed the file meanwhile.
//! root.write(
//!     "src/main.rs",
//!     text.as_bytes(),
//!     &WriteOptions::new().precondition(Precondition::Hash(file.hash)),
//! )
//! .await?;
//! # Ok(()) }
//! ```

use std::ffi::OsStr;

use yas_wire::{
    Decode, Encode, Extensions,
    core::{ResultPrefix, Status},
    family,
    fs::{
        self as wire, Apply, ApplyItem, ApplyResult, Close, Commit, CommitResult, ConflictDetail,
        ContentResult, EntryBody, EntryRecord, Fetch, Open, OpenResult, PageDelivery, Path,
        QueryPage, QueryRecord, QueryRecordBatch, Read, ReadQuestion, RootSource, StageWrite,
        StageWriteResult, StateMutation, Watch, request_kind,
    },
    schema::fs as schema,
    state::Watch as StateWatch,
};

pub use yas_wire::fs::{OsError, Precondition};

use crate::client::{Client, DEFAULT_REQUEST_TIMEOUT, Hook, Route};
use crate::error::{Error, Result};
use crate::process::{nonzero_id, os_bytes};
use crate::state::{STATE_CREDIT, Subscription};
use crate::transfer::{ByteSink, ByteStream, collect_delivery, collect_messages, delivery_routes};

/// Largest file this crate reads or writes in one call (the protocol's
/// staged-write limit, 256 MiB).
pub const MAX_FILE_BYTES: u64 = schema::MAX_STAGED_BYTES;

/// What an entry is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File {
        /// Size in bytes.
        len: u64,
        /// BLAKE3 of the content.
        hash: [u8; 32],
    },
    /// A directory.
    Directory,
    /// A symbolic link (never followed by listings).
    Symlink {
        /// The raw link target.
        target: Vec<u8>,
        /// Whether the target resolves to a directory.
        to_directory: bool,
    },
}

/// One filesystem entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the root, `/`-joined (lossy UTF-8 for display).
    pub path: String,
    /// Exact path components (raw platform bytes).
    pub components: Vec<Vec<u8>>,
    /// File, directory, or symlink.
    pub kind: EntryKind,
    /// Unix permission bits (0 where the platform has none).
    pub mode: u32,
    /// Modification time, Unix nanoseconds.
    pub modified_unix_ns: i64,
    /// The server's revision for this entry (usable as a precondition).
    pub revision: u64,
    /// Executable bit / platform equivalent.
    pub executable: bool,
    /// Read-only.
    pub read_only: bool,
    /// Hidden (dot-file or platform attribute).
    pub hidden: bool,
    /// The server could not read it.
    pub unreadable: bool,
}

impl Entry {
    fn from_wire(record: EntryRecord) -> Self {
        let flags = u64::from(record.flags);
        let kind = match record.body {
            EntryBody::File {
                byte_len,
                content_hash,
                ..
            } => EntryKind::File {
                len: byte_len,
                hash: content_hash,
            },
            EntryBody::Directory => EntryKind::Directory,
            EntryBody::Symlink { target, .. } => EntryKind::Symlink {
                target,
                to_directory: flags & schema::ENTRY_SYMLINK_DIRECTORY != 0,
            },
        };
        Self {
            path: display_path(&record.path),
            components: record.path.components,
            kind,
            mode: record.mode,
            modified_unix_ns: record.modified_unix_ns,
            revision: record.entry_revision,
            executable: flags & schema::ENTRY_EXECUTABLE != 0,
            read_only: flags & schema::ENTRY_READ_ONLY != 0,
            hidden: flags & schema::ENTRY_HIDDEN != 0,
            unreadable: flags & schema::ENTRY_UNREADABLE != 0,
        }
    }

    /// The last path component (lossy UTF-8); empty for the root.
    pub fn name(&self) -> String {
        self.components
            .last()
            .map(|component| String::from_utf8_lossy(component).into_owned())
            .unwrap_or_default()
    }

    /// Whether this is a directory.
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }
}

/// What an entry is itself, as [`FsRoot::list_dir`] and [`FsRoot::stat_only`] say (a symlink
/// is a symlink, a FIFO, socket or device is `Other`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink,
    /// Anything else.
    Other,
}

impl Kind {
    fn from_wire(kind: u8) -> Self {
        match u64::from(kind) {
            schema::ENTRY_FILE => Self::File,
            schema::ENTRY_DIRECTORY => Self::Directory,
            schema::ENTRY_SYMLINK => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// One entry of [`FsRoot::list_dir`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// The raw name (one platform component).
    pub name: Vec<u8>,
    /// What the entry is itself.
    pub kind: Kind,
}

/// What [`FsRoot::stat_only`] says of a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatOnly {
    /// What it is (what a final symlink leads to, when followed).
    pub kind: Kind,
    /// The platform mode (`st_mode`, file-type bits included).
    pub mode: u32,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, Unix nanoseconds.
    pub modified_unix_ns: i64,
}

/// The OS error behind a failed FS call (errno's name and what the server was doing), from
/// servers offering `CAPABILITY_OS_ERROR` ([`Client::fs_capabilities`]).
pub fn os_error(error: &Error) -> Option<OsError> {
    match error {
        Error::Status { extensions, .. } => OsError::from_result_detail(extensions).ok().flatten(),
        _ => None,
    }
}

impl Client {
    /// The opt-in FS values this server offers: a bitmask of `CAPABILITY_*`
    /// (`yas_client::wire::schema::fs`), 0 for servers that predate them.
    pub fn fs_capabilities(&self) -> u32 {
        self.family_limits(family::FS)
            .and_then(|limits| wire::Limits::from_extensions(&limits).ok())
            .map_or(0, |limits| limits.capabilities)
    }

    /// The most content one inline FS write carries on this server (`MAX_INLINE_BYTES`, as it
    /// advertises it): 0 before the FS family is negotiated.
    fn fs_inline_bytes(&self) -> usize {
        self.family_limits(family::FS)
            .and_then(|limits| wire::Limits::from_extensions(&limits).ok())
            .map_or(0, |limits| {
                (limits.max_inline_bytes as usize).min(wire::MAX_INLINE_BYTES)
            })
    }
}

/// File content read by [`FsRoot::read`] and friends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContent {
    /// The bytes read (all of them, or the requested prefix/range).
    pub bytes: Vec<u8>,
    /// The file's full length.
    pub len: u64,
    /// BLAKE3 of the **whole** file (verified when the whole file was read).
    pub hash: [u8; 32],
    /// Whether `bytes` is less than the whole file.
    pub truncated: bool,
}

/// Result of a successful write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// BLAKE3 of the new content (all zero for non-file mutations).
    pub hash: [u8; 32],
    /// The entry's new revision.
    pub revision: u64,
    /// Its new modification time.
    pub modified_unix_ns: i64,
}

/// Options for [`FsRoot::write`].
#[derive(Clone, Debug)]
pub struct WriteOptions {
    precondition: Precondition,
    create_parents: bool,
    mode: u32,
    durable: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteOptions {
    /// Overwrite unconditionally, mode 0644, no parent creation.
    pub fn new() -> Self {
        Self {
            precondition: Precondition::Any,
            create_parents: false,
            mode: 0o644,
            durable: false,
        }
    }

    /// Only write if the current entry matches: [`Precondition::Absent`]
    /// (create), [`Precondition::Hash`] (compare-and-swap on content),
    /// [`Precondition::Revision`], or [`Precondition::Any`].
    pub fn precondition(mut self, precondition: Precondition) -> Self {
        self.precondition = precondition;
        self
    }

    /// Create missing parent directories (`mkdir -p`).
    pub fn create_parents(mut self, create: bool) -> Self {
        self.create_parents = create;
        self
    }

    /// Permission bits for a new file (existing files keep theirs unless the
    /// server replaces the inode).
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }

    /// `fsync` the data and the directory before answering.
    pub fn durable(mut self, durable: bool) -> Self {
        self.durable = durable;
        self
    }
}

/// How a server spells a root's platform paths (its canonical path and the
/// absolute paths it reports). The paths [`FsRoot`]'s methods take are
/// relative and `/`-separated whatever the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathModel {
    /// POSIX: a path is bytes, `/` separates components.
    PosixBytes,
    /// Windows: a path is UTF-8 (the server converts it to UTF-16), with a
    /// drive or UNC prefix, and `\` separates components.
    WindowsUtf8,
}

/// Whether names under a root that differ only by case are the same entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaseBehavior {
    /// They are different entries (what a POSIX server says).
    Sensitive,
    /// They are the same entry, and names are not kept as written.
    Insensitive,
    /// They are the same entry, and a name keeps the case it was created
    /// with (what a Windows server says).
    PreservingInsensitive,
}

/// An open FS root. Dropping it closes the root on the server.
#[derive(Debug)]
pub struct FsRoot {
    client: Client,
    handle: u64,
    canonical_path: Vec<u8>,
    path_model: PathModel,
    case_behavior: CaseBehavior,
    closed: bool,
}

impl Client {
    /// Open a root at a platform path on the server (absolute, or relative
    /// to the server's working directory). `writable: false` opens it
    /// read-only (every mutation fails).
    pub async fn open_root(&self, path: impl AsRef<OsStr>, writable: bool) -> Result<FsRoot> {
        self.open_root_source(RootSource::PlatformPath(os_bytes(path.as_ref())), writable)
            .await
    }

    /// Open a root at a terminal's current directory (plus a relative suffix).
    pub async fn open_terminal_root(
        &self,
        terminal: u64,
        suffix: &str,
        writable: bool,
    ) -> Result<FsRoot> {
        self.open_root_source(
            RootSource::TerminalCwd {
                terminal_handle: terminal,
                suffix: wire_path(suffix)?,
            },
            writable,
        )
        .await
    }

    /// Open this session's staging directory, where a drag and drop leaves
    /// its files: the server makes it when first asked and removes it when
    /// the session ends, not when the root closes.
    pub async fn open_staging_root(&self, writable: bool) -> Result<FsRoot> {
        self.open_root_source(RootSource::Staging, writable).await
    }

    async fn open_root_source(&self, source: RootSource, writable: bool) -> Result<FsRoot> {
        let opened: OpenResult = self
            .request(
                family::FS,
                request_kind::OPEN,
                &Open {
                    flags: if writable {
                        0
                    } else {
                        schema::OPEN_READ_ONLY as u16
                    },
                    source,
                    extensions: Extensions::default(),
                },
            )
            .await?;
        Ok(FsRoot {
            client: self.clone(),
            handle: opened.root_handle,
            canonical_path: opened.canonical_path,
            path_model: if u64::from(opened.path_model) == schema::PATH_WINDOWS_UTF8 {
                PathModel::WindowsUtf8
            } else {
                PathModel::PosixBytes
            },
            case_behavior: match u64::from(opened.case_behavior) {
                schema::CASE_INSENSITIVE => CaseBehavior::Insensitive,
                schema::CASE_PRESERVING_INSENSITIVE => CaseBehavior::PreservingInsensitive,
                _ => CaseBehavior::Sensitive,
            },
            closed: false,
        })
    }
}

impl FsRoot {
    /// The server's canonical absolute path of the root.
    pub fn canonical_path(&self) -> &[u8] {
        &self.canonical_path
    }

    /// The root handle.
    pub fn handle(&self) -> u64 {
        self.handle
    }

    /// How the server spells this root's platform paths, the
    /// [`canonical_path`](Self::canonical_path) among them.
    pub fn path_model(&self) -> PathModel {
        self.path_model
    }

    /// Whether names under this root that differ only by case are the same
    /// entry.
    pub fn case_behavior(&self) -> CaseBehavior {
        self.case_behavior
    }

    /// Read a whole file (at most [`MAX_FILE_BYTES`]), verifying its hash.
    pub async fn read(&self, path: &str) -> Result<FileContent> {
        self.fetch(path, None, MAX_FILE_BYTES).await
    }

    /// Read at most `limit` bytes from the start of a file. Bytes past the
    /// limit are never transferred.
    pub async fn read_limited(&self, path: &str, limit: u64) -> Result<FileContent> {
        self.fetch(path, None, limit).await
    }

    /// Read `len` bytes starting at `offset`. The protocol has no ranged
    /// read: the server sends the file up to `offset + len` and the prefix is
    /// discarded here.
    pub async fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<FileContent> {
        let mut content = self.fetch(path, None, offset.saturating_add(len)).await?;
        let start = usize::try_from(offset.min(content.bytes.len() as u64)).unwrap_or(usize::MAX);
        content.bytes.drain(..start);
        content.truncated = offset > 0 || content.truncated;
        Ok(content)
    }

    async fn fetch(
        &self,
        path: &str,
        expected_hash: Option<[u8; 32]>,
        limit: u64,
    ) -> Result<FileContent> {
        let limit = limit.min(MAX_FILE_BYTES);
        let hook: Hook = Box::new(|prefix: &ResultPrefix| {
            ContentResult::decode(&prefix.body)
                .map(|result| delivery_routes(&result.content))
                .unwrap_or_default()
        });
        let mut reply = self
            .client
            .call_ok(
                family::FS,
                request_kind::FETCH,
                Fetch {
                    root_handle: self.handle,
                    path: wire_path(path)?,
                    expected_hash,
                    initial_receive_credit: limit.clamp(1, 16 * 1024 * 1024),
                    extensions: Extensions::default(),
                }
                .encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                Some(hook),
            )
            .await?;
        let result = ContentResult::decode(&reply.prefix.body)?;
        let len = result.content.byte_len;
        let hash = result.content.content_hash;
        if len <= limit {
            let frames = match &result.content.delivery {
                yas_wire::transfer::Delivery::Transfer(descriptor) => {
                    reply.take(Route::Transfer(descriptor.transfer_id))
                }
                yas_wire::transfer::Delivery::Inline(_) => None,
            };
            let bytes = collect_delivery(&self.client, result.content, frames, limit).await?;
            return Ok(FileContent {
                bytes,
                len,
                hash,
                truncated: false,
            });
        }
        // Longer than asked: take the prefix and stop the Transfer.
        let bytes = match result.content.delivery {
            yas_wire::transfer::Delivery::Inline(mut bytes) => {
                bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
                bytes
            }
            yas_wire::transfer::Delivery::Transfer(descriptor) => {
                let frames = reply
                    .take(Route::Transfer(descriptor.transfer_id))
                    .ok_or_else(|| Error::protocol("FS FETCH route missing"))?;
                ByteStream::new(self.client.clone(), descriptor, frames, limit.max(1))?
                    .read_prefix(limit)
                    .await?
                    .0
            }
        };
        Ok(FileContent {
            bytes,
            len,
            hash,
            truncated: true,
        })
    }

    /// Stat an entry; `None` if it does not exist. Like `lstat`: a symlink
    /// is described itself (its target in [`EntryKind::Symlink`]), because
    /// FS `READ STAT` never follows the last component.
    pub async fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let record = self.read_one(path, schema::READ_STAT as u16, false).await?;
        record
            .map(|content| Ok(Entry::from_wire(EntryRecord::decode(&content)?)))
            .transpose()
    }

    /// The BLAKE3 hash of a file's content; `None` if it does not exist.
    pub async fn hash(&self, path: &str) -> Result<Option<[u8; 32]>> {
        let record = self.read_one(path, schema::READ_HASH as u16, true).await?;
        record
            .map(|content| {
                content
                    .try_into()
                    .map_err(|_| Error::protocol("FS READ HASH is not 32 bytes"))
            })
            .transpose()
    }

    /// The raw target of a symlink.
    pub async fn read_link(&self, path: &str) -> Result<Vec<u8>> {
        self.read_one(path, schema::READ_LINK_TARGET as u16, false)
            .await?
            .ok_or_else(|| {
                Error::status_from(
                    "FS READ LINK_TARGET",
                    Status::NotFound,
                    Extensions::default(),
                )
            })
    }

    /// One directory level (`READ_LIST`, following a final symlink to the directory): each
    /// entry's raw name and own kind, hidden ones included, in no defined order. Needs
    /// `CAPABILITY_READ_LIST`; a failure carries its [`os_error`].
    pub async fn list_dir(&self, directory: &str) -> Result<Vec<DirEntry>> {
        let content = self
            .read_question(directory, schema::READ_LIST as u16, true)
            .await?;
        Ok(wire::ListEntry::decode_list(&content)?
            .into_iter()
            .map(|entry| DirEntry {
                name: entry.name,
                kind: Kind::from_wire(entry.kind),
            })
            .collect())
    }

    /// The canonical absolute platform path, every symlink resolved (`READ_REALPATH`).
    /// Needs `CAPABILITY_READ_REALPATH`; a failure carries its [`os_error`].
    pub async fn realpath(&self, path: &str) -> Result<Vec<u8>> {
        self.read_question(path, schema::READ_REALPATH as u16, true)
            .await
    }

    /// `stat(2)` (`follow`) or `lstat(2)` without reading or hashing anything
    /// (`READ_STAT_ONLY`). Needs `CAPABILITY_READ_STAT_ONLY`; a failure carries its
    /// [`os_error`].
    pub async fn stat_only(&self, path: &str, follow: bool) -> Result<StatOnly> {
        let content = self
            .read_question(path, schema::READ_STAT_ONLY as u16, follow)
            .await?;
        let stat = wire::StatOnly::decode(&content)?;
        Ok(StatOnly {
            kind: Kind::from_wire(stat.kind),
            mode: stat.mode,
            size: stat.size,
            modified_unix_ns: stat.modified_unix_ns,
        })
    }

    /// Ask one READ question whose failure may carry an OS error as its content.
    async fn read_question(&self, path: &str, kind: u16, follow: bool) -> Result<Vec<u8>> {
        let question = ReadQuestion {
            kind,
            flags: if follow {
                0
            } else {
                schema::READ_NO_FOLLOW as u16
            },
            path: wire_path(path)?,
        };
        let records = self.query(vec![question]).await?;
        let record = records
            .into_iter()
            .find_map(|record| match record {
                QueryRecord::Read(read) if read.question_index == 0 => Some(read),
                _ => None,
            })
            .ok_or_else(|| Error::protocol("FS READ omitted its answer"))?;
        match Status::from_code(record.status) {
            Status::Ok => Ok(record.content),
            status => {
                let mut extensions = Extensions::default();
                if let Ok(Some(os)) = record.os_error() {
                    extensions.0.push(os.result_extension()?);
                }
                Err(Error::status_from(
                    format!("FS READ {path}"),
                    status,
                    extensions,
                ))
            }
        }
    }

    async fn read_one(&self, path: &str, kind: u16, follow: bool) -> Result<Option<Vec<u8>>> {
        let question = ReadQuestion {
            kind,
            flags: if follow {
                0
            } else {
                schema::READ_NO_FOLLOW as u16
            },
            path: wire_path(path)?,
        };
        let records = self.query(vec![question]).await?;
        let record = records
            .into_iter()
            .find_map(|record| match record {
                QueryRecord::Read(read) if read.question_index == 0 => Some(read),
                _ => None,
            })
            .ok_or_else(|| Error::protocol("FS READ omitted its answer"))?;
        match Status::from_code(record.status) {
            Status::Ok => Ok(Some(record.content)),
            Status::NotFound => Ok(None),
            status => Err(Error::status_from(
                format!("FS READ {path}"),
                status,
                Extensions::default(),
            )),
        }
    }

    async fn query(&self, questions: Vec<ReadQuestion>) -> Result<Vec<QueryRecord>> {
        let hook: Hook = Box::new(
            |prefix: &ResultPrefix| match QueryPage::decode(&prefix.body) {
                Ok(QueryPage {
                    delivery: PageDelivery::Transfer(descriptor),
                    ..
                }) => vec![Route::Transfer(descriptor.transfer_id)],
                _ => Vec::new(),
            },
        );
        let mut reply = self
            .client
            .call_ok(
                family::FS,
                request_kind::READ,
                Read {
                    root_handle: self.handle,
                    initial_receive_credit: wire::MAX_QUERY_BYTES as u64,
                    questions,
                    extensions: Extensions::default(),
                }
                .encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                Some(hook),
            )
            .await?;
        let page = QueryPage::decode(&reply.prefix.body)?;
        if let Some(records) = page.inline_records()? {
            return Ok(records);
        }
        let PageDelivery::Transfer(descriptor) = page.delivery.clone() else {
            return Ok(Vec::new());
        };
        let frames = reply
            .take(Route::Transfer(descriptor.transfer_id))
            .ok_or_else(|| Error::protocol("FS READ route missing"))?;
        let items = collect_messages(
            &self.client,
            descriptor,
            frames,
            wire::MAX_QUERY_BYTES as u64,
            wire::MAX_QUERY_RECORDS,
        )
        .await?;
        let mut expected = 0u32;
        let mut typed = Vec::new();
        for item in items {
            let batch = QueryRecordBatch::decode(&item)?;
            if batch.first_record_index != expected {
                return Err(Error::protocol(
                    "FS query Transfer batches were not contiguous",
                ));
            }
            expected = expected
                .checked_add(batch.records.len() as u32)
                .ok_or_else(|| Error::protocol("FS query record index overflow"))?;
            typed.extend(batch.records);
        }
        typed
            .iter()
            .map(|record| Ok(QueryRecord::from_typed_record(record)?))
            .collect()
    }

    /// List a directory's immediate entries (hidden ones included; symlinks
    /// described, not followed). Sorted by name.
    pub async fn list(&self, directory: &str) -> Result<Vec<Entry>> {
        let path = wire_path(directory)?;
        // WATCH enumerates a root, so open one at the directory itself.
        let root = if path.components.is_empty() {
            None
        } else {
            Some(
                self.client
                    .open_root_source(RootSource::PlatformPath(self.platform_path(&path)), false)
                    .await?,
            )
        };
        let handle = root.as_ref().map_or(self.handle, |root| root.handle);
        let mut subscription = Subscription::open(
            &self.client,
            family::FS,
            request_kind::WATCH,
            request_kind::UNWATCH,
            Watch {
                root_handle: handle,
                flags: schema::WATCH_INCLUDE_HIDDEN as u16,
                settle_ms: 0,
                inline_max: 0,
                ignore_patterns: String::new(),
                state: StateWatch {
                    initial_credit: STATE_CREDIT,
                    resume: None,
                    extensions: Extensions::default(),
                },
            }
            .encode()?,
        )
        .await?
        .with_record_kinds(&[schema::RECORD_MOVE as u16]);
        let records = subscription.snapshot().await?;
        drop(subscription);
        let mut entries = Vec::new();
        for record in &records {
            if let StateMutation::Complete(entry) = StateMutation::decode_record(record)?
                && !entry.path.components.is_empty()
            {
                let mut entry = Entry::from_wire(entry);
                // Report paths relative to *this* root.
                let mut components = path.components.clone();
                components.append(&mut entry.components);
                entry.path = display_path(&Path {
                    components: components.clone(),
                });
                entry.components = components;
                entries.push(entry);
            }
        }
        entries.sort_by(|left, right| left.components.cmp(&right.components));
        if let Some(root) = root {
            root.close().await?;
        }
        Ok(entries)
    }

    /// Write a whole file. Small non-durable writes go in one `APPLY`; larger
    /// or durable ones are staged (`STAGE_WRITE`, upload, `COMMIT`) so the file
    /// is replaced atomically. A failed precondition is an [`Error::Status`]
    /// with status `CONFLICT`; [`conflict_detail`] describes the current entry.
    pub async fn write(
        &self,
        path: &str,
        content: &[u8],
        options: &WriteOptions,
    ) -> Result<Written> {
        if content.len() as u64 > MAX_FILE_BYTES {
            return Err(Error::invalid(format!(
                "file is {} bytes; the YAS limit is {MAX_FILE_BYTES}",
                content.len()
            )));
        }
        let wire_path = wire_path(path)?;
        if content.len() <= wire::MAX_INLINE_BYTES && !options.durable {
            return self
                .apply_one(ApplyItem::WriteInline {
                    path: wire_path,
                    precondition: options.precondition.clone(),
                    create_parents: options.create_parents,
                    mode: options.mode,
                    content: content.to_vec(),
                    in_place: false,
                })
                .await;
        }
        self.stage_and_commit(
            wire_path,
            content,
            options.precondition.clone(),
            if options.create_parents {
                schema::STAGE_CREATE_PARENTS as u16
            } else {
                0
            },
            options.mode,
            if options.durable {
                (schema::COMMIT_SYNC_DATA | schema::COMMIT_SYNC_DIRECTORY) as u16
            } else {
                0
            },
        )
        .await
    }

    /// Write a whole file as `open(2)` with `O_WRONLY|O_CREAT|O_TRUNC` and `write(2)` would,
    /// as Node's `writeFile` does: through a final symlink, an existing file keeping its inode,
    /// owner and mode, a new one mode 0666 less the server's umask. Not atomic: a failure can
    /// leave the file truncated. Content up to the server's inline limit goes in one `APPLY`
    /// (one round trip) where it offers `CAPABILITY_APPLY_IN_PLACE`; other content is staged
    /// (`STAGE_IN_PLACE`, needing `CAPABILITY_STAGE_IN_PLACE`; two round trips). A failure
    /// carries its [`os_error`] either way.
    pub async fn write_in_place(&self, path: &str, content: &[u8]) -> Result<Written> {
        self.write_in_place_if(path, content, Precondition::Any)
            .await
    }

    /// [`write_in_place`](Self::write_in_place), only while the entry at `path` matches
    /// `precondition`, which the server checks and writes under its mutation lock:
    /// [`Precondition::Hash`] of what was read makes a read, change and write a compare-and-swap
    /// on content. The entry checked is the one at `path` itself (a final symlink's own, not its
    /// target's), so name the file a link resolves to. A failed precondition is an
    /// [`Error::Status`] with status `CONFLICT` and no [`os_error`]; [`conflict_detail`]
    /// describes the current entry.
    pub async fn write_in_place_if(
        &self,
        path: &str,
        content: &[u8],
        precondition: Precondition,
    ) -> Result<Written> {
        if content.len() as u64 > MAX_FILE_BYTES {
            return Err(Error::invalid(format!(
                "file is {} bytes; the YAS limit is {MAX_FILE_BYTES}",
                content.len()
            )));
        }
        if content.len() <= self.client.fs_inline_bytes()
            && self.client.fs_capabilities() & schema::CAPABILITY_APPLY_IN_PLACE as u32 != 0
        {
            return self
                .apply_one(ApplyItem::WriteInline {
                    path: wire_path(path)?,
                    precondition,
                    create_parents: false,
                    mode: 0,
                    content: content.to_vec(),
                    in_place: true,
                })
                .await;
        }
        self.stage_and_commit(
            wire_path(path)?,
            content,
            precondition,
            schema::STAGE_IN_PLACE as u16,
            0,
            0,
        )
        .await
    }

    /// Stage `content` (`STAGE_WRITE`, upload) and `COMMIT` it.
    async fn stage_and_commit(
        &self,
        path: Path,
        content: &[u8],
        precondition: Precondition,
        stage_flags: u16,
        mode: u32,
        commit_flags: u16,
    ) -> Result<Written> {
        let hash = *blake3::hash(content).as_bytes();
        let hook: Hook = Box::new(|prefix: &ResultPrefix| {
            StageWriteResult::decode(&prefix.body)
                .map(|result| vec![Route::Transfer(result.descriptor.transfer_id)])
                .unwrap_or_default()
        });
        let mut reply = self
            .client
            .call_ok(
                family::FS,
                request_kind::STAGE_WRITE,
                StageWrite {
                    root_handle: self.handle,
                    path,
                    precondition,
                    flags: stage_flags,
                    mode,
                    byte_len: content.len() as u64,
                    content_hash: hash,
                    initial_receive_credit: content.len() as u64,
                    extensions: Extensions::default(),
                }
                .encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                Some(hook),
            )
            .await?;
        let staged = StageWriteResult::decode(&reply.prefix.body)?;
        let frames = reply
            .take(Route::Transfer(staged.descriptor.transfer_id))
            .ok_or_else(|| Error::protocol("FS STAGE_WRITE route missing"))?;
        let mut sink = ByteSink::new(self.client.clone(), staged.descriptor, frames)?;
        sink.write_all(content).await?;
        sink.finish().await?;
        let committed: CommitResult = self
            .client
            .request(
                family::FS,
                request_kind::COMMIT,
                &Commit {
                    staging_handle: staged.staging_handle,
                    operation_id: nonzero_id(),
                    flags: commit_flags,
                    extensions: Extensions::default(),
                },
            )
            .await?;
        Ok(Written {
            hash: committed.content_hash,
            revision: committed.entry_revision,
            modified_unix_ns: committed.modified_unix_ns,
        })
    }

    /// `mkdir(2)` of one directory (`mode` less the server's umask; 0: 0777): an existing
    /// directory is fine, anything else fails with the OS's error (`EEXIST` for an entry already
    /// there, `ENOENT` without its parent, …), which [`os_error`] names on servers offering
    /// `CAPABILITY_OS_ERROR`.
    pub async fn create_dir(&self, path: &str, mode: u32) -> Result<()> {
        self.apply_one(ApplyItem::Mkdir {
            path: wire_path(path)?,
            precondition: Precondition::Any,
            create_parents: false,
            mode,
        })
        .await
        .map(|_| ())
    }

    /// `unlink(2)` of whatever is there (a directory is removed only when empty, as
    /// `rmdir(2)`), failing with the OS's error, which [`os_error`] names on servers offering
    /// `CAPABILITY_OS_ERROR`.
    pub async fn unlink(&self, path: &str) -> Result<()> {
        self.remove(path, false, Precondition::Any).await
    }

    /// Create a directory. With `parents`, create missing parents too and
    /// succeed if it already exists as a directory (`mkdir -p`).
    pub async fn mkdir(&self, path: &str, parents: bool) -> Result<()> {
        let result = self
            .apply_one(ApplyItem::Mkdir {
                path: wire_path(path)?,
                precondition: Precondition::Absent,
                create_parents: parents,
                mode: 0o755,
            })
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if parents && error.is_conflict() => match self.stat(path).await? {
                Some(entry) if entry.is_dir() => Ok(()),
                _ => Err(error),
            },
            Err(error) => Err(error),
        }
    }

    /// Rename (move) an entry within the root.
    pub async fn rename(&self, from: &str, to: &str, create_parents: bool) -> Result<()> {
        self.apply_one(ApplyItem::Rename {
            from: wire_path(from)?,
            to: wire_path(to)?,
            precondition: Precondition::Any,
            create_parents,
        })
        .await
        .map(|_| ())
    }

    /// Remove an entry (directories with their contents when `recursive`).
    pub async fn remove(
        &self,
        path: &str,
        recursive: bool,
        precondition: Precondition,
    ) -> Result<()> {
        self.apply_one(ApplyItem::Remove {
            path: wire_path(path)?,
            precondition,
            flags: if recursive {
                schema::REMOVE_RECURSIVE as u16
            } else {
                0
            },
        })
        .await
        .map(|_| ())
    }

    /// Create a symbolic link at `path` pointing to `target` (raw bytes).
    pub async fn symlink(&self, path: &str, target: impl AsRef<OsStr>) -> Result<()> {
        self.apply_one(ApplyItem::Symlink {
            path: wire_path(path)?,
            target: os_bytes(target.as_ref()),
            precondition: Precondition::Absent,
            create_parents: false,
        })
        .await
        .map(|_| ())
    }

    /// Apply one mutation (the building block of the helpers above).
    pub async fn apply_one(&self, item: ApplyItem) -> Result<Written> {
        let path = item_path(&item);
        let result: ApplyResult = self
            .client
            .request(
                family::FS,
                request_kind::APPLY,
                &Apply {
                    root_handle: self.handle,
                    operation_id: nonzero_id(),
                    flags: 0,
                    items: vec![item],
                    extensions: Extensions::default(),
                },
            )
            .await?;
        let os = result
            .os_errors()
            .ok()
            .and_then(|errors| errors.into_iter().find(|(index, _)| *index == 0))
            .map(|(_, os)| os);
        let item = result
            .items
            .into_iter()
            .find(|item| item.index == 0)
            .ok_or_else(|| Error::protocol("FS APPLY omitted its only item Result"))?;
        let status = Status::from_code(item.status);
        if status == Status::Ok {
            return Ok(Written {
                hash: item.content_hash.unwrap_or([0; 32]),
                revision: item.entry_revision,
                modified_unix_ns: item.modified_unix_ns,
            });
        }
        let mut extensions = Extensions::default();
        if status == Status::Conflict
            && let Ok(extension) = (ConflictDetail {
                path,
                current_present: item.entry_revision != 0,
                current_entry_revision: item.entry_revision,
                modified_unix_ns: item.modified_unix_ns,
                current_hash: item.content_hash,
            })
            .result_extension()
        {
            extensions.0.push(extension);
        }
        if let Some(os) = os {
            extensions.0.push(os.result_extension()?);
        }
        Err(Error::Status {
            operation: "FS mutation".into(),
            status,
            detail: if item.detail.is_empty() {
                format!("status {}", item.status)
            } else {
                item.detail
            },
            extensions,
        })
    }

    /// Close the root now (dropping it does the same without waiting).
    pub async fn close(mut self) -> Result<()> {
        self.closed = true;
        self.client
            .call_ok(
                family::FS,
                request_kind::CLOSE,
                Close {
                    root_handle: self.handle,
                    extensions: Extensions::default(),
                }
                .encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                None,
            )
            .await
            .map(|_| ())
    }

    fn platform_path(&self, path: &Path) -> Vec<u8> {
        let separator = if self.path_model == PathModel::WindowsUtf8 {
            b'\\'
        } else {
            b'/'
        };
        let mut out = self.canonical_path.clone();
        for component in &path.components {
            if out.last() != Some(&separator) {
                out.push(separator);
            }
            out.extend_from_slice(component);
        }
        out
    }
}

impl Drop for FsRoot {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if let Ok(payload) = (Close {
            root_handle: self.handle,
            extensions: Extensions::default(),
        })
        .encode()
        {
            self.client
                .send_detached(family::FS, request_kind::CLOSE, payload);
        }
    }
}

/// The current entry described by a `CONFLICT` error from a write.
pub fn conflict_detail(error: &Error) -> Option<ConflictDetail> {
    match error {
        Error::Status {
            status: Status::Conflict,
            extensions,
            ..
        } => ConflictDetail::from_result_detail(extensions)
            .ok()
            .flatten(),
        _ => None,
    }
}

fn item_path(item: &ApplyItem) -> Path {
    match item {
        ApplyItem::WriteInline { path, .. }
        | ApplyItem::Mkdir { path, .. }
        | ApplyItem::Remove { path, .. }
        | ApplyItem::Symlink { path, .. } => path.clone(),
        ApplyItem::Rename { to, .. } => to.clone(),
        ApplyItem::Hardlink { target, .. } => target.clone(),
    }
}

/// Parse a root-relative, `/`-separated path. `""` and `"."` are the root;
/// empty and `.` components are skipped; `..`, NUL, backslashes and absolute
/// paths are rejected.
pub fn wire_path(text: &str) -> Result<Path> {
    if text.as_bytes().contains(&0) {
        return Err(Error::invalid("filesystem path contains NUL"));
    }
    if text.starts_with('/') {
        return Err(Error::invalid(format!(
            "path must be relative to the FS root: {text}"
        )));
    }
    let mut components = Vec::new();
    for component in text.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(Error::invalid(format!(
                    "path must stay below the FS root: {text}"
                )));
            }
            value if value.contains('\\') => {
                return Err(Error::invalid(format!(
                    "path component contains a separator: {value}"
                )));
            }
            value => components.push(value.as_bytes().to_vec()),
        }
    }
    Ok(Path { components })
}

fn display_path(path: &Path) -> String {
    if path.components.is_empty() {
        return ".".into();
    }
    path.components
        .iter()
        .map(|component| String::from_utf8_lossy(component))
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_relative_components() {
        assert_eq!(wire_path("").unwrap().components.len(), 0);
        assert_eq!(
            wire_path("./a//b/").unwrap().components,
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        assert!(wire_path("/etc").is_err());
        assert!(wire_path("a/../b").is_err());
        assert!(wire_path("a\\b").is_err());
    }
}
