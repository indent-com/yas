//! The sequential native YAS session: preface, HELLO, and framed family
//! requests over one byte stream.
//!
//! [`NativeClient`] owns its stream and is driven by `&mut self`: one
//! request at a time, with unrelated frames parked in a bounded queue until a
//! caller asks for them. It is what the `yas` CLI uses for one-shot
//! commands. [`NativeClient::into_framed`] splits a connected session into a
//! [`NativeFrameReader`] and a cloneable [`NativeFrameSender`] for
//! long-lived multiplexed use; [`crate::Client`] is built on that split.
//!
//! Endpoint selection is performed by [`crate::transport`]. This module
//! never probes the byte stream for an alternate protocol.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, RwLock};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use yas_wire::frame::DatagramContext;
use yas_wire::{
    Class, Decode, Encode, Extensions, Frame, FrameCodec, FrameHeader, FrameLimits,
    core::{
        CatalogStep, ClientHello, FamilyUpdate, GoAway, Ping, PingResult, ReceiveLimits,
        ResultPrefix, ServerHello, SessionUpdate, Status,
    },
    family,
    state::{Phase, StateAck, StateEvent, Unwatch, Watch, WatchResult},
    transfer::{
        ByteData, Close as TransferClose, Credit, Delivery, Descriptor, Direction,
        InlineOrTransfer, MessageData, MessageReceiver, Mode, Reset as TransferReset,
    },
};

use crate::error::{Error, Result, format_result_detail, wire_error};
use crate::transport;
use crate::{ConnectOptions, HelloOptions};

const HELLO_REQUEST_ID: u32 = 1;
const WATCH_CREDIT: u64 = yas_wire::schema::transport::RECOMMENDED_BUFFERED;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Frames parked while a caller waits for another, at least, and per this many bytes of the
/// declared receive budget (1,024 in the default 16 MiB): a peer within its credit parks no
/// more bytes than the budget.
const MIN_PENDING_FRAMES: usize = 1_024;
const PENDING_BYTES_PER_FRAME: u64 = 16 * 1024;
pub const MAX_COLLECTED_TRANSFER_BYTES: u64 = 256 * 1024 * 1024;

type Reader = Box<dyn AsyncRead + Unpin + Send>;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;

pub struct NativeClient {
    reader: Reader,
    writer: Writer,
    datagram: Option<transport::DatagramTransport>,
    inbound: FrameCodec,
    outbound: FrameCodec,
    hello: ServerHello,
    pending: VecDeque<Frame>,
    pending_bytes: usize,
    local_receive: ReceiveLimits,
    next_request_id: u32,
    started: std::time::Instant,
    negotiated_codecs: Vec<u16>,
}

/// Cloneable framed writer paired with [`NativeFrameReader`].
///
/// Long-lived multiplexed commands (notably Net) cannot hold `&mut
/// NativeClient` while waiting for the next inbound frame: other flows must be
/// able to write concurrently. Splitting after HELLO keeps one ordered writer
/// and one reader while preserving the negotiated codec and Core control
/// handling.
#[derive(Clone)]
pub struct NativeFrameSender {
    inner: Arc<NativeFrameSenderInner>,
}

struct NativeFrameSenderInner {
    writer: tokio::sync::Mutex<Writer>,
    outbound: RwLock<FrameCodec>,
    datagram: Option<transport::DatagramSender>,
}

pub struct NativeFrameReader {
    reader: Reader,
    inbound: FrameCodec,
    sender: NativeFrameSender,
    hello: ServerHello,
    started: std::time::Instant,
    negotiated_codecs: Vec<u16>,
    max_datagram: u32,
    datagram: Option<transport::DatagramReceiver>,
    _datagram_session: Option<transport::DatagramSession>,
}

impl NativeClient {
    /// Connect to `target` (a URI such as `local:NAME`, `socket:/path`,
    /// `ssh:host`; `None` for the configured default) and complete HELLO.
    pub async fn connect(target: Option<&str>, options: &ConnectOptions) -> Result<Self> {
        let transport = transport::connect_target(target, options)
            .await
            .map_err(Error::Connect)?;
        Self::connect_transport(transport, &options.hello).await
    }

    /// Complete the preface and HELLO over an already connected transport.
    pub async fn connect_transport(
        transport: transport::Transport,
        options: &HelloOptions,
    ) -> Result<Self> {
        let (mut reader, mut writer, datagram) = transport.split_with_datagram();

        let families = options.family_offers();
        let max_datagram = datagram
            .as_ref()
            .map_or(0, transport::DatagramTransport::maximum);
        let hello_request = ClientHello {
            min_minor: 1,
            max_minor: 1,
            receive: ReceiveLimits {
                max_buffered: options.receive_budget,
                ..ReceiveLimits::recommended(max_datagram)
            },
            client_instance: rand::random(),
            client_name: options.client_name.clone(),
            client_release: options.client_release.clone(),
            families,
            codecs: Vec::new(),
            extensions: options.extensions()?,
        };
        hello_request
            .validate()
            .map_err(|error| Error::protocol(format!("invalid local HELLO: {error}")))?;
        let hello_frame = Frame {
            header: FrameHeader::request(
                family::CORE,
                yas_wire::core::request_kind::HELLO,
                HELLO_REQUEST_ID,
            ),
            payload: hello_request
                .encode()
                .map_err(|error| Error::protocol(format!("cannot encode HELLO: {error}")))?,
        };
        let pre_hello = FrameCodec::pre_hello();
        let encoded = pre_hello
            .encode_stream(&hello_frame)
            .map_err(|error| Error::protocol(format!("cannot frame HELLO: {error}")))?;
        writer
            .write_all(&yas_wire::PREFACE)
            .await
            .map_err(|error| Error::disconnected(format!("cannot send YAS preface: {error}")))?;
        writer
            .write_all(&encoded)
            .await
            .map_err(|error| Error::disconnected(format!("cannot send YAS HELLO: {error}")))?;

        let result = read_frame(&mut reader, &pre_hello).await?;
        if result.header
            != FrameHeader::result(
                family::CORE,
                yas_wire::core::request_kind::HELLO,
                HELLO_REQUEST_ID,
            )
        {
            return Err(Error::protocol(
                "native YAS listener returned an unexpected HELLO frame",
            ));
        }
        let prefix = ResultPrefix::decode(&result.payload)
            .map_err(|error| Error::protocol(format!("cannot decode HELLO Result: {error}")))?;
        if !prefix.status.is_ok() {
            return Err(Error::status_from(
                "YAS HELLO",
                prefix.status,
                prefix.detail,
            ));
        }
        let hello = ServerHello::decode(&prefix.body)
            .map_err(|error| Error::protocol(format!("cannot decode ServerHello: {error}")))?;
        hello.validate_for_client(&hello_request).map_err(|error| {
            Error::protocol(format!("server selected an invalid YAS catalogue: {error}"))
        })?;
        let codecs = hello
            .negotiated_codecs()
            .map_err(|error| Error::protocol(format!("invalid negotiated codecs: {error}")))?
            .0;
        let inbound = FrameCodec::new(
            FrameLimits {
                max_wire_frame: hello_request.receive.max_frame,
                max_decoded_frame: hello_request.receive.max_decoded,
            },
            codecs.iter().copied(),
        )
        .map_err(|error| Error::protocol(format!("invalid inbound YAS codec: {error}")))?;
        let outbound = FrameCodec::new(
            FrameLimits {
                max_wire_frame: hello.receive.max_frame,
                max_decoded_frame: hello.receive.max_decoded,
            },
            codecs.iter().copied(),
        )
        .map_err(|error| Error::protocol(format!("invalid outbound YAS codec: {error}")))?;

        Ok(Self {
            reader,
            writer,
            datagram,
            inbound,
            outbound,
            hello,
            pending: VecDeque::new(),
            pending_bytes: 0,
            local_receive: hello_request.receive,
            next_request_id: 3,
            started: std::time::Instant::now(),
            negotiated_codecs: codecs,
        })
    }

