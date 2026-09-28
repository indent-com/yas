//! Human-readable rendering for the binary server event journal.

use std::fmt::Write as _;

use time::OffsetDateTime;
use yas_wire::Decode;
use yas_wire::events::ActivationSet;

const BYTE_PREVIEW: usize = 96;
const EVENT_DUMP_MAGIC: &[u8; 8] = b"YASEVT01";
const EVENT_DUMP_HEADER_LEN: usize = 84;
const EVENT_RECORD_HEADER_LEN: usize = 32;
const EVENT_TYPE_STREAM_GAP: u16 = u16::MAX;
const EVENTS_TARGET_CLIENT: u8 = 0;
const EVENTS_TARGET_FILE: u8 = 1;

#[derive(Clone, Copy)]
#[repr(u32)]
enum EventType {
    ServerStart = 0,
    ServerStop = 1,
    TaskStart = 2,
    TaskStop = 3,
    ClientConnect = 4,
    ClientDisconnect = 5,
    ClientReject = 6,
    ConfigChange = 7,
    StreamStart = 8,
    StreamStop = 9,
    ProtocolError = 10,
    PtyCreate = 11,
    PtyExit = 12,
    PtyRemove = 13,
    Deadline = 14,
    Capacity = 15,
    FrameRead = 16,
    FrameWrite = 17,
    MessageRead = 18,
    MessageWrite = 19,
    TickStart = 20,
    TickStop = 21,
    TickNudge = 22,
    SessionLock = 23,
    PtyRead = 24,
    PtyWrite = 25,
    PtyParse = 26,
    PtySnapshot = 27,
    PtyResize = 28,
    PtyInput = 29,
    CompositorEvent = 30,
    CompositorCommand = 31,
    SurfaceEncode = 32,
    SurfaceFrame = 33,
    AudioFrame = 34,
    FsRequest = 35,
    GitRequest = 36,
    LspRequest = 37,
    KvRequest = 38,
    NetRequest = 39,
    ProcessRequest = 40,
    ExtensionRequest = 41,
    ChannelRequest = 42,
    ClientControl = 43,
    OutboxQueue = 44,
    Supervisor = 45,
    ConnectionAccept = 46,
    Error = 47,
    GitWatchStart = 48,
    GitWatchStop = 49,
    GitState = 50,
    GitRecord = 51,
    GitFsEvent = 52,
    FsRecord = 53,
    FsEvent = 54,
    FsWatchStart = 55,
    FsWatchStop = 56,
    FsState = 57,
    NativeFrameRead = 58,
    NativeFrameWrite = 59,
    NativePayloadRead = 60,
    NativePayloadWrite = 61,
    NativeStateRecordRead = 62,
    NativeStateRecordWrite = 63,
    NativeConnect = 64,
    NativeDisconnect = 65,
    NativeError = 66,
    NativeDatagramRead = 67,
    NativeDatagramWrite = 68,
    NativeDatagramDrop = 69,
}

const EVENT_TYPES: &[EventType] = &[
    EventType::ServerStart,
    EventType::ServerStop,
    EventType::TaskStart,
    EventType::TaskStop,
    EventType::ClientConnect,
    EventType::ClientDisconnect,
    EventType::ClientReject,
    EventType::ConfigChange,
    EventType::StreamStart,
    EventType::StreamStop,
    EventType::ProtocolError,
    EventType::PtyCreate,
    EventType::PtyExit,
    EventType::PtyRemove,
    EventType::Deadline,
    EventType::Capacity,
    EventType::FrameRead,
    EventType::FrameWrite,
    EventType::MessageRead,
    EventType::MessageWrite,
    EventType::TickStart,
    EventType::TickStop,
    EventType::TickNudge,
    EventType::SessionLock,
    EventType::PtyRead,
    EventType::PtyWrite,
    EventType::PtyParse,
    EventType::PtySnapshot,
    EventType::PtyResize,
    EventType::PtyInput,
    EventType::CompositorEvent,
    EventType::CompositorCommand,
    EventType::SurfaceEncode,
    EventType::SurfaceFrame,
    EventType::AudioFrame,
    EventType::FsRequest,
    EventType::GitRequest,
    EventType::LspRequest,
    EventType::KvRequest,
    EventType::NetRequest,
    EventType::ProcessRequest,
    EventType::ExtensionRequest,
    EventType::ChannelRequest,
    EventType::ClientControl,
    EventType::OutboxQueue,
    EventType::Supervisor,
    EventType::ConnectionAccept,
    EventType::Error,
    EventType::GitWatchStart,
    EventType::GitWatchStop,
    EventType::GitState,
    EventType::GitRecord,
    EventType::GitFsEvent,
    EventType::FsRecord,
    EventType::FsEvent,
    EventType::FsWatchStart,
    EventType::FsWatchStop,
    EventType::FsState,
    EventType::NativeFrameRead,
    EventType::NativeFrameWrite,
    EventType::NativePayloadRead,
    EventType::NativePayloadWrite,
    EventType::NativeStateRecordRead,
    EventType::NativeStateRecordWrite,
    EventType::NativeConnect,
    EventType::NativeDisconnect,
    EventType::NativeError,
    EventType::NativeDatagramRead,
    EventType::NativeDatagramWrite,
    EventType::NativeDatagramDrop,
];

impl EventType {
    fn from_id(id: u32) -> Option<Self> {
        EVENT_TYPES.get(usize::try_from(id).ok()?).copied()
    }

    fn id(self) -> u16 {
        self as u16
    }

    fn name(self) -> &'static str {
        crate::yas_events::EVENT_NAMES[self as usize]
    }
}

