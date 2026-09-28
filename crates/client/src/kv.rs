//! The server's key-value store (the KV family) and its environment (Env).
//!
//! KV is one flat store per server instance, shared by every session and
//! persisted across restarts (keys ≤ 256 bytes, values ≤ 4 MiB). A
//! [`KvNamespace`] is a view below a key prefix. Writes are compare-and-swap
//! on content hash and/or modification revision.

use yas_wire::{
    Decode, Encode, Extensions,
    core::{ResultPrefix, Status},
    env::{self as env_wire, Delivery as EnvDelivery, GetResult as EnvGetResult},
    family,
    kv::{
        self as wire, Close, Delete, EntryRecord, Get, GetResult, MutationResult, Open, OpenResult,
        Put, RemovedEntry, StageValue, StageValueResult, ValueSource, Watch, request_kind,
    },
    state::{Phase, RecordKind, Watch as StateWatch},
};

pub use yas_wire::kv::Precondition as KvPrecondition;

use crate::client::{Client, DEFAULT_REQUEST_TIMEOUT, Hook, Route};
use crate::error::{Error, Result};
use crate::process::nonzero_id;
use crate::state::{STATE_CREDIT, Subscription};
use crate::transfer::{ByteSink, collect_delivery, collect_messages, delivery_routes};

/// A value read from KV.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvValue {
    /// The bytes.
    pub value: Vec<u8>,
    /// BLAKE3 of the value.
    pub hash: [u8; 32],
    /// The store revision of its last modification.
    pub revision: u64,
}

/// The outcome of a KV write or delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvWrite {
    /// `Ok` or `Conflict` (the precondition failed; the fields then describe
    /// the current entry).
    pub status: Status,
    /// Modification revision (new on success, current on conflict).
    pub revision: u64,
    /// Modification time, Unix nanoseconds.
    pub modified_unix_ns: i64,
    /// BLAKE3 of the value.
    pub hash: [u8; 32],
    /// Value length.
    pub len: u64,
}

impl KvWrite {
    /// Whether the write happened.
    pub fn applied(&self) -> bool {
        self.status == Status::Ok
    }
}

/// One KV entry in a listing or watch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvEntry {
    /// Key relative to the namespace prefix.
    pub key: Vec<u8>,
    /// BLAKE3 of the value.
    pub hash: [u8; 32],
    /// Value length.
    pub len: u64,
    /// Modification revision.
    pub revision: u64,
    /// Modification time, Unix nanoseconds.
    pub modified_unix_ns: i64,
    /// The value, when small enough to be inlined in the watch.
    pub value: Option<Vec<u8>>,
}

impl KvEntry {
    fn from_wire(record: EntryRecord) -> Self {
        Self {
            key: record.relative_key,
            hash: record.content_hash,
            len: record.byte_len,
            revision: record.modification_revision,
            modified_unix_ns: record.modified_unix_ns,
            value: record.inline_value,
        }
    }
}

/// A change seen by [`KvWatch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvChange {
    /// Every entry below the prefix (first event, and after a reset).
    Snapshot(Vec<KvEntry>),
    /// An entry was created or replaced.
    Put(KvEntry),
    /// An entry was deleted.
    Deleted {
        /// Its key.
        key: Vec<u8>,
        /// The revision of the deletion.
        revision: u64,
    },
}

/// A view of the store below a key prefix. Dropping it closes it.
#[derive(Debug)]
pub struct KvNamespace {
    client: Client,
    handle: u64,
    closed: bool,
}

impl Client {
    /// Open the KV namespace below `prefix` (`b""` for the whole store).
    pub async fn kv(&self, prefix: &[u8]) -> Result<KvNamespace> {
        let opened: OpenResult = self
            .request(
                family::KV,
                request_kind::OPEN,
                &Open {
                    prefix: prefix.to_vec(),
                    extensions: Extensions::default(),
                },
            )
            .await?;
        Ok(KvNamespace {
            client: self.clone(),
            handle: opened.namespace_handle,
            closed: false,
        })
    }