    pub fn hello(&self) -> &ServerHello {
        &self.hello
    }

    /// The receive budget this client offered in HELLO
    /// ([`crate::HelloOptions::receive_budget`]).
    pub fn receive_budget(&self) -> u64 {
        self.local_receive.max_buffered
    }

    pub fn supports_datagrams(&self) -> bool {
        self.datagram.is_some()
            && self.local_receive.max_datagram > 0
            && self.hello.receive.max_datagram > 0
    }

    /// Convert a connected client into independently driven framed halves.
    /// Request correlation and family-specific demultiplexing become the
    /// caller's responsibility; Core PING/GOAWAY/catalogue traffic remains
    /// handled by [`NativeFrameReader`].
    pub fn into_framed(self) -> (NativeFrameReader, NativeFrameSender) {
        let max_datagram = self.local_receive.max_datagram;
        let (datagram_sender, datagram_receiver, datagram_session) = match self.datagram {
            Some(datagram) => {
                let (sender, receiver, session) = datagram.into_parts();
                (Some(sender), Some(receiver), Some(session))
            }
            None => (None, None, None),
        };
        let sender = NativeFrameSender {
            inner: Arc::new(NativeFrameSenderInner {
                writer: tokio::sync::Mutex::new(self.writer),
                outbound: RwLock::new(self.outbound),
                datagram: datagram_sender,
            }),
        };
        let reader = NativeFrameReader {
            reader: self.reader,
            inbound: self.inbound,
            sender: sender.clone(),
            hello: self.hello,
            started: self.started,
            negotiated_codecs: self.negotiated_codecs,
            max_datagram,
            datagram: datagram_receiver,
            _datagram_session: datagram_session,
        };
        (reader, sender)
    }

    pub fn supports(&self, family_id: u16, class: Class, kind: u16) -> bool {
        self.hello
            .families
            .iter()
            .find(|descriptor| descriptor.family_id == family_id)
            .and_then(|descriptor| descriptor.operation(class, kind))
            .is_some_and(|operation| match class {
                Class::Event => operation.server_accepts || operation.server_sends,
                Class::Request => operation.server_accepts,
                Class::Result => false,
            })
    }

    pub async fn snapshot(
        &mut self,
        family_id: u16,
    ) -> Result<Option<Vec<yas_wire::state::Record>>> {
        let Some(descriptor) = self
            .hello
            .families
            .iter()
            .find(|descriptor| descriptor.family_id == family_id)
        else {
            return Ok(None);
        };
        let supports_watch = descriptor
            .operation(Class::Request, 0)
            .is_some_and(|operation| operation.server_accepts)
            && descriptor
                .operation(Class::Event, 0)
                .is_some_and(|operation| operation.server_sends);
        if !supports_watch {
            return Ok(None);
        }

        let watch = Watch {
            initial_credit: WATCH_CREDIT,
            resume: None,
            extensions: Extensions::default(),
        };
        // State snapshots can contain paths, command descriptors, extension
        // names, and other private session data.  Families which merely
        // allow sensitive WATCH frames accept this too, while families such
        // as Extension require it.
        let result = self
            .request(family_id, 0, watch.encode().map_err(wire_error)?, true)
            .await?;
        let watch_result = WatchResult::decode(&result).map_err(wire_error)?;
        let mut records = Vec::new();
        loop {
            let frame =
                tokio::time::timeout(REQUEST_TIMEOUT, self.next_matching_event(family_id, 0))
                    .await
                    .map_err(|_| {
                        Error::Timeout(format!(
                            "timed out waiting for family {family_id:#06x} snapshot"
                        ))
                    })??;
            let event = StateEvent::decode(&frame.payload).map_err(wire_error)?;
            if event.subscription_id != watch_result.subscription_id {
                continue;
            }
            if event.phase == Phase::SnapshotBegin {
                records.clear();
            }
            if matches!(
                event.phase,
                Phase::SnapshotRecords | Phase::SnapshotEnd | Phase::Delta
            ) {
                records.extend(event.records);
            }
            self.send_event(
                family_id,
                1,
                StateAck {
                    subscription_id: event.subscription_id,
                    applied_revision: event.to_revision,
                    cumulative_byte_limit: WATCH_CREDIT,
                }
                .encode()
                .map_err(wire_error)?,
                false,
            )
            .await?;
            if event.phase == Phase::SnapshotEnd {
                break;
            }
            if event.phase == Phase::Reset {
                return Err(Error::protocol(format!(
                    "family {family_id:#06x} reset its snapshot"
                )));
            }
        }

        let unwatch = Unwatch {
            subscription_id: watch_result.subscription_id,
        };
        let _ = self
            .request(family_id, 1, unwatch.encode().map_err(wire_error)?, false)
            .await?;
        Ok(Some(records))
    }

    pub async fn request(
        &mut self,
        family_id: u16,
        kind: u16,
        payload: Vec<u8>,
        sensitive: bool,
    ) -> Result<Vec<u8>> {
        self.request_with_timeout(family_id, kind, payload, sensitive, REQUEST_TIMEOUT)
            .await
    }

    pub async fn request_with_timeout(
        &mut self,
        family_id: u16,
        kind: u16,
        payload: Vec<u8>,
        sensitive: bool,
        timeout: std::time::Duration,
    ) -> Result<Vec<u8>> {
        let prefix = self
            .request_result_with_timeout(family_id, kind, payload, sensitive, timeout)
            .await?;
        if prefix.status == Status::Ok {
            return Ok(prefix.body);
        }
        Err(Error::status_from(
            format!("YAS request {family_id:#06x}/{kind:#06x}"),
            prefix.status,
            prefix.detail,
        ))
    }