pub(crate) fn render_dump(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() < EVENT_DUMP_HEADER_LEN {
        return Err("event dump is truncated".into());
    }
    if bytes.get(..EVENT_DUMP_MAGIC.len()) != Some(EVENT_DUMP_MAGIC.as_slice()) {
        return Err("event dump has invalid magic".into());
    }
    let header_len = read_u16(bytes, 8)? as usize;
    let version = read_u16(bytes, 10)?;
    if header_len < EVENT_DUMP_HEADER_LEN || header_len > bytes.len() {
        return Err(format!("event dump has invalid header length {header_len}"));
    }
    let capacity = read_u64(bytes, 12)?;
    let used = read_u64(bytes, 20)?;
    let declared_records = read_u64(bytes, 28)?;
    let dropped = read_u64(bytes, 36)?;
    let next_sequence = read_u64(bytes, 44)?;
    let activations = activation_set(&bytes[52..84])
        .ok_or_else(|| "event dump has invalid activation bitset".to_owned())?;
    let records = &bytes[header_len..];
    if used != records.len() as u64 {
        return Err(format!(
            "event dump declares {used} retained bytes but contains {}",
            records.len()
        ));
    }

    let enabled = EVENT_TYPES
        .iter()
        .filter_map(|&kind| activations.enabled(kind.id()).then_some(kind.name()))
        .collect::<Vec<_>>()
        .join(",");
    let mut output = format!(
        "# yas.events.v{version} capacity={capacity} retained_bytes={used} retained_records={declared_records} dropped={dropped} next_sequence={next_sequence} enabled={enabled}\n"
    );
    let (rendered, actual_records) = render_record_bytes(records)?;
    if actual_records as u64 != declared_records {
        return Err(format!(
            "event dump declares {declared_records} records but contains {actual_records}"
        ));
    }
    output.push_str(&rendered);
    Ok(output)
}

#[cfg(test)]
pub(crate) fn render_records(bytes: &[u8], expected: Option<u16>) -> Result<String, String> {
    let (rendered, actual) = render_record_bytes(bytes)?;
    if let Some(expected) = expected
        && actual != usize::from(expected)
    {
        return Err(format!(
            "event batch declares {expected} records but contains {actual}"
        ));
    }
    Ok(rendered)
}

pub(crate) fn render_gap(lost: u64) -> String {
    format!("! stream.gap lost={lost}\n")
}

/// Render a canonical YAS Events packed batch. Live native batches carry the
/// server monotonic timestamp but deliberately omit a wall-clock timestamp,
/// so the output does not invent one.
pub(crate) fn render_native_batch(batch: &yas_wire::events::EventBatch) -> String {
    let mut output = String::new();
    for record in &batch.records {
        let kind = EventType::from_id(record.event_id);
        let name = kind
            .map(EventType::name)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("event.{}", record.event_id));
        write!(
            output,
            "+{} #{} {name}",
            format_duration(record.monotonic_ns),
            record.sequence
        )
        .expect("writing to String");
        if record.required {
            output.push_str(" required");
        }
        if record.event_flags != 0 {
            write!(output, " flags=0x{:04x}", record.event_flags).expect("writing to String");
        }
        if !record.payload.is_empty() {
            let detail = kind
                .and_then(|kind| describe_payload(kind, &record.payload))
                .unwrap_or_else(|| format!("payload={}", quoted_bytes(&record.payload)));
            write!(output, " {detail}").expect("writing to String");
        }
        output.push('\n');
    }
    output
}

fn render_record_bytes(mut bytes: &[u8]) -> Result<(String, usize), String> {
    let mut output = String::new();
    let mut count = 0usize;
    while !bytes.is_empty() {
        if bytes.len() < EVENT_RECORD_HEADER_LEN {
            return Err("event record header is truncated".into());
        }
        let len = read_u32(bytes, 0)? as usize;
        if len < EVENT_RECORD_HEADER_LEN || len > bytes.len() {
            return Err(format!("event record has invalid length {len}"));
        }
        render_record(&bytes[..len], &mut output)?;
        bytes = &bytes[len..];
        count += 1;
    }
    Ok((output, count))
}

fn render_record(record: &[u8], output: &mut String) -> Result<(), String> {
    let event_id = read_u16(record, 4)?;
    let flags = read_u16(record, 6)?;
    let sequence = read_u64(record, 8)?;
    let monotonic_ns = read_u64(record, 16)?;
    let unix_ns = read_u64(record, 24)?;
    let payload = &record[EVENT_RECORD_HEADER_LEN..];
    let kind = EventType::from_id(u32::from(event_id));
    let name = kind.map(EventType::name);
    let timestamp = format_timestamp(unix_ns);
    let event_name = if event_id == EVENT_TYPE_STREAM_GAP {
        "stream.gap".to_owned()
    } else {
        name.map(str::to_owned)
            .unwrap_or_else(|| format!("event.{event_id}"))
    };

    write!(
        output,
        "{timestamp} +{} #{sequence} {event_name}",
        format_duration(monotonic_ns)
    )
    .expect("writing to String");
    if flags != 0 {
        write!(output, " flags=0x{flags:04x}").expect("writing to String");
    }
    if !payload.is_empty() {
        let detail = if event_id == EVENT_TYPE_STREAM_GAP && payload.len() == 8 {
            format!("lost={}", read_u64(payload, 0)?)
        } else {
            kind.and_then(|kind| describe_payload(kind, payload))
                .unwrap_or_else(|| format!("payload={}", quoted_bytes(payload)))
        };
        write!(output, " {detail}").expect("writing to String");
    }
    output.push('\n');
    Ok(())
}

