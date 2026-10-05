//! Flow-controlled Transfer streams.
//!
//! YAS moves bulk bytes (process stdio, file contents, captures) in Transfers
//! announced by a descriptor inside a `Result`. The receiver grants the sender
//! credit (a cumulative byte limit); the sender never exceeds it. These types
//! do the accounting:
//!
//! - [`ByteStream`] receives a server-to-client BYTE Transfer. It grants
//!   credit only as the caller consumes bytes, so a caller that stops reading
//!   stops the server after one window, which in turn stops the producing
//!   process once its pipe fills. It implements [`tokio::io::AsyncRead`].
//! - [`ByteSink`] sends a client-to-server BYTE Transfer, waiting for the
//!   server's credit. It implements [`tokio::io::AsyncWrite`].

use std::collections::BTreeMap;
use std::pin::Pin;
use std::task::{Context, Poll};

use yas_wire::{
    Decode, Frame,
    core::Status,
    family,
    transfer::{
        ByteData, Close, Credit, Delivery, Descriptor, Direction, InlineOrTransfer, MessageData,
        MessageReceiver, Mode, Reset, kind,
    },
};

use crate::client::{Client, FrameReceiver, Route};
use crate::error::{Error, Result};

/// Default receive window for streams this crate opens: 1 MiB.
pub const DEFAULT_WINDOW: u64 = 1024 * 1024;

fn closed_error(client: &Client) -> Error {
    client
        .closed_reason()
        .unwrap_or_else(|| Error::disconnected("YAS session closed during a Transfer"))
}

pub(crate) fn check_sensitivity(descriptor: &Descriptor, frame: &Frame) -> Result<()> {
    let required = descriptor.requires_sensitive_frame(frame.header.kind)?;
    if frame.header.sensitive != required {
        return Err(Error::protocol(format!(
            "YAS Transfer {:#010x} sensitivity flag mismatch",
            descriptor.transfer_id
        )));
    }
    Ok(())
}

enum Step {
    Data(Vec<u8>),
    End,
    Nothing,
}

/// A server-to-client BYTE Transfer (process stdout/stderr, file content).
///
/// Read it with [`ByteStream::next_chunk`], [`ByteStream::read_to_end`], or
/// as an [`AsyncRead`](tokio::io::AsyncRead). Dropping it before the end
/// sends Transfer `RESET` (status `CANCELLED`): the server stops sending, and
/// for process output the bytes are discarded from then on (the process keeps
/// running).
pub struct ByteStream {
    client: Client,
    descriptor: Descriptor,
    frames: FrameReceiver,
    received: u64,
    granted: u64,
    window: u64,
    done: bool,
    failed: Option<Error>,
    buffer: Vec<u8>,
    position: usize,
}

impl std::fmt::Debug for ByteStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByteStream")
            .field("transfer_id", &self.descriptor.transfer_id)
            .field("received", &self.received)
            .field("done", &self.done)
            .finish()
    }
}

impl ByteStream {
    pub(crate) fn new(
        client: Client,
        descriptor: Descriptor,
        frames: FrameReceiver,
        window: u64,
    ) -> Result<Self> {
        descriptor.validate()?;
        if descriptor.mode != Mode::Byte || descriptor.direction != Direction::SENDER_TO_RECEIVER {
            client.release(Route::Transfer(descriptor.transfer_id));
            return Err(Error::protocol(
                "YAS delivery did not provide a server-to-client BYTE Transfer",
            ));
        }
        let granted = descriptor.sender_send_credit;
        let mut stream = Self {
            client,
            window: window.max(granted).max(1),
            granted,
            descriptor,
            frames,
            received: 0,
            done: false,
            failed: None,
            buffer: Vec::new(),
            position: 0,
        };
        // The server leases initial credit from the session's shared budget
        // and may grant less than asked (even nothing, when many streams are
        // open). Ask for the whole window now; it tops the lease up as the
        // budget frees instead of the stream waiting for data that cannot come.
        stream.grant()?;
        Ok(stream)
    }

    /// The Transfer ID.
    pub fn transfer_id(&self) -> u32 {
        self.descriptor.transfer_id
    }