    /// Send one correlated Request while leaving operation-level statuses to
    /// the caller. Commands with useful negative answers (for example KV
    /// `NOT_FOUND` and compare-and-swap `CONFLICT`) must not recover those
    /// statuses by parsing an error string.
    pub async fn request_result(
        &mut self,
        family_id: u16,
        kind: u16,
        payload: Vec<u8>,
        sensitive: bool,
    ) -> Result<ResultPrefix> {
        self.request_result_with_timeout(family_id, kind, payload, sensitive, REQUEST_TIMEOUT)
            .await
    }

    pub async fn request_result_with_timeout(
        &mut self,
        family_id: u16,
        kind: u16,
        payload: Vec<u8>,
        sensitive: bool,
        timeout: std::time::Duration,
    ) -> Result<ResultPrefix> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(2).max(3) | 1;
        let mut header = FrameHeader::request(family_id, kind, request_id);
        header.sensitive = sensitive;
        self.send(Frame { header, payload }).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let frame = tokio::time::timeout_at(deadline, self.read_next())
                .await
                .map_err(|_| {
                    Error::Timeout(format!(
                        "timed out waiting for {family_id:#06x}/{kind:#06x} Result"
                    ))
                })??;
            if frame.header.class == Class::Result
                && frame.header.family == family_id
                && frame.header.kind == kind
                && frame.header.request_id == Some(request_id)
            {
                let prefix = ResultPrefix::decode(&frame.payload).map_err(wire_error)?;
                return Ok(prefix);
            }
            if frame.header.class == Class::Result {
                return Err(Error::protocol(format!(
                    "YAS returned an uncorrelated Result for {:#06x}/{:#06x}/{:?}",
                    frame.header.family, frame.header.kind, frame.header.request_id
                )));
            }
            self.defer(frame)?;
        }
    }

    pub async fn request_typed<Request, Response>(
        &mut self,
        family_id: u16,
        kind: u16,
        request: &Request,
        sensitive: bool,
    ) -> Result<Response>
    where
        Request: Encode,
        Response: Decode,
    {
        let payload = request.encode().map_err(wire_error)?;
        let body = self.request(family_id, kind, payload, sensitive).await?;
        Response::decode(&body).map_err(wire_error)
    }

    pub async fn request_typed_with_timeout<Request, Response>(
        &mut self,
        family_id: u16,
        kind: u16,
        request: &Request,
        sensitive: bool,
        timeout: std::time::Duration,
    ) -> Result<Response>
    where
        Request: Encode,
        Response: Decode,
    {
        let payload = request.encode().map_err(wire_error)?;
        let body = self
            .request_with_timeout(family_id, kind, payload, sensitive, timeout)
            .await?;
        Response::decode(&body).map_err(wire_error)
    }

    pub async fn send_event(
        &mut self,
        family_id: u16,
        kind: u16,
        payload: Vec<u8>,
        sensitive: bool,
    ) -> Result<()> {
        let mut header = FrameHeader::event(family_id, kind);
        header.sensitive = sensitive;
        self.send(Frame { header, payload }).await
    }

    pub async fn send_typed_event<Event: Encode>(
        &mut self,
        family_id: u16,
        kind: u16,
        event: &Event,
        sensitive: bool,
    ) -> Result<()> {
        self.send_event(
            family_id,
            kind,
            event.encode().map_err(wire_error)?,
            sensitive,
        )
        .await
    }

    async fn send(&mut self, frame: Frame) -> Result<()> {
        let bytes = self.outbound.encode_stream(&frame).map_err(wire_error)?;
        self.writer
            .write_all(&bytes)
            .await
            .map_err(|error| Error::disconnected(format!("cannot write YAS frame: {error}")))
    }

    pub async fn next_matching_event(&mut self, family_id: u16, kind: u16) -> Result<Frame> {
        if let Some(index) = self.pending.iter().position(|frame| {
            frame.header.class == Class::Event
                && frame.header.family == family_id
                && frame.header.kind == kind
        }) {
            let frame = self
                .pending
                .remove(index)
                .ok_or_else(|| Error::protocol("pending YAS event disappeared"))?;
            self.pending_bytes = self.pending_bytes.saturating_sub(frame.payload.len());
            return Ok(frame);
        }
        loop {
            let frame = self.read_next().await?;
            if frame.header.class == Class::Event
                && frame.header.family == family_id
                && frame.header.kind == kind
            {
                return Ok(frame);
            }
            self.defer(frame)?;
        }
    }

    pub async fn next_typed_event<Event: Decode>(
        &mut self,
        family_id: u16,
        kind: u16,
    ) -> Result<Event> {
        let frame = self.next_matching_event(family_id, kind).await?;
        Event::decode(&frame.payload).map_err(wire_error)
    }

    pub async fn next_event(&mut self) -> Result<Frame> {
        if let Some(index) = self
            .pending
            .iter()
            .position(|frame| frame.header.class == Class::Event)
        {
            let frame = self
                .pending
                .remove(index)
                .ok_or_else(|| Error::protocol("pending YAS event disappeared"))?;
            self.pending_bytes = self.pending_bytes.saturating_sub(frame.payload.len());
            return Ok(frame);
        }
        loop {
            let frame = self.read_next().await?;
            if frame.header.class == Class::Event {
                return Ok(frame);
            }
            if frame.header.class == Class::Result {
                return Err(Error::protocol(format!(
                    "YAS returned an unsolicited Result for {:#06x}/{:#06x}/{:?}",
                    frame.header.family, frame.header.kind, frame.header.request_id
                )));
            }
        }
    }

    pub async fn receive_inline_or_transfer(
        &mut self,
        value: InlineOrTransfer,
        maximum: u64,
    ) -> Result<Vec<u8>> {
        if value.byte_len > maximum {
            return Err(Error::protocol(format!(
                "YAS delivery is {} bytes; collection limit is {maximum}",
                value.byte_len
            )));
        }
        let bytes = match value.delivery {
            Delivery::Inline(bytes) => bytes,
            Delivery::Transfer(descriptor) => {
                self.receive_byte_transfer(&descriptor, Some(value.byte_len), maximum)
                    .await?
            }
        };
        if bytes.len() as u64 != value.byte_len {
            return Err(Error::protocol(format!(
                "YAS delivery length mismatch: declared {}, received {}",
                value.byte_len,
                bytes.len()
            )));
        }
        if blake3::hash(&bytes).as_bytes() != &value.content_hash {
            return Err(Error::protocol("YAS delivery content hash mismatch"));
        }
        Ok(bytes)
    }

    pub async fn receive_byte_transfer(
        &mut self,
        descriptor: &Descriptor,
        expected_length: Option<u64>,
        maximum: u64,
    ) -> Result<Vec<u8>> {
        descriptor.validate().map_err(wire_error)?;
        if descriptor.mode != Mode::Byte || descriptor.direction != Direction::SENDER_TO_RECEIVER {
            return Err(Error::protocol(
                "YAS delivery did not provide a server-to-client BYTE Transfer",
            ));
        }
        if maximum == 0 || expected_length.is_some_and(|length| length > maximum) {
            return Err(Error::protocol("invalid YAS Transfer collection limit"));
        }
        if descriptor.sender_send_credit > maximum {
            return Err(Error::protocol(format!(
                "YAS Transfer initial credit {} exceeds collection limit {maximum}",
                descriptor.sender_send_credit
            )));
        }
        if descriptor.sender_send_credit < maximum {
            self.send_typed_event(
                family::TRANSFER,
                yas_wire::transfer::kind::CREDIT,
                &Credit {
                    transfer_id: descriptor.transfer_id,
                    cumulative_limit: maximum,
                },
                false,
            )
            .await?;
        }

        let capacity = expected_length
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0);
        let mut bytes = Vec::with_capacity(capacity);
        loop {
            let frame = self.next_transfer_event(descriptor.transfer_id).await?;
            let sensitive = descriptor
                .requires_sensitive_frame(frame.header.kind)
                .map_err(wire_error)?;
            if frame.header.sensitive != sensitive {
                return Err(Error::protocol(format!(
                    "YAS Transfer {:#010x} sensitivity flag mismatch",
                    descriptor.transfer_id
                )));
            }
            match frame.header.kind {
                yas_wire::transfer::kind::BYTE_DATA => {
                    let data = ByteData::decode(&frame.payload).map_err(wire_error)?;
                    if data.offset != bytes.len() as u64
                        || data.data.len() > descriptor.max_chunk_bytes as usize
                    {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {:#010x} sent a non-contiguous or oversized chunk",
                            descriptor.transfer_id
                        )));
                    }
                    let next = (bytes.len() as u64)
                        .checked_add(data.data.len() as u64)
                        .ok_or_else(|| Error::protocol("YAS Transfer length overflow"))?;
                    if next > maximum || expected_length.is_some_and(|length| next > length) {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {:#010x} exceeded its declared collection limit",
                            descriptor.transfer_id
                        )));
                    }
                    bytes.extend_from_slice(&data.data);
                }
                yas_wire::transfer::kind::CLOSE => {
                    let close = TransferClose::decode(&frame.payload).map_err(wire_error)?;
                    if close.final_data_bytes != bytes.len() as u64 {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {:#010x} CLOSE length mismatch",
                            descriptor.transfer_id
                        )));
                    }
                    if close.status != Status::Ok.code() {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {:#010x} closed with status {}: {}",
                            descriptor.transfer_id,
                            close.status,
                            String::from_utf8_lossy(&close.detail)
                        )));
                    }
                    if expected_length.is_some_and(|length| length != bytes.len() as u64) {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {:#010x} ended before its declared length",
                            descriptor.transfer_id
                        )));
                    }
                    return Ok(bytes);
                }
                yas_wire::transfer::kind::RESET => {
                    let reset = TransferReset::decode(&frame.payload).map_err(wire_error)?;
                    return Err(Error::protocol(format!(
                        "YAS Transfer {:#010x} reset with status {}: {}",
                        descriptor.transfer_id,
                        reset.status,
                        String::from_utf8_lossy(&reset.detail)
                    )));
                }
                other => {
                    return Err(Error::protocol(format!(
                        "YAS Transfer {:#010x} received unexpected event {other:#06x}",
                        descriptor.transfer_id
                    )));
                }
            }
        }
    }

    pub async fn send_byte_transfer(
        &mut self,
        descriptor: &Descriptor,
        bytes: &[u8],
    ) -> Result<()> {
        descriptor.validate().map_err(wire_error)?;
        if descriptor.mode != Mode::Byte || descriptor.direction != Direction::RECEIVER_TO_SENDER {
            return Err(Error::protocol(
                "YAS upload did not provide a client-to-server BYTE Transfer",
            ));
        }
        let mut offset = 0u64;
        let mut credit = descriptor.receiver_send_credit;
        while offset < bytes.len() as u64 {
            while offset >= credit {
                let frame = self.next_transfer_event(descriptor.transfer_id).await?;
                match frame.header.kind {
                    yas_wire::transfer::kind::CREDIT => {
                        if frame.header.sensitive {
                            return Err(Error::protocol(
                                "YAS Transfer CREDIT was marked sensitive",
                            ));
                        }
                        let update = Credit::decode(&frame.payload).map_err(wire_error)?;
                        if update.cumulative_limit <= credit {
                            return Err(Error::protocol("YAS Transfer credit did not increase"));
                        }
                        credit = update.cumulative_limit;
                    }
                    yas_wire::transfer::kind::RESET => {
                        let reset = TransferReset::decode(&frame.payload).map_err(wire_error)?;
                        return Err(Error::protocol(format!(
                            "YAS Transfer {:#010x} reset with status {}: {}",
                            descriptor.transfer_id,
                            reset.status,
                            String::from_utf8_lossy(&reset.detail)
                        )));
                    }
                    other => {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {:#010x} received unexpected upload event {other:#06x}",
                            descriptor.transfer_id
                        )));
                    }
                }
            }
            let available = usize::try_from(credit - offset).unwrap_or(usize::MAX);
            let remaining = &bytes[usize::try_from(offset).map_err(|_| {
                Error::protocol("YAS Transfer upload offset exceeds this platform's address space")
            })?..];
            let length = remaining
                .len()
                .min(descriptor.max_chunk_bytes as usize)
                .min(available);
            if length == 0 {
                return Err(Error::protocol("YAS Transfer made no upload progress"));
            }
            let data = ByteData {
                transfer_id: descriptor.transfer_id,
                offset,
                data: remaining[..length].to_vec(),
            };
            self.send_typed_event(
                family::TRANSFER,
                yas_wire::transfer::kind::BYTE_DATA,
                &data,
                descriptor
                    .requires_sensitive_frame(yas_wire::transfer::kind::BYTE_DATA)
                    .map_err(wire_error)?,
            )
            .await?;
            offset += length as u64;
        }
        let close = TransferClose {
            transfer_id: descriptor.transfer_id,
            final_data_bytes: offset,
            status: Status::Ok.code(),
            detail: Vec::new(),
        };
        self.send_typed_event(
            family::TRANSFER,
            yas_wire::transfer::kind::CLOSE,
            &close,
            descriptor
                .requires_sensitive_frame(yas_wire::transfer::kind::CLOSE)
                .map_err(wire_error)?,
        )
        .await
    }

    /// Collect a bounded server-to-client MESSAGE Transfer. Returned items
    /// are ordered by sequence number even when the peer interleaves their
    /// fragments within its negotiated open-message window.
    pub async fn receive_message_transfer(
        &mut self,
        descriptor: &Descriptor,
        maximum_bytes: u64,
        maximum_messages: usize,
    ) -> Result<Vec<Vec<u8>>> {
        descriptor.validate().map_err(wire_error)?;
        if descriptor.mode != Mode::Message || descriptor.direction != Direction::SENDER_TO_RECEIVER
        {
            return Err(Error::protocol(
                "YAS delivery did not provide a server-to-client MESSAGE Transfer",
            ));
        }
        if maximum_bytes == 0 || maximum_messages == 0 {
            return Err(Error::protocol(
                "invalid YAS MESSAGE Transfer collection limit",
            ));
        }
        if descriptor.sender_send_credit > maximum_bytes {
            return Err(Error::protocol(format!(
                "YAS Transfer initial credit {} exceeds collection limit {maximum_bytes}",
                descriptor.sender_send_credit
            )));
        }
        if descriptor.sender_send_credit < maximum_bytes {
            self.send_typed_event(
                family::TRANSFER,
                yas_wire::transfer::kind::CREDIT,
                &Credit {
                    transfer_id: descriptor.transfer_id,
                    cumulative_limit: maximum_bytes,
                },
                false,
            )
            .await?;
        }

        let mut validator = MessageReceiver::new(descriptor).map_err(wire_error)?;
        let mut open = BTreeMap::<u64, Vec<u8>>::new();
        let mut complete = BTreeMap::<u64, Vec<u8>>::new();
        let mut received = 0u64;
        loop {
            let frame = self.next_transfer_event(descriptor.transfer_id).await?;
            let sensitive = descriptor
                .requires_sensitive_frame(frame.header.kind)
                .map_err(wire_error)?;
            if frame.header.sensitive != sensitive {
                return Err(Error::protocol(format!(
                    "YAS Transfer {:#010x} sensitivity flag mismatch",
                    descriptor.transfer_id
                )));
            }
            match frame.header.kind {
                yas_wire::transfer::kind::MESSAGE_DATA => {
                    let fragment = MessageData::decode(&frame.payload).map_err(wire_error)?;
                    let ended = validator.accept(&fragment).map_err(wire_error)?;
                    received = received
                        .checked_add(fragment.data.len() as u64)
                        .ok_or_else(|| Error::protocol("YAS MESSAGE Transfer length overflow"))?;
                    if received > maximum_bytes {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {:#010x} exceeded its byte collection limit",
                            descriptor.transfer_id
                        )));
                    }
                    if fragment.start {
                        if open.len() + complete.len() >= maximum_messages
                            || complete.contains_key(&fragment.sequence)
                        {
                            return Err(Error::protocol(format!(
                                "YAS Transfer {:#010x} exceeded its message collection limit",
                                descriptor.transfer_id
                            )));
                        }
                        open.insert(fragment.sequence, Vec::new());
                    }
                    let item = open.get_mut(&fragment.sequence).ok_or_else(|| {
                        Error::protocol(format!(
                            "YAS Transfer {:#010x} lost an open message",
                            descriptor.transfer_id
                        ))
                    })?;
                    item.extend_from_slice(&fragment.data);
                    if ended {
                        let item = open.remove(&fragment.sequence).ok_or_else(|| {
                            Error::protocol("completed YAS Transfer message disappeared")
                        })?;
                        if complete.insert(fragment.sequence, item).is_some() {
                            return Err(Error::protocol("duplicate YAS Transfer message sequence"));
                        }
                    }
                }
                yas_wire::transfer::kind::CLOSE => {
                    let close = TransferClose::decode(&frame.payload).map_err(wire_error)?;
                    if close.final_data_bytes != received || validator.open_messages() != 0 {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {:#010x} CLOSE accounting mismatch",
                            descriptor.transfer_id
                        )));
                    }
                    if close.status != Status::Ok.code() {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {:#010x} closed with status {}: {}",
                            descriptor.transfer_id,
                            close.status,
                            String::from_utf8_lossy(&close.detail)
                        )));
                    }
                    return Ok(complete.into_values().collect());
                }
                yas_wire::transfer::kind::RESET => {
                    let reset = TransferReset::decode(&frame.payload).map_err(wire_error)?;
                    return Err(Error::protocol(format!(
                        "YAS Transfer {:#010x} reset with status {}: {}",
                        descriptor.transfer_id,
                        reset.status,
                        String::from_utf8_lossy(&reset.detail)
                    )));
                }
                other => {
                    return Err(Error::protocol(format!(
                        "YAS Transfer {:#010x} received unexpected message event {other:#06x}",
                        descriptor.transfer_id
                    )));
                }
            }
        }
    }

    async fn next_transfer_event(&mut self, transfer_id: u32) -> Result<Frame> {
        if let Some(index) = self.pending.iter().position(|frame| {
            frame.header.class == Class::Event
                && frame.header.family == family::TRANSFER
                && transfer_event_id(frame) == Some(transfer_id)
        }) {
            let frame = self
                .pending
                .remove(index)
                .ok_or_else(|| Error::protocol("pending YAS Transfer event disappeared"))?;
            self.pending_bytes = self.pending_bytes.saturating_sub(frame.payload.len());
            return Ok(frame);
        }
        loop {
            let frame = self.read_next().await?;
            if frame.header.class == Class::Event
                && frame.header.family == family::TRANSFER
                && transfer_event_id(&frame) == Some(transfer_id)
            {
                return Ok(frame);
            }
            self.defer(frame)?;
        }
    }

    fn defer(&mut self, frame: Frame) -> Result<()> {
        let next_bytes = self
            .pending_bytes
            .checked_add(frame.payload.len())
            .ok_or_else(|| Error::protocol("pending YAS frame accounting overflow"))?;
        let budget = self.local_receive.max_buffered;
        let max_frames = usize::try_from(budget / PENDING_BYTES_PER_FRAME)
            .unwrap_or(usize::MAX)
            .max(MIN_PENDING_FRAMES);
        if self.pending.len() >= max_frames
            || u64::try_from(next_bytes).unwrap_or(u64::MAX) > budget
        {
            return Err(Error::protocol(
                "native YAS peer exceeded the bounded pending-frame queue",
            ));
        }
        self.pending_bytes = next_bytes;
        self.pending.push_back(frame);
        Ok(())
    }

    async fn read_next(&mut self) -> Result<Frame> {
        loop {
            let frame = read_frame(&mut self.reader, &self.inbound).await?;
            if frame.header.class == Class::Request
                && frame.header.family == family::CORE
                && frame.header.kind == yas_wire::core::request_kind::PING
            {
                self.answer_ping(frame).await?;
                continue;
            }
            if frame.header.class == Class::Request {
                return Err(Error::protocol(format!(
                    "YAS server sent an unsupported peer Request {:#06x}/{:#06x}",
                    frame.header.family, frame.header.kind
                )));
            }
            if frame.header.class == Class::Event
                && frame.header.family == family::CORE
                && frame.header.kind == yas_wire::core::event_kind::GOAWAY
            {
                let goaway = GoAway::decode(&frame.payload).map_err(wire_error)?;
                return Err(Error::GoAway {
                    status: goaway.status,
                    detail: format_result_detail(&goaway.detail),
                });
            }
            if frame.header.class == Class::Event
                && frame.header.family == family::CORE
                && frame.header.kind == yas_wire::core::event_kind::SESSION_UPDATE
            {
                self.apply_session_update(&frame.payload)?;
                continue;
            }
            if frame.header.class == Class::Event
                && frame.header.family == family::CORE
                && frame.header.kind == yas_wire::core::event_kind::FAMILY_UPDATE
            {
                self.apply_family_update(&frame.payload)?;
                continue;
            }
            return Ok(frame);
        }
    }

    async fn answer_ping(&mut self, frame: Frame) -> Result<()> {
        let request_id = frame
            .header
            .request_id
            .ok_or_else(|| Error::protocol("YAS PING Request has no request ID"))?;
        let _ping = Ping::decode(&frame.payload).map_err(wire_error)?;
        let receive_ns = self.monotonic_ns();
        let result = PingResult {
            receiver_receive_ns: receive_ns,
            receiver_send_ns: self.monotonic_ns().max(receive_ns),
        };
        let prefix = ResultPrefix {
            status: Status::Ok,
            detail: Extensions::default(),
            body: result.encode().map_err(wire_error)?,
        };
        self.send(Frame {
            header: FrameHeader::result(
                family::CORE,
                yas_wire::core::request_kind::PING,
                request_id,
            ),
            payload: prefix.encode().map_err(wire_error)?,
        })
        .await
    }

    pub fn monotonic_ns(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn apply_session_update(&mut self, payload: &[u8]) -> Result<()> {
        let update = SessionUpdate::decode(payload).map_err(wire_error)?;
        let step = update
            .validate_after(self.hello.catalog_revision, &self.hello.receive)
            .map_err(wire_error)?;
        if step == CatalogStep::Gap {
            return Err(Error::protocol(format!(
                "YAS catalogue jumped from revision {} to {}; reconnect required",
                self.hello.catalog_revision, update.catalog_revision
            )));
        }
        self.outbound = FrameCodec::new(
            FrameLimits {
                max_wire_frame: update.receive.max_frame,
                max_decoded_frame: update.receive.max_decoded,
            },
            self.negotiated_codecs.iter().copied(),
        )
        .map_err(wire_error)?;
        self.hello.receive = update.receive;
        self.hello.catalog_revision = update.catalog_revision;
        Ok(())
    }

    fn apply_family_update(&mut self, payload: &[u8]) -> Result<()> {
        let update = FamilyUpdate::decode(payload).map_err(wire_error)?;
        let descriptor = self
            .hello
            .families
            .iter_mut()
            .find(|descriptor| descriptor.family_id == update.family.family_id)
            .ok_or_else(|| {
                Error::protocol(format!(
                    "YAS FAMILY_UPDATE introduced unknown family {:#06x}; reconnect required",
                    update.family.family_id
                ))
            })?;
        let step = update
            .validate_after(self.hello.catalog_revision, descriptor)
            .map_err(wire_error)?;
        if step == CatalogStep::Gap {
            return Err(Error::protocol(format!(
                "YAS catalogue jumped from revision {} to {}; reconnect required",
                self.hello.catalog_revision, update.catalog_revision
            )));
        }
        *descriptor = update.family;
        self.hello.catalog_revision = update.catalog_revision;
        Ok(())
    }
}

