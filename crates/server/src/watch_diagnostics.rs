//! Git and FS watch observability, shared by the journal and Client catalogue.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use yas_wire::{client, git, schema};

use crate::events::{EventLog, EventType, TRACE_CHUNK_BYTES};

static NEXT_RECORD: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct WatchInfo {
    pub resource_handle: u64,
    pub detail: client::AuxiliarySubscriptionDetail,
    pub timing: client::AuxiliarySubscriptionTiming,
}

impl WatchInfo {
    pub fn state(
        subscription_id: u32,
        request: &git::Watch,
        options: &git::WatchOptions,
        path: Vec<u8>,
    ) -> Self {
        let mut flags = u32::from(request.datasets);
        if request.datasets & schema::git::WATCH_STATUS as u16 != 0 {
            let (untracked, ignored) = super::yas_git_adapter::status_selection(options);
            if untracked {
                flags |= schema::client::GIT_WATCH_UNTRACKED as u32;
            }
            if ignored {
                flags |= schema::client::GIT_WATCH_IGNORED as u32;
            }
        }
        Self::new(
            yas_wire::family::GIT,
            request.repository_handle,
            subscription_id,
            &request.state,
            flags,
            path,
            super::yas_git_adapter::settle_delays(options),
        )
    }

    pub fn query(subscription_id: u32, request: &git::WatchQuery, path: Vec<u8>) -> Self {
        use git::QueryBody;
        let (kind, flags) = match &request.body {
            QueryBody::Resolve { .. } => (schema::git::QUERY_RESOLVE, 0),
            QueryBody::MergeBase { .. } => (schema::git::QUERY_MERGE_BASE, 0),
            QueryBody::Log { flags, .. } => (schema::git::QUERY_LOG, *flags),
            QueryBody::Tree { .. } => (schema::git::QUERY_TREE, 0),
            QueryBody::Blob { flags, .. } => (schema::git::QUERY_BLOB, *flags),
            QueryBody::Diff { flags, .. } => (schema::git::QUERY_DIFF, *flags),
            QueryBody::Patch { flags, .. } => (schema::git::QUERY_PATCH, *flags),
            QueryBody::Index { flags, .. } => (schema::git::QUERY_INDEX, *flags),
            QueryBody::Discover { flags, .. } => (schema::git::QUERY_DISCOVER, *flags),
            QueryBody::Blame { flags, .. } => (schema::git::QUERY_BLAME, *flags),
            QueryBody::Reflog { flags, .. } => (schema::git::QUERY_REFLOG, *flags),
            QueryBody::Worktrees => (schema::git::QUERY_WORKTREES, 0),
        };
        let flags = schema::client::GIT_QUERY_WATCH as u32
            | ((kind as u32) << schema::client::GIT_QUERY_KIND_SHIFT)
            | u32::from(flags);
        let options = request.options().expect("validated Git query watch");
        Self::new(
            yas_wire::family::GIT,
            request.repository_handle,
            subscription_id,
            &request.state,
            flags,
            path,
            super::yas_git_adapter::settle_delays(&options),
        )
    }

    pub fn fs(subscription_id: u32, request: &yas_wire::fs::Watch, path: Vec<u8>) -> Self {
        let settle = if request.settle_ms == 0 {
            yas_fssync::SyncOptions::default().latency.as_millis() as u16
        } else {
            request.settle_ms
        };
        Self::new(
            yas_wire::family::FS,
            request.root_handle,
            subscription_id,
            &request.state,
            u32::from(request.flags),
            path,
            (0, settle),
        )
    }

    fn new(
        family: u16,
        resource_handle: u64,
        subscription_id: u32,
        state: &yas_wire::state::Watch,
        request_flags: u32,
        resource: Vec<u8>,
        timing: (u16, u16),
    ) -> Self {
        Self {
            resource_handle,
            timing: client::AuxiliarySubscriptionTiming {
                family,
                subscription_id,
                refs_settle_ms: timing.0,
                settle_ms: timing.1,
            },
            detail: client::AuxiliarySubscriptionDetail {
                family,
                subscription_id,
                state_watch_flags: if state.resume.is_some() {
                    yas_wire::state::WATCH_RESUME
                } else {
                    0
                },
                request_flags,
                resource,
            },
        }
    }
}

/// Owned by the delivery task so UNWATCH, repository close, disconnect, and
/// cancellation (even before the first poll) all produce a matching stop.
pub(crate) struct WatchJournal {
    log: Arc<EventLog>,
    session: [u8; 16],
    info: Arc<WatchInfo>,
    _observer: Option<yas_fssync::backend::EventObserver>,
}