    /// Bytes received so far.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// The next chunk, or `None` once the sender closed the Transfer cleanly.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.position < self.buffer.len() {
            let chunk = self.buffer.split_off(self.position);
            self.buffer.clear();
            self.position = 0;
            return Ok(Some(chunk));
        }
        loop {
            if let Some(error) = &self.failed {
                return Err(error.clone());
            }
            if self.done {
                return Ok(None);
            }
            let Some(frame) = self.frames.recv().await else {
                let error = closed_error(&self.client);
                self.failed = Some(error.clone());
                return Err(error);
            };
            match self.handle(frame) {
                Ok(Step::Data(data)) => return Ok(Some(data)),
                Ok(Step::End) => return Ok(None),
                Ok(Step::Nothing) => {}
                Err(error) => {
                    self.failed = Some(error.clone());
                    return Err(error);
                }
            }
        }
    }

    /// Collect the rest of the stream, failing if it exceeds `limit` bytes.
    pub async fn read_to_end(mut self, limit: u64) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            if bytes.len() as u64 + chunk.len() as u64 > limit {
                return Err(Error::invalid(format!(
                    "YAS Transfer {:#010x} exceeded the {limit}-byte collection limit",
                    self.descriptor.transfer_id
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    /// Collect up to `limit` bytes; stop the Transfer early if there is more.
    /// Returns the bytes and whether the stream was cut short.
    pub async fn read_prefix(mut self, limit: u64) -> Result<(Vec<u8>, bool)> {
        let mut bytes = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            let room = limit.saturating_sub(bytes.len() as u64) as usize;
            if chunk.len() > room {
                bytes.extend_from_slice(&chunk[..room]);
                return Ok((bytes, true));
            }
            bytes.extend_from_slice(&chunk);
            if bytes.len() as u64 == limit && !self.done {
                // Peek: a clean CLOSE right after the limit is not a cut.
                return match self.next_chunk().await? {
                    None => Ok((bytes, false)),
                    Some(_) => Ok((bytes, true)),
                };
            }
        }
        Ok((bytes, false))
    }

    fn handle(&mut self, frame: Frame) -> Result<Step> {
        check_sensitivity(&self.descriptor, &frame)?;
        match frame.header.kind {
            kind::BYTE_DATA => {
                let data = ByteData::decode(&frame.payload)?;
                if self.done
                    || data.offset != self.received
                    || data.data.len() > self.descriptor.max_chunk_bytes as usize
                {
                    return Err(Error::protocol(format!(
                        "YAS Transfer {:#010x} sent a non-contiguous, oversized, or post-CLOSE chunk",
                        self.descriptor.transfer_id
                    )));
                }
                let next = self
                    .received
                    .checked_add(data.data.len() as u64)
                    .ok_or_else(|| Error::protocol("YAS Transfer length overflow"))?;
                if next > self.granted {
                    return Err(Error::protocol(format!(
                        "YAS Transfer {:#010x} exceeded its credit",
                        self.descriptor.transfer_id
                    )));
                }
                self.received = next;
                self.grant()?;
                Ok(Step::Data(data.data))
            }
            kind::CLOSE => {
                let close = Close::decode(&frame.payload)?;
                self.done = true;
                self.client
                    .release(Route::Transfer(self.descriptor.transfer_id));
                if close.final_data_bytes != self.received {
                    return Err(Error::protocol(format!(
                        "YAS Transfer {:#010x} CLOSE length mismatch",
                        self.descriptor.transfer_id
                    )));
                }
                if close.status != Status::Ok.code() {
                    return Err(Error::status_from(
                        format!("YAS Transfer {:#010x}", self.descriptor.transfer_id),
                        Status::from_code(close.status),
                        detail_extensions(&close.detail),
                    ));
                }
                Ok(Step::End)
            }
            kind::RESET => {
                let reset = Reset::decode(&frame.payload)?;
                self.done = true;
                self.client
                    .release(Route::Transfer(self.descriptor.transfer_id));
                Err(Error::status_from(
                    format!("YAS Transfer {:#010x} (reset)", self.descriptor.transfer_id),
                    Status::from_code(reset.status),
                    detail_extensions(&reset.detail),
                ))
            }
            // Credit flows from us; ignore anything else a peer might add.
            _ => Ok(Step::Nothing),
        }
    }

    /// Grant another window once less than half of one is outstanding.
    fn grant(&mut self) -> Result<()> {
        if self.granted - self.received >= self.window / 2 {
            return Ok(());
        }
        let limit = self.received.saturating_add(self.window);
        if limit <= self.granted {
            return Ok(());
        }
        self.granted = limit;
        self.client.send_event(
            family::TRANSFER,
            kind::CREDIT,
            &Credit {
                transfer_id: self.descriptor.transfer_id,
                cumulative_limit: limit,
            },
            false,
        )
    }
}

impl tokio::io::AsyncRead for ByteStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.position < this.buffer.len() {
                let count = out.remaining().min(this.buffer.len() - this.position);
                out.put_slice(&this.buffer[this.position..this.position + count]);
                this.position += count;
                if this.position == this.buffer.len() {
                    this.buffer.clear();
                    this.position = 0;
                }
                return Poll::Ready(Ok(()));
            }
            if let Some(error) = &this.failed {
                return Poll::Ready(Err(std::io::Error::other(error.clone())));
            }
            if this.done {
                return Poll::Ready(Ok(()));
            }
            let frame = match this.frames.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(frame)) => frame,
                Poll::Ready(None) => {
                    let error = closed_error(&this.client);
                    this.failed = Some(error.clone());
                    return Poll::Ready(Err(std::io::Error::other(error)));
                }
            };
            match this.handle(frame) {
                Ok(Step::Data(data)) => {
                    this.buffer = data;
                    this.position = 0;
                }
                Ok(Step::End | Step::Nothing) => {}
                Err(error) => {
                    this.failed = Some(error.clone());
                    return Poll::Ready(Err(std::io::Error::other(error)));
                }
            }
        }
    }
}