impl NativeFrameSender {
    pub async fn send(&self, frame: Frame) -> Result<()> {
        let bytes = {
            let codec = self
                .inner
                .outbound
                .read()
                .map_err(|_| Error::protocol("native YAS outbound codec lock is poisoned"))?;
            codec.encode_stream(&frame).map_err(wire_error)?
        };
        self.inner
            .writer
            .lock()
            .await
            .write_all(&bytes)
            .await
            .map_err(|error| Error::disconnected(format!("cannot write YAS frame: {error}")))
    }

    /// Attempt one message-preserving transport datagram. Queue or SCTP
    /// congestion returns `Ok(false)` and is ordinary packet loss.
    pub fn try_send_datagram(
        &self,
        frame: &Frame,
        maximum: u32,
        context: DatagramContext,
    ) -> Result<transport::DatagramSend> {
        let sender = self.inner.datagram.as_ref().ok_or_else(|| {
            Error::Unsupported("native YAS datagram transport is unavailable".into())
        })?;
        let bytes = self
            .inner
            .outbound
            .read()
            .map_err(|_| Error::protocol("native YAS outbound codec lock is poisoned"))?
            .encode_datagram(frame, maximum, context)
            .map_err(wire_error)?;
        Ok(sender.try_send(bytes))
    }