fn describe_payload(kind: EventType, payload: &[u8]) -> Option<String> {
    let mut cursor = Cursor::new(payload);
    let detail = match kind {
        EventType::ServerStart => {
            format!("version={:?} server={:?}", cursor.name()?, cursor.name()?)
        }
        EventType::TaskStart | EventType::TaskStop => format!("task={:?}", cursor.name()?),
        EventType::ClientReject => format!("reason={:?}", cursor.name()?),
        EventType::ProtocolError | EventType::Error => format!("message={:?}", cursor.name()?),
        EventType::Capacity => format!("resource={:?}", cursor.name()?),
        EventType::ClientConnect | EventType::ClientDisconnect => {
            format!("client={}", cursor.u64()?)
        }
        EventType::ConfigChange => {
            let client = cursor.u64()?;
            let size = cursor.u64()?;
            let active = activation_set(cursor.take(32)?)?;
            let names = EVENT_TYPES
                .iter()
                .filter_map(|&event| active.enabled(event.id()).then_some(event.name()))
                .collect::<Vec<_>>()
                .join(",");
            format!("client={client} capacity={size} enabled={names}")
        }
        EventType::StreamStart => describe_stream_start(&mut cursor)?,
        EventType::StreamStop => {
            format!("client={} stream={}", cursor.u64()?, cursor.u32()?)
        }
        EventType::PtyCreate => {
            let client = cursor.u64()?;
            let nonce = cursor.u16()?;
            let stage = cursor.u8()?;
            let status = cursor.u8()?;
            let pty = cursor.u16()?;
            format!(
                "client={client} nonce={nonce} stage={} status={} pty={pty}",
                pty_create_stage(stage),
                status_text(status),
            )
        }
        EventType::PtyExit => {
            let pty = cursor.u16()?;
            let status = cursor.i32()?;
            let reason = cursor.u8()?;
            format!(
                "pty={pty} status={status} reason={:?}",
                exit_reason_text(reason)
            )
        }
        EventType::PtyRemove => {
            let pty = cursor.u16()?;
            let source = cursor
                .u8_optional()
                .map(|value| if value == 1 { "close" } else { "unknown" })
                .unwrap_or("retention");
            format!("pty={pty} source={source}")
        }
        EventType::Deadline => {
            let pty = cursor.u16()?;
            let stage = match cursor.u8()? {
                1 => "term",
                2 => "kill",
                _ => "unknown",
            };
            format!("pty={pty} stage={stage}")
        }
        EventType::GitWatchStart
        | EventType::GitWatchStop
        | EventType::FsWatchStart
        | EventType::FsWatchStop => {
            let identity = watch_identity(
                &mut cursor,
                matches!(kind, EventType::GitWatchStart | EventType::GitWatchStop),
            )?;
            let flags = cursor.u32()?;
            let state_flags = cursor.u16()?;
            let refs_settle_ms = cursor.u16()?;
            let settle_ms = cursor.u16()?;
            let path_len = usize::from(cursor.u16()?);
            let path = cursor.take(path_len)?;
            format!(
                "{identity} flags=0x{flags:x} state_flags=0x{state_flags:x} refs_settle_ms={refs_settle_ms} settle_ms={settle_ms} path={}",
                quoted_bytes(path)
            )
        }
        EventType::GitState | EventType::FsState => {
            let identity = watch_identity(&mut cursor, matches!(kind, EventType::GitState))?;
            format!(
                "{identity} revision={} records={} bytes={}",
                cursor.u64()?,
                cursor.u32()?,
                cursor.u64()?
            )
        }
        EventType::GitFsEvent | EventType::FsEvent => {
            let identity = watch_identity(&mut cursor, matches!(kind, EventType::GitFsEvent))?;
            let event = cursor.bytes_u32()?;
            let rescan = cursor.u8()? != 0;
            let count = cursor.u32()? as usize;
            if count > (cursor.bytes.len() - cursor.at) / 4 {
                return None;
            }
            let mut paths = Vec::with_capacity(count);
            for _ in 0..count {
                paths.push(quoted_bytes(cursor.bytes_u32()?));
            }
            format!(
                "{identity} event={} rescan={rescan} paths=[{}]",
                quoted_bytes(event),
                paths.join(", ")
            )
        }
        EventType::GitRecord | EventType::FsRecord => {
            let git = matches!(kind, EventType::GitRecord);
            let identity = watch_identity(&mut cursor, git)?;
            let revision = cursor.u64()?;
            let watch_flags = cursor.u32()?;
            let record_id = cursor.u64()?;
            let record_kind = cursor.u16()?;
            let required = cursor.u16()?;
            let total = cursor.u32()?;
            let offset = cursor.u32()?;
            let body = cursor.remaining();
            let decoded = if offset != 0 || body.len() != total as usize {
                None
            } else if git {
                describe_git_record(record_kind, watch_flags, body)
            } else {
                describe_fs_record(record_kind, body)
            };
            format!(
                "{identity} revision={revision} record={record_id} kind={record_kind} required={required} offset={offset} total={total} chunk_bytes={} {}",
                body.len(),
                decoded.unwrap_or_else(|| format!("body={}", quoted_bytes(body)))
            )
        }
        EventType::NativeConnect | EventType::NativeDisconnect | EventType::NativeError => {
            let session = cursor
                .take(16)?
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let detail = match kind {
                EventType::NativeConnect => {
                    format!("name={:?} release={:?}", cursor.name()?, cursor.name()?)
                }
                EventType::NativeDisconnect => format!(
                    "elapsed_us={} received_bytes={} sent_bytes={} reason={:?}",
                    cursor.u64()?,
                    cursor.u64()?,
                    cursor.u64()?,
                    cursor.name()?
                ),
                _ => format!("stage={:?} error={:?}", cursor.name()?, cursor.name()?),
            };
            format!("session={session} {detail}")
        }
        EventType::NativeFrameRead
        | EventType::NativeFrameWrite
        | EventType::NativeDatagramRead
        | EventType::NativeDatagramWrite
        | EventType::NativeDatagramDrop
        | EventType::NativePayloadRead
        | EventType::NativePayloadWrite
        | EventType::NativeStateRecordRead
        | EventType::NativeStateRecordWrite => {
            let session = cursor
                .take(16)?
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let family_id = cursor.u16()?;
            let operation_kind = cursor.u16()?;
            let class = cursor.u8()?;
            let flags = cursor.u8()?;
            let request = cursor.u32()?;
            let watch = cursor.u32()?;
            let payload_bytes = cursor.u32()?;
            let wire_bytes = cursor.u64()?;
            let trace = cursor.u64()?;
            let family = yas_wire::schema::FAMILIES
                .iter()
                .find(|family| family.id == family_id);
            let operation = family.and_then(|family| {
                family.operations.iter().find(|operation| {
                    operation.kind == operation_kind
                        && operation.class == if class == 2 { 1 } else { class }
                })
            });
            let name = operation.map_or("unknown", |operation| operation.name);
            let mut text = format!(
                "session={session} trace={trace} family={} kind={}({operation_kind}) class={} request={request} watch={watch} flags=0x{flags:x} payload_bytes={payload_bytes} wire_bytes={wire_bytes}",
                family.map_or("unknown", |family| family.name),
                operation.map_or("unknown", |operation| operation.name),
                match class {
                    0 => "event",
                    1 => "request",
                    2 => "result",
                    _ => "unknown",
                }
            );
            match kind {
                EventType::NativePayloadRead | EventType::NativePayloadWrite => {
                    let length = cursor.u32()?;
                    let offset = cursor.u32()?;
                    let bytes = cursor.remaining();
                    write!(
                        text,
                        " offset={offset} total={length} chunk_bytes={} data={}",
                        bytes.len(),
                        quoted_bytes(bytes)
                    )
                    .ok()?;
                }
                EventType::NativeStateRecordRead | EventType::NativeStateRecordWrite => {
                    text.push_str(&describe_state_header(&mut cursor)?);
                    let index = cursor.u16()?;
                    let record_kind = cursor.u16()?;
                    let required = cursor.u16()?;
                    let length = cursor.u32()?;
                    let offset = cursor.u32()?;
                    let bytes = cursor.remaining();
                    let detail = (offset == 0 && bytes.len() == length as usize)
                        .then(|| describe_native_record(family_id, name, record_kind, bytes))
                        .flatten()
                        .unwrap_or_else(|| format!("data={}", quoted_bytes(bytes)));
                    write!(text, " record={index} action={}({record_kind}) required={required} offset={offset} total={length} chunk_bytes={} {detail}",
                        match record_kind { 0 => "add", 1 => "replace", 2 => "patch", 3 => "remove", _ => "family" }, bytes.len()).ok()?;
                }
                _ if !cursor.finished() => {
                    if class == 2 {
                        let status = cursor.u16()?;
                        let _reserved = cursor.u16()?;
                        write!(
                            text,
                            " status={:?}",
                            yas_wire::core::Status::from_code(status)
                        )
                        .ok()?;
                    } else if matches!(name, "STATE" | "QUERY_STATE") {
                        text.push_str(&describe_state_header(&mut cursor)?);
                        write!(text, " records={}", cursor.u16()?).ok()?;
                    } else if matches!(name, "STATE_ACK" | "QUERY_STATE_ACK") {
                        cursor.u32()?;
                        write!(
                            text,
                            " applied_revision={} credit={}",
                            cursor.u64()?,
                            cursor.u64()?
                        )
                        .ok()?;
                    } else if family_id == yas_wire::family::TRANSFER {
                        write!(text, " transfer={}", cursor.u32()?).ok()?;
                    }
                }
                _ => {}
            }
            text
        }
        EventType::FrameRead
        | EventType::FrameWrite
        | EventType::SurfaceFrame
        | EventType::AudioFrame => describe_frame(&mut cursor)?,
        EventType::MessageRead
        | EventType::MessageWrite
        | EventType::FsRequest
        | EventType::GitRequest
        | EventType::LspRequest
        | EventType::KvRequest
        | EventType::NetRequest
        | EventType::ProcessRequest
        | EventType::ExtensionRequest
        | EventType::ChannelRequest
        | EventType::ClientControl
        | EventType::CompositorCommand => format!(
            "client={} opcode=0x{:02x} bytes={}",
            cursor.u64()?,
            cursor.u8()?,
            cursor.u32()?
        ),
        EventType::TickStop => format!(
            "elapsed={} clients={} ptys={}",
            format_duration(cursor.u64()?),
            cursor.u32()?,
            cursor.u32()?
        ),
        EventType::SessionLock => format!(
            "owner={:?} waited={}",
            cursor.name()?,
            format_duration(cursor.u64()?)
        ),
        EventType::PtyRead | EventType::PtyWrite => {
            let pty = cursor.u16()?;
            let declared = cursor.u32()? as usize;
            let bytes = cursor.take(declared)?;
            format!("pty={pty} bytes={declared} data={}", quoted_bytes(bytes))
        }
        EventType::PtyParse => format!(
            "pty={} bytes={} elapsed={}",
            cursor.u16()?,
            cursor.u32()?,
            format_duration(cursor.u64()?)
        ),
        EventType::PtySnapshot => format!("pty={}", cursor.u16()?),
        EventType::PtyResize => format!(
            "client={} pty={} rows={} cols={}",
            cursor.u64()?,
            cursor.u16()?,
            cursor.u16()?,
            cursor.u16()?
        ),
        EventType::PtyInput => {
            let client = cursor.u64()?;
            let pty = cursor.u16()?;
            format!(
                "client={client} pty={pty} data={}",
                quoted_bytes(cursor.remaining())
            )
        }
        EventType::CompositorEvent => describe_compositor_event(&mut cursor)?,
        EventType::SurfaceEncode => format!(
            "surface={} client={} size={}x{} bytes={} codec={} keyframe={}",
            cursor.u16()?,
            cursor.u64()?,
            cursor.u32()?,
            cursor.u32()?,
            cursor.u32()?,
            cursor.u8()?,
            cursor.u8()? != 0
        ),
        EventType::OutboxQueue => {
            format!("client={} bytes={}", cursor.u64()?, cursor.u32()?)
        }
        EventType::Supervisor => match cursor.u8()? {
            1 => "stage=start".to_owned(),
            2 => format!("stage=stop elapsed={}", format_duration(cursor.u64()?)),
            stage => format!("stage={stage}"),
        },
        EventType::ServerStop
        | EventType::TickStart
        | EventType::TickNudge
        | EventType::ConnectionAccept => return None,
    };
    cursor.finished().then_some(detail)
}