impl Drop for ByteStream {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let _ = self.client.send_event(
            family::TRANSFER,
            kind::RESET,
            &Reset {
                transfer_id: self.descriptor.transfer_id,
                status: Status::Cancelled.code(),
                detail: Vec::new(),
            },
            self.descriptor
                .requires_sensitive_frame(kind::RESET)
                .unwrap_or(true),
        );
        self.client
            .release(Route::Transfer(self.descriptor.transfer_id));
    }
}

/// A client-to-server BYTE Transfer (process stdin, staged file uploads).
///
/// Write with [`ByteSink::write_all`] (waits for server credit) or as an
/// [`AsyncWrite`](tokio::io::AsyncWrite); end it with [`ByteSink::finish`]
/// (Transfer `CLOSE`, the reader sees end of file). Dropping an unfinished
/// sink also sends `CLOSE`, like dropping `std::process::ChildStdin`.
pub struct ByteSink {
    client: Client,
    descriptor: Descriptor,
    frames: FrameReceiver,
    sent: u64,
    credit: u64,
    closed: bool,
    failed: Option<Error>,
}

impl std::fmt::Debug for ByteSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByteSink")
            .field("transfer_id", &self.descriptor.transfer_id)
            .field("sent", &self.sent)
            .field("credit", &self.credit)
            .finish()
    }
}

impl ByteSink {
    pub(crate) fn new(
        client: Client,
        descriptor: Descriptor,
        frames: FrameReceiver,
    ) -> Result<Self> {
        descriptor.validate()?;
        if descriptor.mode != Mode::Byte || descriptor.direction != Direction::RECEIVER_TO_SENDER {
            client.release(Route::Transfer(descriptor.transfer_id));
            return Err(Error::protocol(
                "YAS upload did not provide a client-to-server BYTE Transfer",
            ));
        }
        Ok(Self {
            credit: descriptor.receiver_send_credit,
            client,
            descriptor,
            frames,
            sent: 0,
            closed: false,
            failed: None,
        })
    }

    /// The Transfer ID.
    pub fn transfer_id(&self) -> u32 {
        self.descriptor.transfer_id
    }