    /// The server process's complete environment (unredacted: it may hold
    /// credentials), as raw key/value bytes.
    pub async fn environment(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let hook: Hook =
            Box::new(
                |prefix: &ResultPrefix| match EnvGetResult::decode(&prefix.body) {
                    Ok(EnvGetResult {
                        delivery: EnvDelivery::Transfer(descriptor),
                        ..
                    }) => vec![Route::Transfer(descriptor.transfer_id)],
                    _ => Vec::new(),
                },
            );
        let mut reply = self
            .call_ok(
                family::ENV,
                env_wire::request_kind::GET,
                env_wire::Get {
                    initial_receive_credit: env_wire::MAX_TOTAL_DATA_BYTES as u64 * 2,
                    extensions: Extensions::default(),
                }
                .encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                Some(hook),
            )
            .await?;
        let result = EnvGetResult::decode(&reply.prefix.body)?;
        let entries = match result.delivery {
            EnvDelivery::Inline(entries) => entries,
            EnvDelivery::Transfer(descriptor) => {
                let frames = reply
                    .take(Route::Transfer(descriptor.transfer_id))
                    .ok_or_else(|| Error::protocol("Env GET route missing"))?;
                let items = collect_messages(
                    self,
                    descriptor,
                    frames,
                    env_wire::MAX_TOTAL_DATA_BYTES as u64 * 2,
                    env_wire::MAX_ENTRIES,
                )
                .await?;
                let mut assembler =
                    env_wire::SnapshotAssembler::new(result.entry_count, result.total_data_bytes)?;
                for item in items {
                    assembler.push(env_wire::SnapshotBatch::decode(&item)?)?;
                }
                assembler.finish()?
            }
        };
        Ok(entries
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect())
    }

    /// One variable of the server's environment.
    pub async fn env_var(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .environment()
            .await?
            .into_iter()
            .find(|(name, _)| name == key.as_bytes())
            .map(|(_, value)| value))
    }
}

impl KvNamespace {
    /// Read a value; `None` if absent.
    pub async fn get(&self, key: &[u8]) -> Result<Option<KvValue>> {
        let hook: Hook = Box::new(|prefix: &ResultPrefix| {
            GetResult::decode(&prefix.body)
                .map(|result| delivery_routes(&result.value))
                .unwrap_or_default()
        });
        let mut reply = self
            .client
            .call(
                family::KV,
                request_kind::GET,
                Get {
                    namespace_handle: self.handle,
                    relative_key: key.to_vec(),
                    initial_receive_credit: wire::MAX_VALUE_BYTES as u64,
                    extensions: Extensions::default(),
                }
                .encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                Some(hook),
            )
            .await?;
        match reply.prefix.status {
            Status::Ok => {}
            Status::NotFound => return Ok(None),
            status => {
                return Err(Error::status_from(
                    "KV GET",
                    status,
                    reply.prefix.detail.clone(),
                ));
            }
        }
        let result = GetResult::decode(&reply.prefix.body)?;
        let hash = result.value.content_hash;
        let frames = match &result.value.delivery {
            yas_wire::transfer::Delivery::Transfer(descriptor) => {
                reply.take(Route::Transfer(descriptor.transfer_id))
            }
            yas_wire::transfer::Delivery::Inline(_) => None,
        };
        let value = collect_delivery(
            &self.client,
            result.value,
            frames,
            wire::MAX_VALUE_BYTES as u64,
        )
        .await?;
        Ok(Some(KvValue {
            value,
            hash,
            revision: result.modification_revision,
        }))
    }

    /// Write a value if `precondition` holds. A failed precondition is not an
    /// error: check [`KvWrite::applied`].
    pub async fn put(
        &self,
        key: &[u8],
        value: &[u8],
        precondition: KvPrecondition,
        durable: bool,
    ) -> Result<KvWrite> {
        if value.len() > wire::MAX_VALUE_BYTES {
            return Err(Error::invalid(format!(
                "KV value is {} bytes; the YAS limit is {}",
                value.len(),
                wire::MAX_VALUE_BYTES
            )));
        }
        let source = if value.len() <= wire::MAX_INLINE_BYTES {
            ValueSource::Inline(value.to_vec())
        } else {
            let hash = *blake3::hash(value).as_bytes();
            let hook: Hook = Box::new(|prefix: &ResultPrefix| {
                StageValueResult::decode(&prefix.body)
                    .map(|result| vec![Route::Transfer(result.transfer.transfer_id)])
                    .unwrap_or_default()
            });
            let mut reply = self
                .client
                .call_ok(
                    family::KV,
                    request_kind::STAGE_VALUE,
                    StageValue {
                        byte_len: value.len() as u64,
                        content_hash: hash,
                        extensions: Extensions::default(),
                    }
                    .encode()?,
                    Some(DEFAULT_REQUEST_TIMEOUT),
                    Some(hook),
                )
                .await?;
            let stage = StageValueResult::decode(&reply.prefix.body)?;
            let frames = reply
                .take(Route::Transfer(stage.transfer.transfer_id))
                .ok_or_else(|| Error::protocol("KV STAGE_VALUE route missing"))?;
            let mut sink = ByteSink::new(self.client.clone(), stage.transfer, frames)?;
            sink.write_all(value).await?;
            sink.finish().await?;
            ValueSource::Staged(stage.staging_handle)
        };
        let result: MutationResult = self
            .client
            .request(
                family::KV,
                request_kind::PUT,
                &Put {
                    namespace_handle: self.handle,
                    operation_id: nonzero_id(),
                    durable,
                    relative_key: key.to_vec(),
                    precondition,
                    value: source,
                    extensions: Extensions::default(),
                },
            )
            .await?;
        Ok(mutation(result))
    }