fn describe_state_header(cursor: &mut Cursor<'_>) -> Option<String> {
    let watch = cursor.u32()?;
    let phase = cursor.u8()?;
    let flags = cursor.u8()?;
    cursor.u16()?;
    let from = cursor.u64()?;
    let to = cursor.u64()?;
    Some(format!(
        " state_watch={watch} phase={}({phase}) state_flags=0x{flags:x} from_revision={from} to_revision={to}",
        match phase {
            0 => "snapshot-begin",
            1 => "snapshot-records",
            2 => "snapshot-end",
            3 => "delta",
            4 => "reset",
            _ => "unknown",
        }
    ))
}

fn describe_native_record(family: u16, operation: &str, kind: u16, body: &[u8]) -> Option<String> {
    use yas_wire::{
        family as f,
        state::{Record, RecordKind},
    };
    let record_kind = match kind {
        0 => RecordKind::Add,
        1 => RecordKind::Replace,
        2 => RecordKind::Patch,
        3 => RecordKind::Remove,
        value => RecordKind::Family(value),
    };
    let remove = record_kind == RecordKind::Remove;
    let patch = record_kind == RecordKind::Patch;
    macro_rules! decoded {
        ($ty:ty) => {
            Some(format!("value={:?}", <$ty>::decode(body).ok()?))
        };
    }
    macro_rules! removed {
        ($ty:ty) => {
            Some(format!(
                "value={:?}",
                <$ty>::from_state_record(&Record {
                    kind: record_kind,
                    required: false,
                    body: body.to_vec(),
                })
                .ok()?
            ))
        };
    }
    match family {
        f::GIT => describe_git_record(
            kind,
            if operation == "QUERY_STATE" {
                yas_wire::schema::client::GIT_QUERY_WATCH as u32
            } else {
                0
            },
            body,
        ),
        f::FS => describe_fs_record(kind, body),
        f::KV if remove => removed!(yas_wire::kv::RemovedEntry),
        f::KV => {
            let entry = yas_wire::kv::EntryRecord::decode(body).ok()?;
            Some(format!(
                "key={} value={entry:?}",
                quoted_bytes(&entry.relative_key)
            ))
        }
        f::TERMINAL if remove => decoded!(yas_wire::terminal::RemovedTerminal),
        f::TERMINAL if patch => decoded!(yas_wire::terminal::TerminalPatch),
        f::TERMINAL => decoded!(yas_wire::terminal::TerminalRecord),
        f::SURFACE if remove => decoded!(yas_wire::surface::RemovedSurface),
        f::SURFACE if patch => decoded!(yas_wire::surface::SurfacePatch),
        f::SURFACE => decoded!(yas_wire::surface::SurfaceRecord),
        f::CLIENT if remove => decoded!(yas_wire::client::RemovedClient),
        f::CLIENT if patch => decoded!(yas_wire::client::ClientPatch),
        f::CLIENT => decoded!(yas_wire::client::ClientRecord),
        f::PROCESS if remove => decoded!(yas_wire::process::RemovedProcess),
        f::PROCESS => decoded!(yas_wire::process::ProcessRecord),
        f::EXTENSION if remove => decoded!(yas_wire::extension::RemovedExtension),
        f::EXTENSION => decoded!(yas_wire::extension::ExtensionRecord),
        f::CHANNEL if remove => decoded!(yas_wire::channel::RemovedListener),
        f::CHANNEL => decoded!(yas_wire::channel::ListenerRecord),
        f::RELAY if remove => removed!(yas_wire::relay::RemovedRoute),
        f::RELAY => decoded!(yas_wire::relay::RouteRecord),
        f::FONT if remove => removed!(yas_wire::font::RemovedFamily),
        f::FONT => decoded!(yas_wire::font::FamilyRecord),
        f::LSP if remove => decoded!(yas_wire::lsp::RemovedEntity),
        f::LSP if patch => decoded!(yas_wire::lsp::EntityPatch),
        f::LSP => decoded!(yas_wire::lsp::StateEntity),
        f::MEDIA | f::DESKTOP | f::SELECTION => {
            let record = Record {
                kind: record_kind,
                required: false,
                body: body.to_vec(),
            };
            Some(match family {
                f::MEDIA => format!(
                    "value={:?}",
                    yas_wire::media::decode_state_record(&record).ok()?
                ),
                f::DESKTOP => format!(
                    "value={:?}",
                    yas_wire::desktop::decode_state_record(&record).ok()?
                ),
                _ => format!(
                    "value={:?}",
                    yas_wire::selection::decode_state_record(&record).ok()?
                ),
            })
        }
        _ => None,
    }
}