impl WatchJournal {
    pub fn start(
        log: Arc<EventLog>,
        session: [u8; 16],
        info: Arc<WatchInfo>,
        observe: impl FnOnce(
            Box<yas_fssync::backend::EventCallback>,
        ) -> Option<yas_fssync::backend::EventObserver>,
    ) -> Self {
        let event_log = log.clone();
        let event_info = info.clone();
        let callback: Box<yas_fssync::backend::EventCallback> = Box::new(move |event| {
            let kind = if event_info.detail.family == yas_wire::family::GIT {
                EventType::GitFsEvent
            } else {
                EventType::FsEvent
            };
            crate::yas_event!(event_log, kind, {
                let mut payload = identity(&session, &event_info);
                let (label, paths, rescan) = match event {
                    Ok(event) => (
                        format!("{:?}", event.kind),
                        event.paths.as_slice(),
                        event.need_rescan(),
                    ),
                    Err(error) => (format!("error: {error}"), error.paths.as_slice(), true),
                };
                put_bytes(&mut payload, label.as_bytes());
                payload.push(u8::from(rescan));
                payload.extend_from_slice(&(paths.len() as u32).to_le_bytes());
                for path in paths {
                    put_bytes(&mut payload, yas_fssync::escape_path(path).as_bytes());
                }
                payload
            });
        });
        let mut guard = Self {
            log,
            session,
            info,
            _observer: None,
        };
        guard.lifecycle(if guard.is_git() {
            EventType::GitWatchStart
        } else {
            EventType::FsWatchStart
        });
        guard._observer = observe(callback);
        guard
    }

    fn is_git(&self) -> bool {
        self.info.detail.family == yas_wire::family::GIT
    }

    fn identity(&self) -> Vec<u8> {
        identity(&self.session, &self.info)
    }

    fn lifecycle(&self, kind: EventType) {
        crate::yas_event!(self.log, kind, {
            let detail = &self.info.detail;
            let mut payload = self.identity();
            payload.extend_from_slice(&detail.request_flags.to_le_bytes());
            payload.extend_from_slice(&detail.state_watch_flags.to_le_bytes());
            payload.extend_from_slice(&self.info.timing.refs_settle_ms.to_le_bytes());
            payload.extend_from_slice(&self.info.timing.settle_ms.to_le_bytes());
            let length = detail.resource.len().min(u16::MAX as usize);
            payload.extend_from_slice(&(length as u16).to_le_bytes());
            payload.extend_from_slice(&detail.resource[..length]);
            payload
        });
    }

    pub fn state(&self, revision: u64, records: usize, bytes: u64) {
        let kind = if self.is_git() {
            EventType::GitState
        } else {
            EventType::FsState
        };
        crate::yas_event!(self.log, kind, {
            let mut payload = self.identity();
            payload.extend_from_slice(&revision.to_le_bytes());
            payload.extend_from_slice(&(records as u32).to_le_bytes());
            payload.extend_from_slice(&bytes.to_le_bytes());
            payload
        });
    }

    /// Complete canonical State records in bounded chunks. Recorded before enqueue; the
    /// matching *.state event confirms that the complete update was queued.
    pub fn records(&self, revision: u64, records: &[yas_wire::state::Record]) {
        let kind = if self.is_git() {
            EventType::GitRecord
        } else {
            EventType::FsRecord
        };
        if !self.log.enabled(kind) {
            return;
        }
        for record in records {
            let record_id = NEXT_RECORD.fetch_add(1, Ordering::Relaxed);
            for offset in (0..record.body.len().max(1)).step_by(TRACE_CHUNK_BYTES) {
                let mut payload = self.identity();
                payload.extend_from_slice(&revision.to_le_bytes());
                payload.extend_from_slice(&self.info.detail.request_flags.to_le_bytes());
                payload.extend_from_slice(&record_id.to_le_bytes());
                payload.extend_from_slice(&record.kind.wire().to_le_bytes());
                payload.extend_from_slice(&u16::from(record.required).to_le_bytes());
                payload.extend_from_slice(&(record.body.len() as u32).to_le_bytes());
                payload.extend_from_slice(&(offset as u32).to_le_bytes());
                payload.extend_from_slice(
                    &record.body[offset..record.body.len().min(offset + TRACE_CHUNK_BYTES)],
                );
                self.log.record(kind, 0, &payload);
            }
        }
    }
}

impl Drop for WatchJournal {
    fn drop(&mut self) {
        // Unregister before announcing stop. A callback whose weak reference
        // was already upgraded can still finish after this point.
        self._observer.take();
        self.lifecycle(if self.is_git() {
            EventType::GitWatchStop
        } else {
            EventType::FsWatchStop
        });
    }
}