    fn replace_codec(&self, codec: FrameCodec) -> Result<()> {
        *self
            .inner
            .outbound
            .write()
            .map_err(|_| Error::protocol("native YAS outbound codec lock is poisoned"))? = codec;
        Ok(())
    }
}

impl NativeFrameReader {
    /// The server HELLO as updated by the latest catalog revision.
    pub fn hello(&self) -> &ServerHello {
        &self.hello
    }

    /// Read the next family frame after servicing Core control traffic.
    pub async fn next(&mut self) -> Result<Frame> {
        self.next_with_source().await.map(|(frame, _)| frame)
    }

    /// Read the next family frame and report whether it arrived on the lossy
    /// transport sideband. Malformed datagrams are dropped without affecting
    /// the reliable session.
    pub async fn next_with_source(&mut self) -> Result<(Frame, bool)> {
        loop {
            let (frame, datagram) = if let Some(receiver) = self.datagram.as_mut() {
                tokio::select! {
                    result = read_frame(&mut self.reader, &self.inbound) => (result?, false),
                    bytes = receiver.recv() => {
                        let Some(bytes) = bytes else {
                            self.datagram = None;
                            continue;
                        };
                        let Some(frame) = decode_transport_datagram(
                            &self.inbound,
                            &bytes,
                            self.max_datagram,
                        ) else {
                            continue;
                        };
                        (frame, true)
                    }
                }
            } else {
                (read_frame(&mut self.reader, &self.inbound).await?, false)
            };
            if frame.header.class == Class::Request
                && frame.header.family == family::CORE
                && frame.header.kind == yas_wire::core::request_kind::PING
            {
                self.answer_ping(frame).await?;
                continue;
            }
            if frame.header.class == Class::Request {
                return Err(Error::protocol(format!(
                    "YAS server sent an unsupported peer Request {:#06x}/{:#06x}",
                    frame.header.family, frame.header.kind
                )));
            }
            if frame.header.class == Class::Event
                && frame.header.family == family::CORE
                && frame.header.kind == yas_wire::core::event_kind::GOAWAY
            {
                let goaway = GoAway::decode(&frame.payload).map_err(wire_error)?;
                return Err(Error::GoAway {
                    status: goaway.status,
                    detail: format_result_detail(&goaway.detail),
                });
            }
            if frame.header.class == Class::Event
                && frame.header.family == family::CORE
                && frame.header.kind == yas_wire::core::event_kind::SESSION_UPDATE
            {
                self.apply_session_update(&frame.payload)?;
                continue;
            }
            if frame.header.class == Class::Event
                && frame.header.family == family::CORE
                && frame.header.kind == yas_wire::core::event_kind::FAMILY_UPDATE
            {
                self.apply_family_update(&frame.payload)?;
                continue;
            }
            return Ok((frame, datagram));
        }
    }