fn describe_stream_start(cursor: &mut Cursor<'_>) -> Option<String> {
    let payload = cursor.remaining();
    let mut request = Cursor::new(payload);
    if payload
        .get(12)
        .is_some_and(|target| matches!(*target, EVENTS_TARGET_CLIENT | EVENTS_TARGET_FILE))
    {
        let client = request.u64()?;
        let stream = request.u32()?;
        let target = match request.u8()? {
            EVENTS_TARGET_CLIENT => "client",
            EVENTS_TARGET_FILE => "file",
            _ => unreachable!("checked above"),
        };
        let path = if request.is_empty() {
            String::new()
        } else {
            format!(" path={:?}", request.name()?)
        };
        if request.finished() {
            return Some(format!(
                "client={client} stream={stream} target={target}{path}"
            ));
        }
    }

    let mut startup = Cursor::new(payload);
    let stream = startup.u32()?;
    let detail = format!("stream={stream} target=file path={:?}", startup.name()?);
    startup.finished().then_some(detail)
}

fn describe_frame(cursor: &mut Cursor<'_>) -> Option<String> {
    let client = cursor.u64()?;
    let declared = cursor.u32()? as usize;
    let frame = cursor.take(declared)?;
    let opcode = frame.first().copied();
    Some(match opcode {
        Some(opcode) => format!(
            "client={client} bytes={declared} opcode=0x{opcode:02x} data={}",
            quoted_bytes(frame)
        ),
        None => format!("client={client} bytes=0 data=b\"\""),
    })
}