    /// Bytes sent so far.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// Send every byte of `data`, waiting for credit as needed.
    pub async fn write_all(&mut self, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            self.drain_control()?;
            if self.sent >= self.credit {
                let Some(frame) = self.frames.recv().await else {
                    let error = closed_error(&self.client);
                    self.failed = Some(error.clone());
                    return Err(error);
                };
                self.control(frame)?;
                continue;
            }
            let count = self.send_chunk(data)?;
            data = &data[count..];
        }
        Ok(())
    }

    /// End the Transfer cleanly (`CLOSE`); returns the byte count sent.
    pub async fn finish(mut self) -> Result<u64> {
        self.drain_control()?;
        self.close_now(Status::Ok)?;
        Ok(self.sent)
    }

    /// Abort the Transfer (`RESET`, status `CANCELLED`). For staged file
    /// writes this discards the stage.
    pub fn abort(mut self) {
        self.closed = true;
        let _ = self.client.send_event(
            family::TRANSFER,
            kind::RESET,
            &Reset {
                transfer_id: self.descriptor.transfer_id,
                status: Status::Cancelled.code(),
                detail: Vec::new(),
            },
            self.descriptor
                .requires_sensitive_frame(kind::RESET)
                .unwrap_or(true),
        );
        self.client
            .release(Route::Transfer(self.descriptor.transfer_id));
    }

    fn send_chunk(&mut self, data: &[u8]) -> Result<usize> {
        if self.closed {
            return Err(Error::invalid("YAS upload is already closed"));
        }
        let available = usize::try_from(self.credit - self.sent).unwrap_or(usize::MAX);
        let count = data
            .len()
            .min(self.descriptor.max_chunk_bytes as usize)
            .min(available);
        if count == 0 {
            return Ok(0);
        }
        self.client.send_event(
            family::TRANSFER,
            kind::BYTE_DATA,
            &ByteData {
                transfer_id: self.descriptor.transfer_id,
                offset: self.sent,
                data: data[..count].to_vec(),
            },
            self.descriptor.requires_sensitive_frame(kind::BYTE_DATA)?,
        )?;
        self.sent += count as u64;
        Ok(count)
    }

    fn close_now(&mut self, status: Status) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let result = self.client.send_event(
            family::TRANSFER,
            kind::CLOSE,
            &Close {
                transfer_id: self.descriptor.transfer_id,
                final_data_bytes: self.sent,
                status: status.code(),
                detail: Vec::new(),
            },
            self.descriptor.requires_sensitive_frame(kind::CLOSE)?,
        );
        self.client
            .release(Route::Transfer(self.descriptor.transfer_id));
        result
    }

    fn drain_control(&mut self) -> Result<()> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        while let Ok(frame) = self.frames.try_recv() {
            self.control(frame)?;
        }
        Ok(())
    }

    fn control(&mut self, frame: Frame) -> Result<()> {
        let outcome = self.control_inner(frame);
        if let Err(error) = &outcome {
            self.failed = Some(error.clone());
        }
        outcome
    }

    fn control_inner(&mut self, frame: Frame) -> Result<()> {
        check_sensitivity(&self.descriptor, &frame)?;
        match frame.header.kind {
            kind::CREDIT => {
                let credit = Credit::decode(&frame.payload)?;
                if credit.cumulative_limit > self.credit {
                    self.credit = credit.cumulative_limit;
                }
                Ok(())
            }
            kind::RESET => {
                let reset = Reset::decode(&frame.payload)?;
                self.closed = true;
                self.client
                    .release(Route::Transfer(self.descriptor.transfer_id));
                Err(Error::status_from(
                    format!(
                        "YAS Transfer {:#010x} (reset by the server)",
                        self.descriptor.transfer_id
                    ),
                    Status::from_code(reset.status),
                    detail_extensions(&reset.detail),
                ))
            }
            kind::CLOSE => {
                self.closed = true;
                self.client
                    .release(Route::Transfer(self.descriptor.transfer_id));
                Err(Error::disconnected(format!(
                    "YAS Transfer {:#010x} was closed by the server",
                    self.descriptor.transfer_id
                )))
            }
            _ => Ok(()),
        }
    }
}

impl tokio::io::AsyncWrite for ByteSink {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }
        loop {
            if let Err(error) = this.drain_control() {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    error,
                )));
            }
            if this.closed {
                return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            if this.sent < this.credit {
                return Poll::Ready(this.send_chunk(data).map_err(std::io::Error::other));
            }
            match this.frames.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(frame)) => {
                    if let Err(error) = this.control(frame) {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            error,
                        )));
                    }
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        closed_error(&this.client),
                    )));
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(
            self.get_mut()
                .close_now(Status::Ok)
                .map_err(std::io::Error::other),
        )
    }
}

impl Drop for ByteSink {
    fn drop(&mut self) {
        let _ = self.close_now(Status::Ok);
    }
}

pub(crate) fn detail_extensions(detail: &[u8]) -> yas_wire::Extensions {
    if detail.is_empty() {
        return yas_wire::Extensions::default();
    }
    // Transfer CLOSE/RESET details are free-form bytes; keep them readable.
    yas_wire::Extensions(vec![yas_wire::Extension {
        tag: 0,
        required: false,
        value: detail.to_vec(),
    }])
}

/// The routes a delivery needs registered when its `Result` arrives.
pub(crate) fn delivery_routes(value: &InlineOrTransfer) -> Vec<Route> {
    match &value.delivery {
        Delivery::Transfer(descriptor) => vec![Route::Transfer(descriptor.transfer_id)],
        Delivery::Inline(_) => Vec::new(),
    }
}