    async fn answer_ping(&mut self, frame: Frame) -> Result<()> {
        let request_id = frame
            .header
            .request_id
            .ok_or_else(|| Error::protocol("YAS PING Request has no request ID"))?;
        let _ping = Ping::decode(&frame.payload).map_err(wire_error)?;
        let receive_ns = self.monotonic_ns();
        let result = PingResult {
            receiver_receive_ns: receive_ns,
            receiver_send_ns: self.monotonic_ns().max(receive_ns),
        };
        let prefix = ResultPrefix {
            status: Status::Ok,
            detail: Extensions::default(),
            body: result.encode().map_err(wire_error)?,
        };
        self.sender
            .send(Frame {
                header: FrameHeader::result(
                    family::CORE,
                    yas_wire::core::request_kind::PING,
                    request_id,
                ),
                payload: prefix.encode().map_err(wire_error)?,
            })
            .await
    }

    fn monotonic_ns(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn apply_session_update(&mut self, payload: &[u8]) -> Result<()> {
        let update = SessionUpdate::decode(payload).map_err(wire_error)?;
        let step = update
            .validate_after(self.hello.catalog_revision, &self.hello.receive)
            .map_err(wire_error)?;
        if step == CatalogStep::Gap {
            return Err(Error::protocol(format!(
                "YAS catalogue jumped from revision {} to {}; reconnect required",
                self.hello.catalog_revision, update.catalog_revision
            )));
        }
        let codec = FrameCodec::new(
            FrameLimits {
                max_wire_frame: update.receive.max_frame,
                max_decoded_frame: update.receive.max_decoded,
            },
            self.negotiated_codecs.iter().copied(),
        )
        .map_err(wire_error)?;
        self.sender.replace_codec(codec)?;
        self.hello.receive = update.receive;
        self.hello.catalog_revision = update.catalog_revision;
        Ok(())
    }

    fn apply_family_update(&mut self, payload: &[u8]) -> Result<()> {
        let update = FamilyUpdate::decode(payload).map_err(wire_error)?;
        let descriptor = self
            .hello
            .families
            .iter_mut()
            .find(|descriptor| descriptor.family_id == update.family.family_id)
            .ok_or_else(|| {
                Error::protocol(format!(
                    "YAS FAMILY_UPDATE introduced unknown family {:#06x}; reconnect required",
                    update.family.family_id
                ))
            })?;
        let step = update
            .validate_after(self.hello.catalog_revision, descriptor)
            .map_err(wire_error)?;
        if step == CatalogStep::Gap {
            return Err(Error::protocol(format!(
                "YAS catalogue jumped from revision {} to {}; reconnect required",
                self.hello.catalog_revision, update.catalog_revision
            )));
        }
        *descriptor = update.family;
        self.hello.catalog_revision = update.catalog_revision;
        Ok(())
    }
}

fn decode_transport_datagram(codec: &FrameCodec, bytes: &[u8], maximum: u32) -> Option<Frame> {
    let probe = codec.decode(bytes).ok()?;
    let context = match (probe.header.family, probe.header.class, probe.header.kind) {
        (family::NET, Class::Event, yas_wire::schema::net::event::DATAGRAM) => {
            DatagramContext::NetNativeFlow
        }
        (family::SURFACE, Class::Event, yas_wire::schema::surface::event::FRAME) => {
            DatagramContext::SurfaceFrame
        }
        (family::MEDIA, Class::Event, yas_wire::schema::media::event::FRAME) => {
            DatagramContext::MediaFrame
        }
        _ => return None,
    };
    codec.decode_datagram(bytes, maximum, context).ok()
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin), codec: &FrameCodec) -> Result<Frame> {
    let mut length = [0; 4];
    reader
        .read_exact(&mut length)
        .await
        .map_err(|error| Error::disconnected(format!("cannot read YAS frame length: {error}")))?;
    let length = u32::from_le_bytes(length) as usize;
    let total = length
        .checked_add(4)
        .ok_or_else(|| Error::protocol("YAS frame length overflow"))?;
    if total > codec.limits().max_wire_frame as usize + 4 {
        return Err(Error::protocol(format!(
            "YAS frame exceeds negotiated limit: {length}"
        )));
    }
    let mut bytes = vec![0; total];
    bytes[..4].copy_from_slice(&(length as u32).to_le_bytes());
    reader
        .read_exact(&mut bytes[4..])
        .await
        .map_err(|error| Error::disconnected(format!("cannot read YAS frame: {error}")))?;
    let (frame, consumed) = codec.decode_stream(&bytes).map_err(wire_error)?;
    if consumed != bytes.len() {
        return Err(Error::protocol(
            "YAS decoder did not consume one complete frame",
        ));
    }
    Ok(frame)
}