fn describe_compositor_event(cursor: &mut Cursor<'_>) -> Option<String> {
    match cursor.u8()? {
        1 => Some(format!(
            "kind=created surface={} parent={} size={}x{} title={:?} app_id={:?}",
            cursor.u16()?,
            cursor.u16()?,
            cursor.u16()?,
            cursor.u16()?,
            cursor.name()?,
            cursor.name()?
        )),
        2 => Some(format!("kind=destroyed surface={}", cursor.u16()?)),
        3 => Some(format!(
            "kind=commit surface={} size={}x{} timestamp_ms={} timestamp_sub_us={} encoder_skip={}",
            cursor.u16()?,
            cursor.u32()?,
            cursor.u32()?,
            cursor.u32()?,
            cursor.u16()?,
            cursor.u8()? != 0
        )),
        kind => Some(format!(
            "kind={kind} payload={}",
            quoted_bytes(cursor.remaining())
        )),
    }
}

fn pty_create_stage(stage: u8) -> &'static str {
    match stage {
        1 => "request-received",
        2 => "session-acquired",
        3 => "spawn-begin",
        4 => "spawn-end",
        5 => "registered",
        6 => "refused",
        7 => "reply-written",
        _ => "unknown",
    }
}

fn activation_set(bytes: &[u8]) -> Option<ActivationSet> {
    if bytes.len() != 32 {
        return None;
    }
    let mut words = [0u64; 4];
    for (index, word) in words.iter_mut().enumerate() {
        *word = u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().ok()?);
    }
    Some(ActivationSet(words))
}

fn status_text(status: u8) -> &'static str {
    match status {
        0 => "ok",
        1 => "unknown id",
        2 => "not found",
        3 => "wrong type",
        4 => "permission denied",
        5 => "too large",
        6 => "budget exhausted",
        7 => "invalid request",
        8 => "cancelled",
        9 => "backend error",
        10 => "warming up",
        11 => "conflict",
        12 => "no merge base",
        _ => "unknown status",
    }
}

fn exit_reason_text(reason: u8) -> &'static str {
    match reason {
        0 => "exited",
        1 => "killed by deadline",
        2 => "killed by lease expiry",
        3 => "evicted",
        4 => "stopped by unit",
        _ => "unknown reason",
    }
}

fn watch_identity(cursor: &mut Cursor<'_>, git: bool) -> Option<String> {
    let session = cursor
        .take(16)?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let resource = cursor.u64()?;
    let subscription = cursor.u32()?;
    Some(format!(
        "session={session} {}={resource} watch={subscription}",
        if git { "repository" } else { "root" }
    ))
}

fn record_path(path: &yas_wire::fs::Path) -> String {
    quoted_bytes(&path.components.join(&b'/'))
}

fn describe_git_record(kind: u16, flags: u32, body: &[u8]) -> Option<String> {
    use yas_wire::{git, schema, state::RecordKind};
    if flags & schema::client::GIT_QUERY_WATCH as u32 != 0 {
        let value = git::WatchedQueryValue::decode(body).ok()?;
        return Some(format!("query={value:?}"));
    }
    if kind == RecordKind::Remove.wire() {
        let value = git::RemovedEntity::decode(body).ok()?;
        return Some(format!(
            "entity={} key={}",
            value.entity_kind,
            quoted_bytes(&value.key)
        ));
    }
    let value = if kind == RecordKind::Patch.wire() {
        git::EntityPatch::decode(body).ok()?.replacement
    } else {
        git::EntityRecord::decode(body).ok()?
    };
    if let git::EntityBody::Status(status) = &value.body {
        let path = yas_wire::fs::Path::decode(&value.key).ok()?;
        let letter = |value: u8| {
            " AMDRCTU?!"
                .as_bytes()
                .get(usize::from(value))
                .copied()
                .map(char::from)
                .unwrap_or('?')
        };
        return Some(format!(
            "entity=status path={} status=\"{}{}\" flags=0x{:x}",
            record_path(&path),
            letter(status.index_status),
            letter(status.worktree_status),
            status.flags
        ));
    }
    Some(format!(
        "entity={} key={} value={:?}",
        value.entity_kind,
        quoted_bytes(&value.key),
        value.body
    ))
}