fn identity(session: &[u8; 16], info: &WatchInfo) -> Vec<u8> {
    let mut payload = session.to_vec();
    payload.extend_from_slice(&info.resource_handle.to_le_bytes());
    payload.extend_from_slice(&info.detail.subscription_id.to_le_bytes());
    payload
}

fn put_bytes(payload: &mut Vec<u8>, bytes: &[u8]) {
    payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{DEFAULT_RING_SIZE, EVENT_RECORD_HEADER_LEN};

    #[test]
    fn large_query_records_are_reconstructible_within_live_batch_limits() {
        let log = EventLog::new(DEFAULT_RING_SIZE, yas_wire::events::ActivationSet::all());
        let state = yas_wire::state::Watch {
            initial_credit: 4096,
            resume: None,
            extensions: Default::default(),
        };
        let info = Arc::new(WatchInfo::new(
            yas_wire::family::GIT,
            7,
            9,
            &state,
            1,
            b"/root".to_vec(),
            (17, 23),
        ));
        let journal = WatchJournal::start(log.clone(), [3; 16], info, |_| None);
        let body = (0..70_000).map(|index| index as u8).collect::<Vec<_>>();
        journal.records(
            5,
            &[yas_wire::state::Record {
                kind: yas_wire::state::RecordKind::Replace,
                required: true,
                body: body.clone(),
            }],
        );
        let dump = log.dump();
        let mut bytes = &dump[crate::events::EVENT_DUMP_HEADER_LEN..];
        let mut reconstructed = Vec::new();
        let mut record_id = None;
        while !bytes.is_empty() {
            let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
            assert!(len < 4096);
            if u16::from_le_bytes(bytes[4..6].try_into().unwrap()) == EventType::GitRecord.id() {
                let payload = &bytes[EVENT_RECORD_HEADER_LEN..len];
                let id = u64::from_le_bytes(payload[40..48].try_into().unwrap());
                assert_eq!(*record_id.get_or_insert(id), id);
                assert_eq!(
                    u32::from_le_bytes(payload[52..56].try_into().unwrap()),
                    body.len() as u32
                );
                assert_eq!(
                    u32::from_le_bytes(payload[56..60].try_into().unwrap()),
                    reconstructed.len() as u32
                );
                reconstructed.extend_from_slice(&payload[60..]);
            }
            bytes = &bytes[len..];
        }
        assert_eq!(reconstructed, body);
    }

    #[tokio::test]
    async fn records_raw_notifications_and_cancellation_keep_watch_identity() {
        let log = EventLog::new(DEFAULT_RING_SIZE, yas_wire::events::ActivationSet::all());
        let (_, _, mut events) = log.native_snapshot_and_subscribe(false);
        let state = yas_wire::state::Watch {
            initial_credit: 4096,
            resume: None,
            extensions: Default::default(),
        };
        let info = Arc::new(WatchInfo::new(
            yas_wire::family::FS,
            7,
            9,
            &state,
            1,
            b"/root".to_vec(),
            (0, 17),
        ));
        let hub = yas_fssync::backend::EventObservers::default();
        let journal =
            WatchJournal::start(log, [3; 16], info, |callback| Some(hub.observe(callback)));
        hub.emit(&Ok(notify::Event::new(notify::EventKind::Create(
            notify::event::CreateKind::File,
        ))
        .add_path("/root/file".into())));
        journal.records(
            5,
            &[yas_wire::state::Record {
                kind: yas_wire::state::RecordKind::Remove,
                required: true,
                body: vec![11, 12],
            }],
        );
        journal.state(5, 1, 42);
        let task = tokio::spawn(async move {
            let _journal = journal;
            std::future::pending::<()>().await;
        });
        task.abort();
        let _ = task.await;
        let mut kinds = Vec::new();
        while let Ok(event) = events.try_recv() {
            kinds.push(u16::from_le_bytes(event[4..6].try_into().unwrap()));
            let payload = &event[EVENT_RECORD_HEADER_LEN..];
            assert_eq!(&payload[..16], &[3; 16]);
            assert_eq!(&payload[16..24], &7u64.to_le_bytes());
            assert_eq!(&payload[24..28], &9u32.to_le_bytes());
        }
        assert_eq!(
            kinds,
            [
                EventType::FsWatchStart,
                EventType::FsEvent,
                EventType::FsRecord,
                EventType::FsState,
                EventType::FsWatchStop
            ]
            .map(EventType::id)
        );
        hub.emit(&Ok(notify::Event::new(notify::EventKind::Any)));
        assert!(events.try_recv().is_err());
    }
}