fn transfer_event_id(frame: &Frame) -> Option<u32> {
    (frame.header.class == Class::Event
        && frame.header.family == family::TRANSFER
        && matches!(
            frame.header.kind,
            yas_wire::transfer::kind::BYTE_DATA
                | yas_wire::transfer::kind::MESSAGE_DATA
                | yas_wire::transfer::kind::CREDIT
                | yas_wire::transfer::kind::CLOSE
                | yas_wire::transfer::kind::RESET
        )
        && frame.payload.len() >= 4)
        .then(|| {
            u32::from_le_bytes(
                frame.payload[..4]
                    .try_into()
                    .expect("checked Transfer ID length"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use yas_wire::core::{FamilyDescriptor, Operation, RuntimeState, SessionUpdate};

    fn test_server_hello() -> ServerHello {
        ServerHello {
            minor: 1,
            boot_id: [1; 16],
            session_id: [2; 16],
            receive: ReceiveLimits::recommended(0),
            server_monotonic_ns: 3,
            catalog_revision: 1,
            server_name: "home".into(),
            server_release: "test".into(),
            families: vec![FamilyDescriptor {
                family_id: family::CORE,
                version: yas_wire::core::VERSION,
                runtime_state: RuntimeState::Available,
                operations: vec![
                    Operation {
                        server_accepts: true,
                        server_sends: true,
                        class: Class::Request,
                        kind: yas_wire::core::request_kind::PING,
                    },
                    Operation {
                        server_accepts: false,
                        server_sends: true,
                        class: Class::Event,
                        kind: yas_wire::core::event_kind::SESSION_UPDATE,
                    },
                ],
                limits: Extensions::default(),
            }],
            extensions: Extensions::default(),
        }
    }

    /// Answer a client's HELLO on `server_stream` with [`test_server_hello`]: the codec
    /// for what follows, and the client's HELLO.
    async fn answer_hello(
        server_stream: &mut tokio::io::DuplexStream,
    ) -> (FrameCodec, ClientHello) {
        let mut preface = [0; yas_wire::PREFACE.len()];
        server_stream.read_exact(&mut preface).await.unwrap();
        let pre_hello = FrameCodec::pre_hello();
        let hello_frame = read_frame(server_stream, &pre_hello).await.unwrap();
        let client_hello = ClientHello::decode(&hello_frame.payload).unwrap();
        let result_frame = Frame {
            header: FrameHeader::result(
                family::CORE,
                yas_wire::core::request_kind::HELLO,
                HELLO_REQUEST_ID,
            ),
            payload: ResultPrefix {
                status: Status::Ok,
                detail: Extensions::default(),
                body: test_server_hello().encode().unwrap(),
            }
            .encode()
            .unwrap(),
        };
        server_stream
            .write_all(&pre_hello.encode_stream(&result_frame).unwrap())
            .await
            .unwrap();
        let codec = FrameCodec::new(
            FrameLimits {
                max_wire_frame: client_hello.receive.max_frame,
                max_decoded_frame: client_hello.receive.max_decoded,
            },
            [],
        )
        .unwrap();
        (codec, client_hello)
    }

    /// A client that declares a wider receive budget parks as much as it declared while it
    /// waits for another frame: 17 MiB of Transfer data in 1,372 frames within 64 MiB, past
    /// both the default's caps (16 MiB, 1,024 frames).
    #[tokio::test]
    async fn frames_parked_within_a_wider_receive_budget_keep_the_session() {
        const BUDGET: u64 = 64 << 20;
        const CHUNK: usize = 64 * 1024;
        const CHUNKS: usize = 272;
        const SMALL: usize = 1_100;
        let (client_stream, mut server_stream) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let (codec, client_hello) = answer_hello(&mut server_stream).await;
            assert_eq!(client_hello.receive.max_buffered, BUDGET);
            for index in 0..CHUNKS + SMALL {
                let (offset, size) = if index < CHUNKS {
                    (index * CHUNK, CHUNK)
                } else {
                    (CHUNKS * CHUNK + index - CHUNKS, 1)
                };
                let data = Frame {
                    header: FrameHeader::event(
                        family::TRANSFER,
                        yas_wire::transfer::kind::BYTE_DATA,
                    ),
                    payload: ByteData {
                        transfer_id: 7,
                        offset: offset as u64,
                        data: vec![0; size],
                    }
                    .encode()
                    .unwrap(),
                };
                server_stream
                    .write_all(&codec.encode_stream(&data).unwrap())
                    .await
                    .unwrap();
            }
            // What the client waits for: another Transfer's end.
            let close = Frame {
                header: FrameHeader::event(family::TRANSFER, yas_wire::transfer::kind::CLOSE),
                payload: TransferClose {
                    transfer_id: 8,
                    final_data_bytes: 0,
                    status: Status::Ok.code(),
                    detail: Vec::new(),
                }
                .encode()
                .unwrap(),
            };
            server_stream
                .write_all(&codec.encode_stream(&close).unwrap())
                .await
                .unwrap();
            server_stream
        });
        let mut client = NativeClient::connect_transport(
            transport::Transport::Duplex(client_stream),
            &HelloOptions::named("yas-test").receive_budget(BUDGET),
        )
        .await
        .unwrap();
        assert_eq!(client.receive_budget(), BUDGET);
        let close = client
            .next_matching_event(family::TRANSFER, yas_wire::transfer::kind::CLOSE)
            .await
            .unwrap();
        assert_eq!(
            TransferClose::decode(&close.payload).unwrap().transfer_id,
            8
        );
        assert_eq!(client.pending.len(), CHUNKS + SMALL);
        assert!(
            client.pending_bytes > CHUNKS * CHUNK,
            "{}",
            client.pending_bytes
        );
        drop(server.await.unwrap());
    }

    #[tokio::test]
    async fn native_session_answers_peer_ping_without_legacy_fallback() {
        let (client_stream, mut server_stream) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut preface = [0; yas_wire::PREFACE.len()];
            server_stream.read_exact(&mut preface).await.unwrap();
            assert_eq!(preface, yas_wire::PREFACE);

            let pre_hello = FrameCodec::pre_hello();
            let hello_frame = read_frame(&mut server_stream, &pre_hello).await.unwrap();
            assert_eq!(
                hello_frame.header,
                FrameHeader::request(
                    family::CORE,
                    yas_wire::core::request_kind::HELLO,
                    HELLO_REQUEST_ID,
                )
            );
            let client_hello = ClientHello::decode(&hello_frame.payload).unwrap();
            assert_eq!(client_hello.client_name, "yas-test");

            let hello = test_server_hello();
            let hello_result = ResultPrefix {
                status: Status::Ok,
                detail: Extensions::default(),
                body: hello.encode().unwrap(),
            };
            let result_frame = Frame {
                header: FrameHeader::result(
                    family::CORE,
                    yas_wire::core::request_kind::HELLO,
                    HELLO_REQUEST_ID,
                ),
                payload: hello_result.encode().unwrap(),
            };
            server_stream
                .write_all(&pre_hello.encode_stream(&result_frame).unwrap())
                .await
                .unwrap();

            let codec = FrameCodec::new(
                FrameLimits {
                    max_wire_frame: client_hello.receive.max_frame,
                    max_decoded_frame: client_hello.receive.max_decoded,
                },
                [],
            )
            .unwrap();
            let ping = Frame {
                header: FrameHeader::request(family::CORE, yas_wire::core::request_kind::PING, 2),
                payload: Ping {
                    sender_monotonic_ns: 9,
                }
                .encode()
                .unwrap(),
            };
            server_stream
                .write_all(&codec.encode_stream(&ping).unwrap())
                .await
                .unwrap();
            let update = SessionUpdate {
                catalog_revision: 2,
                receive: hello.receive,
                extensions: Extensions::default(),
            };
            let update_frame = Frame {
                header: FrameHeader::event(
                    family::CORE,
                    yas_wire::core::event_kind::SESSION_UPDATE,
                ),
                payload: update.encode().unwrap(),
            };
            server_stream
                .write_all(&codec.encode_stream(&update_frame).unwrap())
                .await
                .unwrap();

            let ping_result_frame = read_frame(&mut server_stream, &codec).await.unwrap();
            assert_eq!(
                ping_result_frame.header,
                FrameHeader::result(family::CORE, yas_wire::core::request_kind::PING, 2,)
            );
            let prefix = ResultPrefix::decode(&ping_result_frame.payload).unwrap();
            assert_eq!(prefix.status, Status::Ok);
            let ping_result = PingResult::decode(&prefix.body).unwrap();
            assert!(ping_result.receiver_send_ns >= ping_result.receiver_receive_ns);
        });

        let mut client = NativeClient::connect_transport(
            transport::Transport::Duplex(client_stream),
            &HelloOptions::named("yas-test"),
        )
        .await
        .unwrap();
        let error = client
            .next_matching_event(family::CORE, yas_wire::core::event_kind::SESSION_UPDATE)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("cannot read YAS frame length"),
            "{error}"
        );
        assert_eq!(client.hello().catalog_revision, 2);
        server.await.unwrap();
    }
}