fn describe_fs_record(kind: u16, body: &[u8]) -> Option<String> {
    use yas_wire::{fs, schema, state::RecordKind};
    if kind == RecordKind::Remove.wire() {
        let entry = fs::RemoveRecord::decode(body).ok()?;
        return Some(format!(
            "path={} removed_revision={}",
            record_path(&entry.path),
            entry.removed_revision
        ));
    }
    if kind == schema::fs::RECORD_MOVE as u16 {
        let entry = fs::MoveRecord::decode(body).ok()?;
        return Some(format!(
            "from={} to={}",
            record_path(&entry.from),
            record_path(&entry.to)
        ));
    }
    let entry = if kind == RecordKind::Patch.wire() {
        fs::EntryPatch::decode(body).ok()?.replacement
    } else {
        fs::EntryRecord::decode(body).ok()?
    };
    let detail = match &entry.body {
        fs::EntryBody::File {
            byte_len,
            content_hash,
            inline_content,
        } => format!(
            "file bytes={byte_len} hash={} inline_bytes={}",
            content_hash
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            inline_content.as_ref().map_or(0, Vec::len)
        ),
        fs::EntryBody::Directory => "directory".to_owned(),
        fs::EntryBody::Symlink { target, .. } => format!("symlink target={}", quoted_bytes(target)),
    };
    Some(format!(
        "path={} entry_revision={} mode={:o} flags=0x{:x} {detail}",
        record_path(&entry.path),
        entry.entry_revision,
        entry.mode,
        entry.flags
    ))
}

fn quoted_bytes(bytes: &[u8]) -> String {
    let shown = bytes.len().min(BYTE_PREVIEW);
    let mut output = String::from("b\"");
    for &byte in &bytes[..shown] {
        for escaped in std::ascii::escape_default(byte) {
            output.push(char::from(escaped));
        }
    }
    output.push('"');
    if bytes.len() > shown {
        write!(output, "...(+{} bytes)", bytes.len() - shown).expect("writing to String");
    }
    output
}

/// The wall clock, to the nanosecond, always nine digits of it.
///
/// RFC 3339 permits any number of subsecond digits and the formatter drops the
/// trailing zeros, so a timestamp's precision would depend on its own value —
/// `.221884495Z` one line, `.2218Z` the next.
fn format_timestamp(unix_ns: u64) -> String {
    let Ok(value) = OffsetDateTime::from_unix_timestamp_nanos(i128::from(unix_ns)) else {
        return format!("unix-ns:{unix_ns}");
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
        value.nanosecond(),
    )
}

