//! Shared native protocol tracing, at the successful read/write boundary.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use yas_wire::{Class, Frame, family, schema};

use crate::events::{EventLog, EventType, TRACE_CHUNK_BYTES as CHUNK_BYTES};

// Each chunk fits even the smallest journal. Full bodies remain reconstructible
// using trace ID, record index and offset, without allocating a second frame.
static NEXT_TRACE: AtomicU64 = AtomicU64::new(1);

// Borrowed framing only: diagnostics must preserve unknown family records and
// avoid copying an entire State event just to log its individual records.
struct Decoder<'a>(&'a [u8]);

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }
    fn take(&mut self, count: usize) -> yas_wire::Result<&'a [u8]> {
        let bytes = self.0.get(..count).ok_or(yas_wire::Error::Truncated)?;
        self.0 = &self.0[count..];
        Ok(bytes)
    }
    fn u16(&mut self) -> yas_wire::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> yas_wire::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn rest(&self) -> &'a [u8] {
        self.0
    }
    fn finish(self) -> yas_wire::Result<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(yas_wire::Error::TrailingBytes(self.0.len()))
        }
    }
    #[cfg(test)]
    fn remaining(&self) -> usize {
        self.0.len()
    }
}

impl EventLog {
    pub(crate) fn native_frame(
        &self,
        kind: EventType,
        session: &[u8; 16],
        frame: &Frame,
        wire_bytes: u64,
    ) {
        // Following the event journal must not generate its own next event.
        if frame.header.family == family::EVENTS {
            return;
        }
        let (payload_kind, record_kind) = if matches!(
            kind,
            EventType::NativeFrameRead | EventType::NativeDatagramRead
        ) {
            (
                EventType::NativePayloadRead,
                EventType::NativeStateRecordRead,
            )
        } else {
            (
                EventType::NativePayloadWrite,
                EventType::NativeStateRecordWrite,
            )
        };
        if ![kind, payload_kind, record_kind]
            .into_iter()
            .any(|kind| self.enabled(kind))
        {
            return;
        }
        let header = &frame.header;
        let operation = schema::FAMILIES
            .iter()
            .find(|family| family.id == header.family)
            .and_then(|family| {
                family.operations.iter().find(|op| {
                    op.kind == header.kind
                        && op.class
                            == if header.class == Class::Result {
                                Class::Request as u8
                            } else {
                                header.class as u8
                            }
                })
            });
        let name = operation.map_or("", |op| op.name);
        let state = if header.class == Class::Event {
            if name == "STATE" {
                Some(frame.payload.as_slice())
            } else if name == "QUERY_STATE" && header.family == family::GIT {
                let mut cursor = Decoder::new(&frame.payload);
                (|| {
                    cursor.u32().ok()?;
                    let len = cursor.u32().ok()? as usize;
                    cursor.take(len).ok()
                })()
            } else {
                None
            }
        } else {
            None
        };
        let watch = if header.class == Class::Result && matches!(name, "WATCH" | "WATCH_QUERY") {
            let mut result = Decoder::new(&frame.payload);
            (|| {
                if result.u16().ok()? != 0 {
                    return None;
                }
                result.u16().ok()?;
                let detail_len = result.u32().ok()? as usize;
                result.take(detail_len).ok()?;
                result.u32().ok()
            })()
            .unwrap_or(0)
        } else if header.class != Class::Result
            && matches!(
                name,
                "STATE"
                    | "STATE_ACK"
                    | "QUERY_STATE"
                    | "QUERY_STATE_ACK"
                    | "UNWATCH"
                    | "UNWATCH_QUERY"
            )
        {
            frame
                .payload
                .get(..4)
                .map_or(0, |bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
        } else {
            0
        };
        let mut identity = session.to_vec();
        identity.extend_from_slice(&header.family.to_le_bytes());
        identity.extend_from_slice(&header.kind.to_le_bytes());
        identity.push(header.class as u8);
        let datagram = matches!(
            kind,
            EventType::NativeDatagramRead
                | EventType::NativeDatagramWrite
                | EventType::NativeDatagramDrop
        );
        identity.push(
            u8::from(header.sensitive)
                | (u8::from(header.compressed) << 1)
                | (u8::from(datagram) << 2),
        );
        identity.extend_from_slice(&header.request_id.unwrap_or(0).to_le_bytes());
        identity.extend_from_slice(&watch.to_le_bytes());
        identity.extend_from_slice(&(frame.payload.len() as u32).to_le_bytes());
        identity.extend_from_slice(&wire_bytes.to_le_bytes());
        identity.extend_from_slice(&NEXT_TRACE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        crate::yas_event!(self, kind, {
            let mut payload = identity.clone();
            // A bounded prefix permits readable status, State phase/revisions,
            // ACK credit and transfer identity without enabling payload capture.
            let prefix = if header.class == Class::Result {
                frame.payload.get(..4)
            } else if state.is_some() {
                state.and_then(|bytes| bytes.get(..26))
            } else if matches!(name, "STATE_ACK" | "QUERY_STATE_ACK") {
                frame.payload.get(..20)
            } else if header.family == family::TRANSFER {
                frame.payload.get(..4)
            } else {
                None
            };
            payload.extend_from_slice(prefix.unwrap_or_default());
            payload
        });
        if kind != EventType::NativeDatagramDrop && self.enabled(payload_kind) {
            self.native_chunks(payload_kind, &identity, &frame.payload);
        }
        if self.enabled(record_kind)
            && let Some(state) = state
        {
            // Read borrowed typed records; do not discard unknown optional kinds.
            let mut cursor = Decoder::new(state);
            let result = (|| -> yas_wire::Result<()> {
                let state_header = cursor.take(24)?;
                let count = cursor.u16()?;
                for index in 0..count {
                    let len = cursor.u32()? as usize;
                    let mut record = Decoder::new(cursor.take(len)?);
                    let typed_header = record.take(4)?;
                    let mut metadata = identity.clone();
                    metadata.extend_from_slice(state_header);
                    metadata.extend_from_slice(&index.to_le_bytes());
                    metadata.extend_from_slice(typed_header);
                    self.native_chunks(record_kind, &metadata, record.rest());
                }
                cursor.finish()
            })();
            if let Err(error) = result {
                self.native_error(session, "state trace decode", &error);
            }
        }
    }

    fn native_chunks(&self, kind: EventType, metadata: &[u8], body: &[u8]) {
        for offset in (0..body.len().max(1)).step_by(CHUNK_BYTES) {
            let end = body.len().min(offset + CHUNK_BYTES);
            let mut payload = metadata.to_vec();
            payload.extend_from_slice(&(body.len() as u32).to_le_bytes());
            payload.extend_from_slice(&(offset as u32).to_le_bytes());
            payload.extend_from_slice(&body[offset..end]);
            self.record(kind, 0, &payload);
        }
    }

    pub(crate) fn native_error(
        &self,
        session: &[u8; 16],
        stage: &str,
        error: &dyn std::fmt::Display,
    ) {
        crate::yas_event!(self, EventType::NativeError, {
            let mut payload = session.to_vec();
            put_text(&mut payload, stage);
            put_text(&mut payload, &error.to_string());
            payload
        });
    }
}

pub(crate) struct ConnectionJournal {
    log: Arc<EventLog>,
    session: [u8; 16],
    started: Instant,
    received: Arc<AtomicU64>,
    sent: Arc<AtomicU64>,
    pub reason: String,
}

impl ConnectionJournal {
    pub fn new(
        log: Arc<EventLog>,
        session: [u8; 16],
        name: &str,
        release: &str,
        received: Arc<AtomicU64>,
        sent: Arc<AtomicU64>,
    ) -> Self {
        crate::yas_event!(log, EventType::NativeConnect, {
            let mut payload = session.to_vec();
            put_text(&mut payload, name);
            put_text(&mut payload, release);
            payload
        });
        Self {
            log,
            session,
            started: Instant::now(),
            received,
            sent,
            reason: "connection task ended before dispatch completed".to_owned(),
        }
    }
}

impl Drop for ConnectionJournal {
    fn drop(&mut self) {
        crate::yas_event!(self.log, EventType::NativeDisconnect, {
            let mut payload = self.session.to_vec();
            payload.extend_from_slice(
                &(self.started.elapsed().as_micros().min(u64::MAX as u128) as u64).to_le_bytes(),
            );
            payload.extend_from_slice(&self.received.load(Ordering::Relaxed).to_le_bytes());
            payload.extend_from_slice(&self.sent.load(Ordering::Relaxed).to_le_bytes());
            put_text(&mut payload, &self.reason);
            payload
        });
    }
}

fn put_text(payload: &mut Vec<u8>, text: &str) {
    let bytes = &text.as_bytes()[..text.floor_char_boundary(text.len().min(1024))];
    payload.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    payload.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{DEFAULT_RING_SIZE, EVENT_DUMP_HEADER_LEN, EVENT_RECORD_HEADER_LEN};
    use yas_wire::{
        Encode, FrameHeader,
        events::ActivationSet,
        state::{Phase, Record, RecordKind, StateEvent},
    };

    fn frame(family: u16, kind: u16, class: Class, payload: Vec<u8>) -> Frame {
        Frame {
            header: FrameHeader {
                family,
                kind,
                class,
                sensitive: false,
                compressed: false,
                request_id: (class != Class::Event).then_some(7),
            },
            payload,
        }
    }

    fn records(log: &EventLog) -> Vec<(u16, Vec<u8>)> {
        let dump = log.dump();
        let mut cursor = Decoder::new(&dump[EVENT_DUMP_HEADER_LEN..]);
        let mut records = Vec::new();
        while cursor.remaining() != 0 {
            let len = cursor.u32().unwrap() as usize;
            let record = cursor.take(len - 4).unwrap();
            records.push((
                u16::from_le_bytes(record[..2].try_into().unwrap()),
                record[EVENT_RECORD_HEADER_LEN - 4..].to_vec(),
            ));
        }
        records
    }

    #[test]
    fn all_families_have_bidirectional_traces_without_journal_feedback() {
        let log = EventLog::new(DEFAULT_RING_SIZE, ActivationSet::all());
        for family in schema::FAMILIES {
            for class in [Class::Request, Class::Event, Class::Result] {
                for direction in [EventType::NativeFrameRead, EventType::NativeFrameWrite] {
                    log.native_frame(
                        direction,
                        &[1; 16],
                        &frame(family.id, u16::MAX, class, Vec::new()),
                        13,
                    );
                }
            }
        }
        let records = records(&log);
        assert_eq!(records.len(), (schema::FAMILIES.len() - 1) * 6 * 2);
        for (_, payload) in &records {
            assert_ne!(
                u16::from_le_bytes(payload[16..18].try_into().unwrap()),
                family::EVENTS
            );
            assert_eq!(&payload[..16], &[1; 16]);
        }
    }

    #[test]
    fn payload_chunks_reconstruct_large_binary_frames() {
        let log = EventLog::new(DEFAULT_RING_SIZE, ActivationSet::all());
        let bytes = (0..10_000).map(|i| i as u8).collect::<Vec<_>>();
        log.native_frame(
            EventType::NativeFrameWrite,
            &[2; 16],
            &frame(family::TRANSFER, 0, Class::Event, bytes.clone()),
            10_020,
        );
        let records = records(&log);
        let mut reconstructed = Vec::new();
        let trace = &records[0].1[42..50];
        for (_, payload) in records
            .iter()
            .filter(|(kind, _)| *kind == EventType::NativePayloadWrite.id())
        {
            assert_eq!(&payload[42..50], trace);
            assert_eq!(
                u32::from_le_bytes(payload[50..54].try_into().unwrap()),
                bytes.len() as u32
            );
            assert_eq!(
                u32::from_le_bytes(payload[54..58].try_into().unwrap()),
                reconstructed.len() as u32
            );
            assert!(payload.len() <= 58 + CHUNK_BYTES);
            reconstructed.extend_from_slice(&payload[58..]);
        }
        assert_eq!(reconstructed, bytes);
    }

    #[test]
    fn datagrams_distinguish_queue_drops_and_watch_results_report_assigned_ids() {
        use yas_wire::{
            Decode,
            core::{ResultPrefix, Status},
            state::{WatchMode, WatchResult},
        };
        let log = EventLog::new(DEFAULT_RING_SIZE, ActivationSet::all());
        let packet = frame(
            family::MEDIA,
            schema::media::event::FRAME,
            Class::Event,
            vec![1],
        );
        log.native_frame(EventType::NativeDatagramRead, &[2; 16], &packet, 6);
        log.native_frame(EventType::NativeDatagramWrite, &[2; 16], &packet, 6);
        log.native_frame(EventType::NativeDatagramDrop, &[2; 16], &packet, 6);
        let observed = records(&log);
        assert_eq!(observed.len(), 5);
        assert!(observed.iter().all(|(_, payload)| payload[21] & 4 != 0));
        let result = ResultPrefix {
            status: Status::Ok,
            detail: Default::default(),
            body: WatchResult {
                subscription_id: 42,
                mode: WatchMode::Snapshot,
                current_revision: 1,
                extensions: Default::default(),
            }
            .encode()
            .unwrap(),
        }
        .encode()
        .unwrap();
        assert!(ResultPrefix::decode(&result).is_ok());
        log.native_frame(
            EventType::NativeFrameWrite,
            &[2; 16],
            &frame(
                family::KV,
                schema::kv::request::WATCH,
                Class::Result,
                result,
            ),
            50,
        );
        let observed = records(&log);
        assert_eq!(&observed[5].1[26..30], &42u32.to_le_bytes());
    }

    #[test]
    fn state_records_include_unknown_kinds_and_nested_git_queries() {
        let log = EventLog::new(DEFAULT_RING_SIZE, ActivationSet::all());
        let event = StateEvent {
            subscription_id: 9,
            phase: Phase::Delta,
            flags: 0,
            from_revision: 4,
            to_revision: 5,
            records: vec![
                Record {
                    kind: RecordKind::Family(99),
                    required: false,
                    body: b"unknown optional".to_vec(),
                },
                Record {
                    kind: RecordKind::Remove,
                    required: true,
                    body: vec![],
                },
            ],
        };
        let bytes = event.encode().unwrap();
        log.native_frame(
            EventType::NativeFrameWrite,
            &[3; 16],
            &frame(
                family::KV,
                schema::kv::event::STATE,
                Class::Event,
                bytes.clone(),
            ),
            100,
        );
        let mut query = 9u32.to_le_bytes().to_vec();
        query.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        query.extend_from_slice(&bytes);
        log.native_frame(
            EventType::NativeFrameWrite,
            &[3; 16],
            &frame(
                family::GIT,
                schema::git::event::QUERY_STATE,
                Class::Event,
                query,
            ),
            108,
        );
        let records = records(&log);
        let states = records
            .iter()
            .filter(|(kind, _)| *kind == EventType::NativeStateRecordWrite.id())
            .collect::<Vec<_>>();
        assert_eq!(states.len(), 4);
        for pair in states.chunks(2) {
            assert_eq!(&pair[0].1[50..74], &bytes[..24]);
            assert_eq!(&pair[0].1[76..78], &99u16.to_le_bytes());
            assert_eq!(&pair[0].1[88..], b"unknown optional");
            assert_eq!(&pair[1].1[74..76], &1u16.to_le_bytes());
        }
    }

    #[test]
    fn lifecycle_is_default_but_frame_and_body_capture_are_opt_in() {
        let log = EventLog::new(DEFAULT_RING_SIZE, ActivationSet::low_throughput());
        let guard = ConnectionJournal::new(
            log.clone(),
            [4; 16],
            "test",
            "release",
            Arc::new(AtomicU64::new(17)),
            Arc::new(AtomicU64::new(42)),
        );
        log.native_frame(
            EventType::NativeFrameRead,
            &[4; 16],
            &frame(family::KV, 1, Class::Request, b"private".to_vec()),
            20,
        );
        drop(guard);
        let records = records(&log);
        assert_eq!(
            records.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![
                EventType::NativeConnect.id(),
                EventType::NativeDisconnect.id()
            ]
        );
        assert_eq!(&records[1].1[24..32], &17u64.to_le_bytes());
        assert_eq!(&records[1].1[32..40], &42u64.to_le_bytes());
    }
}