    /// Delete a key if `precondition` holds.
    pub async fn delete(
        &self,
        key: &[u8],
        precondition: KvPrecondition,
        durable: bool,
    ) -> Result<KvWrite> {
        let result: MutationResult = self
            .client
            .request(
                family::KV,
                request_kind::DELETE,
                &Delete {
                    namespace_handle: self.handle,
                    operation_id: nonzero_id(),
                    durable,
                    relative_key: key.to_vec(),
                    precondition,
                    extensions: Extensions::default(),
                },
            )
            .await?;
        Ok(mutation(result))
    }

    /// Every entry below the prefix right now (values inlined when small).
    pub async fn list(&self) -> Result<Vec<KvEntry>> {
        let mut watch = self.watch(true).await?;
        match watch.next().await? {
            KvChange::Snapshot(entries) => Ok(entries),
            _ => Err(Error::protocol("KV WATCH did not start with a snapshot")),
        }
    }

    /// Watch the entries below the prefix: a snapshot, then changes. With
    /// `values`, values up to the inline limit are included.
    pub async fn watch(&self, values: bool) -> Result<KvWatch> {
        let subscription = Subscription::open(
            &self.client,
            family::KV,
            request_kind::WATCH,
            request_kind::UNWATCH,
            Watch {
                namespace_handle: self.handle,
                inline_max: if values {
                    wire::MAX_INLINE_BYTES as u32
                } else {
                    0
                },
                state: StateWatch {
                    initial_credit: STATE_CREDIT,
                    resume: None,
                    extensions: Extensions::default(),
                },
            }
            .encode()?,
        )
        .await?;
        Ok(KvWatch {
            subscription,
            snapshot: Vec::new(),
            pending: std::collections::VecDeque::new(),
        })
    }

    /// Close the namespace now.
    pub async fn close(mut self) -> Result<()> {
        self.closed = true;
        self.client
            .call_ok(
                family::KV,
                request_kind::CLOSE,
                Close {
                    namespace_handle: self.handle,
                    extensions: Extensions::default(),
                }
                .encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                None,
            )
            .await
            .map(|_| ())
    }
}

impl Drop for KvNamespace {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if let Ok(payload) = (Close {
            namespace_handle: self.handle,
            extensions: Extensions::default(),
        })
        .encode()
        {
            self.client
                .send_detached(family::KV, request_kind::CLOSE, payload);
        }
    }
}

/// A live view of a KV namespace.
#[derive(Debug)]
pub struct KvWatch {
    subscription: Subscription,
    snapshot: Vec<KvEntry>,
    pending: std::collections::VecDeque<KvChange>,
}

impl KvWatch {
    /// The next change.
    pub async fn next(&mut self) -> Result<KvChange> {
        loop {
            if let Some(change) = self.pending.pop_front() {
                return Ok(change);
            }
            let event = self.subscription.next().await?;
            match event.phase {
                Phase::SnapshotBegin | Phase::Reset => {
                    self.snapshot.clear();
                    for record in &event.records {
                        if let KvChange::Put(entry) = change(record)? {
                            self.snapshot.push(entry);
                        }
                    }
                }
                Phase::SnapshotRecords => {
                    for record in &event.records {
                        if let KvChange::Put(entry) = change(record)? {
                            self.snapshot.push(entry);
                        }
                    }
                }
                Phase::SnapshotEnd => {
                    for record in &event.records {
                        if let KvChange::Put(entry) = change(record)? {
                            self.snapshot.push(entry);
                        }
                    }
                    return Ok(KvChange::Snapshot(std::mem::take(&mut self.snapshot)));
                }
                Phase::Delta => {
                    for record in &event.records {
                        self.pending.push_back(change(record)?);
                    }
                }
            }
        }
    }
}

fn change(record: &yas_wire::state::Record) -> Result<KvChange> {
    match record.kind {
        RecordKind::Remove => {
            let removed = RemovedEntry::from_state_record(record)?;
            Ok(KvChange::Deleted {
                key: removed.relative_key,
                revision: removed.modification_revision,
            })
        }
        _ => Ok(KvChange::Put(KvEntry::from_wire(
            wire::entry_from_state_record(record)?,
        ))),
    }
}

fn mutation(result: MutationResult) -> KvWrite {
    KvWrite {
        status: result.status,
        revision: result.modification_revision,
        modified_unix_ns: result.modified_unix_ns,
        hash: result.content_hash,
        len: result.byte_len,
    }
}