/// Exactly what the clock said, to the nanosecond, always the same width.
///
/// Trailing zeros are digits like any other: a rounded `+9796.711854s` cannot
/// be subtracted from the record above it, and a column that changes unit
/// every few lines cannot be read down.
fn format_duration(nanos: u64) -> String {
    format!("{}.{:09}s", nanos / 1_000_000_000, nanos % 1_000_000_000)
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(
        bytes
            .get(at..at + 2)
            .ok_or_else(|| "event data is truncated".to_owned())?
            .try_into()
            .expect("slice length checked"),
    ))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        bytes
            .get(at..at + 4)
            .ok_or_else(|| "event data is truncated".to_owned())?
            .try_into()
            .expect("slice length checked"),
    ))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        bytes
            .get(at..at + 8)
            .ok_or_else(|| "event data is truncated".to_owned())?
            .try_into()
            .expect("slice length checked"),
    ))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn is_empty(&self) -> bool {
        self.at == self.bytes.len()
    }

    fn finished(&self) -> bool {
        self.is_empty()
    }

    fn remaining(&mut self) -> &'a [u8] {
        let remaining = &self.bytes[self.at..];
        self.at = self.bytes.len();
        remaining
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(len)?;
        let value = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(value)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u8_optional(&mut self) -> Option<u8> {
        (!self.is_empty()).then(|| self.u8()).flatten()
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn bytes_u32(&mut self) -> Option<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn name(&mut self) -> Option<String> {
        let len = usize::from(self.u16()?);
        Some(std::str::from_utf8(self.take(len)?).ok()?.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u16, sequence: u64, payload: &[u8]) -> Vec<u8> {
        let len = EVENT_RECORD_HEADER_LEN + payload.len();
        let mut record = Vec::with_capacity(len);
        record.extend_from_slice(&(len as u32).to_le_bytes());
        record.extend_from_slice(&kind.to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes());
        record.extend_from_slice(&sequence.to_le_bytes());
        record.extend_from_slice(&1_250_000u64.to_le_bytes());
        record.extend_from_slice(&1_700_000_000_123_456_789u64.to_le_bytes());
        record.extend_from_slice(payload);
        record
    }

    fn name(value: &str) -> Vec<u8> {
        let mut payload = (value.len() as u16).to_le_bytes().to_vec();
        payload.extend_from_slice(value.as_bytes());
        payload
    }

    #[test]
    fn renders_dump_header_and_typed_payload() {
        let mut payload = name("0.55.1");
        payload.extend_from_slice(&name("default"));
        let records = record(EventType::ServerStart.id(), 7, &payload);
        let activations = ActivationSet::low_throughput();
        let mut dump = Vec::new();
        dump.extend_from_slice(EVENT_DUMP_MAGIC);
        dump.extend_from_slice(&(EVENT_DUMP_HEADER_LEN as u16).to_le_bytes());
        dump.extend_from_slice(&1u16.to_le_bytes());
        dump.extend_from_slice(&4096u64.to_le_bytes());
        dump.extend_from_slice(&(records.len() as u64).to_le_bytes());
        dump.extend_from_slice(&1u64.to_le_bytes());
        dump.extend_from_slice(&3u64.to_le_bytes());
        dump.extend_from_slice(&8u64.to_le_bytes());
        for word in activations.0 {
            dump.extend_from_slice(&word.to_le_bytes());
        }
        dump.extend_from_slice(&records);

        let output = render_dump(&dump).unwrap();
        assert!(output.contains("retained_records=1 dropped=3 next_sequence=8"));
        assert!(output.contains("#7 server.start version=\"0.55.1\" server=\"default\""));
        // Nine digits, trailing zeros and all: 1_250_000 ns is 0.001250000s.
        assert!(output.contains("+0.001250000s"), "{output}");
    }

    #[test]
    fn unknown_events_have_bounded_escaped_payloads() {
        let payload = vec![b'a', b'\n', 0, 0xff];
        let output = render_records(&record(500, 9, &payload), Some(1)).unwrap();
        assert!(output.contains("#9 event.500 payload=b\"a\\n\\x00\\xff\""));
    }

    #[test]
    fn rejects_declared_record_count_mismatch() {
        let error =
            render_records(&record(EventType::TickNudge.id(), 1, &[]), Some(2)).unwrap_err();
        assert!(error.contains("declares 2 records but contains 1"));
    }

    #[test]
    fn renders_stream_gap() {
        assert_eq!(render_gap(12), "! stream.gap lost=12\n");
        let gap = record(EVENT_TYPE_STREAM_GAP, 0, &12u64.to_le_bytes());
        assert!(
            render_records(&gap, Some(1))
                .unwrap()
                .contains("stream.gap")
        );
    }

    #[test]
    fn native_schema_event_catalog_is_dense_and_aligned() {
        assert_eq!(EVENT_TYPES.len(), crate::yas_events::EVENT_NAMES.len());
        for (id, &kind) in EVENT_TYPES.iter().enumerate() {
            assert_eq!(usize::from(kind.id()), id);
            assert_eq!(kind.name(), crate::yas_events::EVENT_NAMES[id]);
        }
        assert_eq!(
            u64::from(EventType::Error.id()),
            yas_wire::schema::events::EVENT_SERVER_ERROR
        );
        assert_eq!(
            EVENT_TYPES.last().unwrap().id() as u64,
            yas_wire::schema::events::EVENT_NATIVE_DATAGRAM_DROP
        );
    }

    fn native_identity(family: u16, operation: u16, class: u8) -> Vec<u8> {
        let mut bytes = vec![1; 16];
        bytes.extend_from_slice(&family.to_le_bytes());
        bytes.extend_from_slice(&operation.to_le_bytes());
        bytes.extend_from_slice(&[class, 1]);
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&9u32.to_le_bytes());
        bytes.extend_from_slice(&32u32.to_le_bytes());
        bytes.extend_from_slice(&45u64.to_le_bytes());
        bytes.extend_from_slice(&11u64.to_le_bytes());
        bytes
    }

    #[test]
    fn native_rendering_names_operations_statuses_and_flow_credit() {
        use yas_wire::{Encode, family, schema, state::StateAck};
        let mut result = native_identity(family::GIT, schema::git::request::OPEN, 2);
        result.extend_from_slice(&[3, 0, 0, 0]);
        let text = describe_payload(EventType::NativeFrameWrite, &result).unwrap();
        assert!(text.contains("trace=11 family=yas.git kind=OPEN"));
        assert!(text.contains("status=NotFound"));
        let mut ack = native_identity(family::KV, schema::kv::event::STATE_ACK, 0);
        ack.extend_from_slice(
            &StateAck {
                subscription_id: 9,
                applied_revision: 42,
                cumulative_byte_limit: 1000,
            }
            .encode()
            .unwrap(),
        );
        assert!(
            describe_payload(EventType::NativeFrameRead, &ack)
                .unwrap()
                .contains("applied_revision=42 credit=1000")
        );
    }

    #[test]
    fn native_record_rendering_decodes_paths_and_rejects_truncated_headers() {
        use yas_wire::{
            Encode, family,
            fs::{Path, RemoveRecord},
            schema,
            state::{Phase, StateEvent},
        };
        let body = RemoveRecord {
            path: Path {
                components: vec![b"src".to_vec(), b"gone.rs".to_vec()],
            },
            removed_revision: 5,
            operation_id: None,
        }
        .encode()
        .unwrap();
        let event = StateEvent {
            subscription_id: 9,
            phase: Phase::Delta,
            flags: 0,
            from_revision: 5,
            to_revision: 6,
            records: vec![],
        }
        .encode()
        .unwrap();
        let mut payload = native_identity(family::FS, schema::fs::event::STATE, 0);
        payload.extend_from_slice(&event[..24]);
        payload.extend_from_slice(&[0, 0, 3, 0, 1, 0]);
        payload.extend_from_slice(&(body.len() as u32).to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&body);
        let text = describe_payload(EventType::NativeStateRecordWrite, &payload).unwrap();
        assert!(text.contains("phase=delta(3)"));
        assert!(text.contains("action=remove(3)"));
        assert!(text.contains("path=b\"src/gone.rs\" removed_revision=5"));
        for len in 0..88 {
            assert!(describe_payload(EventType::NativeStateRecordWrite, &payload[..len]).is_none());
        }
    }
}