/// Collect an inline-or-Transfer delivery whose Transfer route was
/// registered, verifying length and BLAKE3 hash. At most `limit` bytes.
pub(crate) async fn collect_delivery(
    client: &Client,
    value: InlineOrTransfer,
    frames: Option<FrameReceiver>,
    limit: u64,
) -> Result<Vec<u8>> {
    if value.byte_len > limit {
        if let (Delivery::Transfer(descriptor), Some(frames)) = (value.delivery, frames) {
            drop(ByteStream::new(client.clone(), descriptor, frames, 1)?);
        }
        return Err(Error::invalid(format!(
            "YAS delivery is {} bytes; collection limit is {limit}",
            value.byte_len
        )));
    }
    let bytes = match value.delivery {
        Delivery::Inline(bytes) => bytes,
        Delivery::Transfer(descriptor) => {
            let frames = frames.ok_or_else(|| Error::protocol("YAS delivery route missing"))?;
            let window = value.byte_len.max(1);
            ByteStream::new(client.clone(), descriptor, frames, window)?
                .read_to_end(limit)
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

/// Collect a bounded server-to-client MESSAGE Transfer, items ordered by
/// sequence number.
pub(crate) async fn collect_messages(
    client: &Client,
    descriptor: Descriptor,
    mut frames: FrameReceiver,
    maximum_bytes: u64,
    maximum_messages: usize,
) -> Result<Vec<Vec<u8>>> {
    let transfer_id = descriptor.transfer_id;
    let outcome = async {
        descriptor.validate()?;
        if descriptor.mode != Mode::Message || descriptor.direction != Direction::SENDER_TO_RECEIVER
        {
            return Err(Error::protocol(
                "YAS delivery did not provide a server-to-client MESSAGE Transfer",
            ));
        }
        if descriptor.sender_send_credit < maximum_bytes {
            client.send_event(
                family::TRANSFER,
                kind::CREDIT,
                &Credit {
                    transfer_id,
                    cumulative_limit: maximum_bytes,
                },
                false,
            )?;
        }
        let mut validator = MessageReceiver::new(&descriptor)?;
        let mut open = BTreeMap::<u64, Vec<u8>>::new();
        let mut complete = BTreeMap::<u64, Vec<u8>>::new();
        let mut received = 0u64;
        loop {
            let frame = frames.recv().await.ok_or_else(|| closed_error(client))?;
            check_sensitivity(&descriptor, &frame)?;
            match frame.header.kind {
                kind::MESSAGE_DATA => {
                    let fragment = MessageData::decode(&frame.payload)?;
                    let ended = validator.accept(&fragment)?;
                    received = received
                        .checked_add(fragment.data.len() as u64)
                        .ok_or_else(|| Error::protocol("YAS MESSAGE Transfer length overflow"))?;
                    if received > maximum_bytes {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {transfer_id:#010x} exceeded its byte collection limit"
                        )));
                    }
                    if fragment.start {
                        if open.len() + complete.len() >= maximum_messages
                            || complete.contains_key(&fragment.sequence)
                        {
                            return Err(Error::protocol(format!(
                                "YAS Transfer {transfer_id:#010x} exceeded its message collection limit"
                            )));
                        }
                        open.insert(fragment.sequence, Vec::new());
                    }
                    let item = open.get_mut(&fragment.sequence).ok_or_else(|| {
                        Error::protocol(format!(
                            "YAS Transfer {transfer_id:#010x} lost an open message"
                        ))
                    })?;
                    item.extend_from_slice(&fragment.data);
                    if ended {
                        let item = open.remove(&fragment.sequence).unwrap_or_default();
                        complete.insert(fragment.sequence, item);
                    }
                }
                kind::CLOSE => {
                    let close = Close::decode(&frame.payload)?;
                    if close.final_data_bytes != received || validator.open_messages() != 0 {
                        return Err(Error::protocol(format!(
                            "YAS Transfer {transfer_id:#010x} CLOSE accounting mismatch"
                        )));
                    }
                    if close.status != Status::Ok.code() {
                        return Err(Error::status_from(
                            format!("YAS Transfer {transfer_id:#010x}"),
                            Status::from_code(close.status),
                            detail_extensions(&close.detail),
                        ));
                    }
                    return Ok(complete.into_values().collect());
                }
                kind::RESET => {
                    let reset = Reset::decode(&frame.payload)?;
                    return Err(Error::status_from(
                        format!("YAS Transfer {transfer_id:#010x} (reset)"),
                        Status::from_code(reset.status),
                        detail_extensions(&reset.detail),
                    ));
                }
                _ => {}
            }
        }
    }
    .await;
    client.release(Route::Transfer(transfer_id));
    outcome
}
