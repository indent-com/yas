//! The Net family: TCP (and other reliable) streams and UDP datagram flows
//! opened by the server, relayed over the session
//! ([docs/design/net.md](https://github.com/yas-run/yas/blob/main/docs/design/net.md)).
//!
//! The client names a host and port; the server opens the socket (within its
//! `--allow-forward` policy) and the two ends copy bytes. Whatever runs over
//! the stream — HTTP, PostgreSQL, TLS — is the caller's: the server never
//! parses it.
//!
//! ```no_run
//! # async fn demo() -> yas_client::Result<()> {
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//! use yas_client::{Client, ConnectOptions};
//!
//! let client = Client::connect(Some("ssh:build@ci.example"), &ConnectOptions::default()).await?;
//! let net = client.net()?;
//! let mut stream = net.open_tcp("db.internal", 5432).await?;
//! stream.write_all(b"...").await.map_err(|error| yas_client::Error::Invalid(error.to_string()))?;
//! stream.shutdown().await.ok(); // half-close: the peer reads end of file
//! let mut reply = Vec::new();
//! stream.read_to_end(&mut reply).await.ok();
//! # Ok(()) }
//! ```
//!
//! # Streams
//!
//! [`Net::open_tcp`] returns a [`NetStream`]: [`tokio::io::AsyncRead`] +
//! [`tokio::io::AsyncWrite`], over one bidirectional Transfer with credit in
//! each direction (a reader that stops reading stops the server after one
//! window, and through it the peer). `shutdown` half-closes (the peer reads
//! end of file and may keep sending); a read of zero bytes is the peer's
//! half-close. Dropping a stream whose peer already finished half-closes
//! ours; dropping it any earlier, or [`NetStream::abort`], aborts the flow
//! (Net `CLOSE`, the peer sees a reset). Any number of streams and flows
//! share the session, each flow-controlled on its own, up to the server's
//! per-session flow limit ([`Net::limits`]).
//!
//! # Errors
//!
//! A refused open is an [`Error::Status`]; [`Error::net_failure`] says why
//! in the terms a caller acts on: [`NetFailure::Denied`] (the server's
//! target policy), [`NetFailure::NotFound`] (the name did not resolve),
//! [`NetFailure::Refused`] (the target refused or reset the connection),
//! [`NetFailure::Timeout`] (connecting or the TLS handshake took too long,
//! on the server or before the local deadline), [`NetFailure::Exhausted`]
//! (too many flows on the session).

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::sync::Notify;
use yas_wire::core::Status;
use yas_wire::net::{
    self as wire, Address, Datagram, DatagramDelivery, DeliveryPreference, DropPolicy, FlowMode,
    Open, TlsOptions, TlsVerification,
};
use yas_wire::transfer::{ByteData, Close as TransferClose, Credit, Descriptor, Direction, Reset};
use yas_wire::{Class, Decode, Encode, Extensions, Frame, FrameHeader, family};

use crate::client::{Client, Datagrams, FrameReceiver, Hook, Route};
use crate::error::{Error, Result};
use crate::transfer::{check_sensitivity, detail_extensions};

pub use yas_wire::net::{DatagramStats, Endpoint, Limits};

/// Default receive window of a stream: 256 KiB, capped by the server's
/// per-flow buffer limit.
pub const DEFAULT_STREAM_WINDOW: u64 = 256 * 1024;
/// Default local deadline for a Net open. The server bounds its own connect
/// and TLS handshake (10 s each by default) and answers `TIMEOUT`.
pub const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on queued received datagram bytes per flow (oldest dropped past it).
const DATAGRAM_QUEUE_BYTES: usize = 1024 * 1024;

/// Why a Net open failed, from its [`Error`] ([`Error::net_failure`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetFailure {
    /// The server's target policy refused it (`UNAVAILABLE`): not on its
    /// `--allow-forward` list, an `INSECURE` TLS open without
    /// `--allow-forward-insecure`, or Net turned off.
    Denied,
    /// The host name did not resolve (`NOT_FOUND`).
    NotFound,
    /// The target refused, reset or failed the connection (`IO`).
    Refused,
    /// Connecting or the TLS handshake timed out, on the server (`TIMEOUT`)
    /// or before the local deadline.
    Timeout,
    /// Too many flows or opens on this session (`RESOURCE_EXHAUSTED`).
    Exhausted,
    /// The server cannot open this kind of address, or the session does not
    /// offer Net (`UNSUPPORTED`).
    Unsupported,
    /// Anything else: a lost session, a protocol error, another status.
    Other,
}

impl Error {
    /// What this error means for a Net open ([`crate::net`]): why the
    /// server refused it, or [`NetFailure::Other`].
    pub fn net_failure(&self) -> NetFailure {
        match self {
            Self::Status { status, .. } => match status {
                Status::Unavailable => NetFailure::Denied,
                Status::NotFound => NetFailure::NotFound,
                Status::Io => NetFailure::Refused,
                Status::Timeout => NetFailure::Timeout,
                Status::ResourceExhausted => NetFailure::Exhausted,
                Status::Unsupported => NetFailure::Unsupported,
                _ => NetFailure::Other,
            },
            Self::Timeout(_) => NetFailure::Timeout,
            Self::Unsupported(_) => NetFailure::Unsupported,
            _ => NetFailure::Other,
        }
    }
}

/// TLS the server terminates for a TCP stream (the `tls_options` of Net
/// `OPEN`). Leave it out to run TLS yourself, end to end, over a plain
/// stream: the server then never sees the plaintext.
#[derive(Clone, Debug, Default)]
pub struct Tls {
    /// SNI and the name verified; the host opened when `None`.
    pub server_name: Option<String>,
    /// ALPN protocols to offer, best first.
    pub alpn: Vec<Vec<u8>>,
    /// Skip certificate verification; refused unless the server runs with
    /// `--allow-forward-insecure`.
    pub insecure: bool,
}

/// How to open a stream ([`Net::open_tcp_with`], [`Net::open_stream`]).
#[derive(Clone, Debug, Default)]
pub struct StreamOptions {
    /// Server-terminated TLS (TCP only).
    pub tls: Option<Tls>,
    /// Bytes sent to the target as soon as it is connected, saving a round
    /// trip; at most [`Limits::max_early_data_bytes`].
    pub early_data: Vec<u8>,
    /// Receive window; [`DEFAULT_STREAM_WINDOW`] when `None`. Capped by the
    /// server's per-flow buffer limit.
    pub receive_window: Option<u64>,
    /// Local deadline for the open; [`DEFAULT_OPEN_TIMEOUT`] when `None`.
    pub timeout: Option<Duration>,
}

/// The Net family on one [`Client`] session: cheap to clone.
#[derive(Clone)]
pub struct Net {
    client: Client,
    limits: Limits,
    opens: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Net")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// The Net family on this session, to open TCP streams and UDP flows
    /// from the server. Fails with [`Error::Unsupported`] when the session
    /// does not offer Net with Transfer streams (a server run with
    /// `YAS_NET=0`, an older server, a read-only session).
    pub fn net(&self) -> Result<Net> {
        for (family_id, class, kind) in [
            (family::NET, Class::Request, wire::request_kind::OPEN),
            (family::NET, Class::Request, wire::request_kind::CLOSE),
            (
                family::TRANSFER,
                Class::Event,
                yas_wire::transfer::kind::BYTE_DATA,
            ),
            (
                family::TRANSFER,
                Class::Event,
                yas_wire::transfer::kind::CREDIT,
            ),
            (
                family::TRANSFER,
                Class::Event,
                yas_wire::transfer::kind::CLOSE,
            ),
            (
                family::TRANSFER,
                Class::Event,
                yas_wire::transfer::kind::RESET,
            ),
        ] {
            if !self.supports(family_id, class, kind) {
                return Err(Error::Unsupported(
                    "this YAS session does not offer the Net family with Transfer streams".into(),
                ));
            }
        }
        let hello = self.hello();
        let descriptor = hello
            .families
            .iter()
            .find(|descriptor| descriptor.family_id == family::NET)
            .ok_or_else(|| Error::protocol("YAS server omitted its Net descriptor"))?;
        let limits = Limits::from_extensions(&descriptor.limits)
            .map_err(|error| Error::protocol(format!("invalid Net family limits: {error}")))?;
        let opens = self.net_opens(limits.max_pending_opens as usize);
        Ok(Net {
            client: self.clone(),
            limits,
            opens,
        })
    }
}

impl Net {
    /// The session this runs on.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The server's Net limits (flows per session, early data, datagram
    /// sizes, buffers).
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The largest UDP payload a [`DatagramFlow`] may carry.
    pub fn max_datagram_payload(&self) -> usize {
        self.limits.max_datagram_payload as usize
    }

    /// Open a TCP stream to `host:port` from the server.
    pub async fn open_tcp(&self, host: &str, port: u16) -> Result<NetStream> {
        self.open_tcp_with(host, port, &StreamOptions::default())
            .await
    }

    /// Open a TCP stream with TLS, early data, a window or a deadline.
    pub async fn open_tcp_with(
        &self,
        host: &str,
        port: u16,
        options: &StreamOptions,
    ) -> Result<NetStream> {
        self.open_stream(
            Address::Tcp {
                host: host.to_owned(),
                port,
            },
            options,
        )
        .await
    }

    /// Open a reliable byte stream to any address the server opens as one:
    /// TCP, a Unix stream socket, a Windows byte pipe.
    pub async fn open_stream(
        &self,
        address: Address,
        options: &StreamOptions,
    ) -> Result<NetStream> {
        if address.is_datagram() {
            return Err(Error::invalid("a datagram address opens a DatagramFlow"));
        }
        if options.tls.is_some() && !address.is_tcp() {
            return Err(Error::invalid("Net TLS is for TCP streams only"));
        }
        if options.early_data.len() > self.limits.max_early_data_bytes as usize {
            return Err(Error::invalid(format!(
                "Net early data is {} bytes; the server takes at most {}",
                options.early_data.len(),
                self.limits.max_early_data_bytes
            )));
        }
        let window = options
            .receive_window
            .unwrap_or(DEFAULT_STREAM_WINDOW)
            .clamp(1, self.limits.max_buffered_per_flow.max(1));
        let target = describe(&address);
        let request = Open {
            operation_id: nonzero_operation_id(),
            address,
            delivery_preference: DeliveryPreference::NotApplicable,
            drop_policy: DropPolicy::NotApplicable,
            initial_receive_credit: window,
            early_data: options.early_data.clone(),
            tls_options: options.tls.as_ref().map(|tls| TlsOptions {
                verification: if tls.insecure {
                    TlsVerification::Insecure
                } else {
                    TlsVerification::Strict
                },
                sni: tls.server_name.clone().unwrap_or_default(),
                alpn: tls.alpn.clone(),
                extensions: Extensions::default(),
            }),
            extensions: Extensions::default(),
        };
        let hook: Hook = Box::new(|prefix| match Endpoint::decode(&prefix.body) {
            Ok(Endpoint {
                mode: FlowMode::Byte,
                descriptor: Some(descriptor),
                ..
            }) => vec![Route::Transfer(descriptor.transfer_id)],
            _ => Vec::new(),
        });
        let (endpoint, frames) = {
            let _permit = self.acquire_open().await?;
            let mut reply = self
                .open(&request, &target, options.timeout, Some(hook))
                .await?;
            let endpoint = self.decode_endpoint(&reply.prefix.body, &target)?;
            let frames = endpoint
                .descriptor
                .as_ref()
                .and_then(|descriptor| reply.take(Route::Transfer(descriptor.transfer_id)));
            (endpoint, frames)
        };
        let (descriptor, frames) = match (endpoint.mode, endpoint.descriptor.clone(), frames) {
            (FlowMode::Byte, Some(descriptor), Some(frames)) => (descriptor, frames),
            _ => {
                self.abort_flow(endpoint.flow_handle);
                return Err(Error::protocol(format!(
                    "YAS Net OPEN {target} did not return a byte stream"
                )));
            }
        };
        if let Err(error) = self.check_stream_descriptor(&descriptor, window) {
            self.client.release(Route::Transfer(descriptor.transfer_id));
            self.abort_flow(endpoint.flow_handle);
            return Err(error);
        }
        NetStream::new(
            self.client.clone(),
            endpoint,
            descriptor,
            frames,
            window,
            self.limits.max_buffered_per_flow,
        )
    }

    /// Open a UDP flow to `host:port` from the server: one connected socket
    /// there, datagrams relayed whole (never split, merged or retransmitted).
    /// Over a transport with a datagram sideband (WebRTC, WebTransport) they
    /// travel unreliably, as UDP does; otherwise in order over the session.
    pub async fn open_udp(&self, host: &str, port: u16) -> Result<DatagramFlow> {
        self.open_datagram(Address::Udp {
            host: host.to_owned(),
            port,
        })
        .await
    }

    /// Open a datagram flow to a UDP or Unix datagram address.
    pub async fn open_datagram(&self, address: Address) -> Result<DatagramFlow> {
        if !address.is_datagram() {
            return Err(Error::invalid("a stream address opens a NetStream"));
        }
        let stats_sent = self
            .client
            .hello()
            .families
            .iter()
            .find(|descriptor| descriptor.family_id == family::NET)
            .and_then(|descriptor| {
                descriptor.operation(Class::Event, wire::event_kind::DATAGRAM_STATS)
            })
            .is_some_and(|operation| operation.server_sends);
        if !stats_sent
            || !self
                .client
                .supports(family::NET, Class::Event, wire::event_kind::DATAGRAM)
        {
            return Err(Error::Unsupported(
                "this YAS session does not offer Net datagrams".into(),
            ));
        }
        let native = self.client.supports_datagrams();
        let target = describe(&address);
        let request = Open {
            operation_id: nonzero_operation_id(),
            address,
            delivery_preference: if native {
                DeliveryPreference::PreferNative
            } else {
                DeliveryPreference::ReliableTunnel
            },
            drop_policy: DropPolicy::Oldest,
            initial_receive_credit: 0,
            early_data: Vec::new(),
            tls_options: None,
            extensions: Extensions::default(),
        };
        // Register the flow before the reader takes the next frame, so its
        // first datagrams are never dropped as strangers'.
        let registry = self.client.datagrams();
        let limits = self.limits;
        let hook_registry = registry.clone();
        let hook: Hook = Box::new(move |prefix| {
            if let Ok(endpoint) = Endpoint::decode(&prefix.body)
                && endpoint.mode == FlowMode::Datagram
                && endpoint.max_datagram_payload <= limits.max_datagram_payload
                && (endpoint.selected_delivery != DatagramDelivery::Native || native)
            {
                hook_registry.insert(
                    endpoint.flow_handle,
                    Arc::new(DatagramState::new(
                        endpoint.selected_delivery,
                        endpoint.max_datagram_payload as usize,
                        limits.max_datagram_queue as usize,
                    )),
                );
            }
            Vec::new()
        });
        let endpoint = {
            let _permit = self.acquire_open().await?;
            let reply = self.open(&request, &target, None, Some(hook)).await?;
            self.decode_endpoint(&reply.prefix.body, &target)?
        };
        let Some(state) = (endpoint.mode == FlowMode::Datagram)
            .then(|| registry.get(endpoint.flow_handle))
            .flatten()
        else {
            self.abort_flow(endpoint.flow_handle);
            return Err(Error::protocol(format!(
                "YAS Net OPEN {target} did not return a usable datagram flow"
            )));
        };
        Ok(DatagramFlow {
            inner: Arc::new(FlowInner {
                client: self.client.clone(),
                registry,
                endpoint,
                state,
                send_sequence: AtomicU64::new(0),
                closed: AtomicBool::new(false),
            }),
        })
    }

    async fn acquire_open(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.opens)
            .acquire_owned()
            .await
            .map_err(|_| Error::Closed)
    }

    async fn open(
        &self,
        request: &Open,
        target: &str,
        timeout: Option<Duration>,
        hook: Option<Hook>,
    ) -> Result<crate::client::Reply> {
        let reply = self
            .client
            .call(
                family::NET,
                wire::request_kind::OPEN,
                request.encode()?,
                Some(timeout.unwrap_or(DEFAULT_OPEN_TIMEOUT)),
                hook,
            )
            .await
            .map_err(|error| match error {
                Error::Timeout(_) => Error::Timeout(format!("YAS Net OPEN {target} timed out")),
                other => other,
            })?;
        if reply.prefix.status != Status::Ok {
            return Err(Error::status_from(
                format!("YAS Net OPEN {target}"),
                reply.prefix.status,
                reply.prefix.detail,
            ));
        }
        Ok(reply)
    }

    fn decode_endpoint(&self, body: &[u8], target: &str) -> Result<Endpoint> {
        Endpoint::decode(body)
            .map_err(|error| Error::protocol(format!("invalid Net endpoint for {target}: {error}")))
    }

    fn check_stream_descriptor(&self, descriptor: &Descriptor, window: u64) -> Result<()> {
        descriptor.validate()?;
        if descriptor.mode != yas_wire::transfer::Mode::Byte
            || descriptor.direction != Direction::BIDIRECTIONAL
            || descriptor.max_chunk_bytes == 0
        {
            return Err(Error::protocol(
                "YAS Net stream is not a bidirectional BYTE Transfer",
            ));
        }
        if descriptor.sender_send_credit > window
            || descriptor.receiver_send_credit > self.limits.max_buffered_per_flow
        {
            return Err(Error::protocol(
                "YAS Net Transfer exceeded the negotiated credit limits",
            ));
        }
        Ok(())
    }

    fn abort_flow(&self, flow_handle: u64) {
        abort_flow(&self.client, flow_handle);
    }
}

fn abort_flow(client: &Client, flow_handle: u64) {
    if let Ok(payload) = (wire::Close {
        flow_handle,
        operation_id: nonzero_operation_id(),
        extensions: Extensions::default(),
    })
    .encode()
    {
        client.send_request_detached(family::NET, wire::request_kind::CLOSE, payload);
    }
}

fn describe(address: &Address) -> String {
    match address {
        Address::Tcp { host, port } => format!("tcp {}:{port}", bracket(host)),
        Address::Udp { host, port } => format!("udp {}:{port}", bracket(host)),
        Address::UnixStream(_) => "unix stream".into(),
        Address::UnixDatagram(_) => "unix datagram".into(),
        Address::UnixSeqpacket(_) => "unix seqpacket".into(),
        Address::WindowsPipe { name, .. } => format!("pipe {name}"),
    }
}

/// `[host]` for IPv6 literals, as in `host:port`.
pub fn bracket(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

fn nonzero_operation_id() -> [u8; 16] {
    loop {
        let value: [u8; 16] = rand::random();
        if value != [0; 16] {
            return value;
        }
    }
}

fn io_error(error: &Error) -> std::io::Error {
    let kind = match error {
        Error::Status {
            status: Status::Cancelled,
            ..
        } => std::io::ErrorKind::ConnectionAborted,
        Error::Status { .. } => std::io::ErrorKind::ConnectionReset,
        Error::Disconnected(_) | Error::GoAway { .. } | Error::Closed => {
            std::io::ErrorKind::ConnectionAborted
        }
        Error::Timeout(_) => std::io::ErrorKind::TimedOut,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, error.clone())
}

/// A TCP (or other reliable) stream the server opened: bytes both ways,
/// half-close, credit in each direction. See the [module docs](self).
pub struct NetStream {
    client: Client,
    endpoint: Endpoint,
    descriptor: Descriptor,
    frames: FrameReceiver,
    // Peer to us.
    chunks: VecDeque<Vec<u8>>,
    position: usize,
    received: u64,
    consumed: u64,
    granted: u64,
    window: u64,
    read_done: bool,
    // Us to peer.
    sent: u64,
    credit: u64,
    max_credit_ahead: u64,
    write_closed: bool,
    failed: Option<Error>,
    released: bool,
    /// The reading and the writing task (one, or two after `tokio::io::split`).
    wakers: Arc<Wakers>,
    /// Polls the frame receiver on behalf of both: whichever side polled last,
    /// a frame arriving wakes both, so neither waits on the other's waker.
    fanout: Waker,
}

#[derive(Default)]
struct Wakers {
    read: std::sync::Mutex<Option<Waker>>,
    write: std::sync::Mutex<Option<Waker>>,
}

impl Wakers {
    fn set(slot: &std::sync::Mutex<Option<Waker>>, waker: &Waker) {
        let mut slot = slot.lock().unwrap();
        if !slot
            .as_ref()
            .is_some_and(|current| current.will_wake(waker))
        {
            *slot = Some(waker.clone());
        }
    }

    fn wake(slot: &std::sync::Mutex<Option<Waker>>) {
        let waker = slot.lock().unwrap().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl std::task::Wake for Wakers {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        Self::wake(&self.read);
        Self::wake(&self.write);
    }
}

impl std::fmt::Debug for NetStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetStream")
            .field("flow_handle", &self.endpoint.flow_handle)
            .field("peer", &self.endpoint.peer_address)
            .field("received", &self.received)
            .field("sent", &self.sent)
            .field("read_done", &self.read_done)
            .field("write_closed", &self.write_closed)
            .finish_non_exhaustive()
    }
}

impl NetStream {
    fn new(
        client: Client,
        endpoint: Endpoint,
        descriptor: Descriptor,
        frames: FrameReceiver,
        window: u64,
        max_credit_ahead: u64,
    ) -> Result<Self> {
        let wakers = Arc::new(Wakers::default());
        let fanout = Waker::from(Arc::clone(&wakers));
        let mut stream = Self {
            client,
            wakers,
            fanout,
            granted: descriptor.sender_send_credit,
            credit: descriptor.receiver_send_credit,
            max_credit_ahead: max_credit_ahead.max(descriptor.receiver_send_credit),
            endpoint,
            descriptor,
            frames,
            chunks: VecDeque::new(),
            position: 0,
            received: 0,
            consumed: 0,
            window,
            read_done: false,
            sent: 0,
            write_closed: false,
            failed: None,
            released: false,
        };
        // The server may lease less initial credit than asked (its session
        // budget is shared): ask for the whole window now.
        if let Err(error) = stream.grant() {
            stream.abort_now();
            return Err(error);
        }
        Ok(stream)
    }

    /// What the server reported when it opened the stream.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The flow's handle on the server.
    pub fn flow_handle(&self) -> u64 {
        self.endpoint.flow_handle
    }

    /// The address the server connected to (the resolved one for a name).
    pub fn peer_address(&self) -> &Address {
        &self.endpoint.peer_address
    }

    /// The server's own end of the connection, when it has one.
    pub fn local_address(&self) -> Option<&Address> {
        self.endpoint.local_address.as_ref()
    }

    /// The ALPN protocol server-terminated TLS negotiated (empty without).
    pub fn negotiated_alpn(&self) -> &[u8] {
        &self.endpoint.negotiated_alpn
    }

    /// Bytes received from the peer so far.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// Bytes sent to the peer so far.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// Abort the flow now: Net `CLOSE`, the peer's connection is reset and
    /// unread bytes are dropped.
    pub fn abort(mut self) {
        self.abort_now();
    }

    fn abort_now(&mut self) {
        if !self.released {
            if self.failed.is_none() && self.client.closed_reason().is_none() {
                abort_flow(&self.client, self.endpoint.flow_handle);
            }
            self.release();
        }
        if self.failed.is_none() {
            self.failed = Some(Error::Closed);
        }
    }

    fn release(&mut self) {
        if !self.released {
            self.released = true;
            self.client
                .release(Route::Transfer(self.descriptor.transfer_id));
        }
    }

    fn fail(&mut self, error: Error) {
        if self.failed.is_none() {
            self.failed = Some(error);
        }
        self.wake_both();
    }

    fn wake_both(&mut self) {
        Wakers::wake(&self.wakers.read);
        Wakers::wake(&self.wakers.write);
    }

    /// Take every frame that arrived, registering both sides for the next.
    fn pump(&mut self) {
        let fanout = self.fanout.clone();
        let mut cx = Context::from_waker(&fanout);
        while self.failed.is_none() {
            match self.frames.poll_recv(&mut cx) {
                Poll::Ready(Some(frame)) => {
                    if let Err(error) = self.handle(frame) {
                        self.fail(error);
                    }
                }
                Poll::Ready(None) => {
                    if self.read_done && self.write_closed {
                        return;
                    }
                    let error = self.client.closed_reason().unwrap_or_else(|| {
                        Error::Disconnected("YAS session closed during a Net stream".into())
                    });
                    self.fail(error);
                }
                Poll::Pending => return,
            }
        }
    }

    fn handle(&mut self, frame: Frame) -> Result<()> {
        check_sensitivity(&self.descriptor, &frame)?;
        let transfer_id = self.descriptor.transfer_id;
        match frame.header.kind {
            yas_wire::transfer::kind::BYTE_DATA => {
                let data = ByteData::decode(&frame.payload)?;
                let end = data
                    .offset
                    .checked_add(data.data.len() as u64)
                    .ok_or_else(|| Error::protocol("YAS Net Transfer length overflow"))?;
                if self.read_done
                    || data.offset != self.received
                    || data.data.len() > self.descriptor.max_chunk_bytes as usize
                    || end > self.granted
                {
                    return Err(Error::protocol(format!(
                        "YAS Net Transfer {transfer_id:#010x} sent a non-contiguous, oversized, \
                         over-credit or post-CLOSE chunk"
                    )));
                }
                self.received = end;
                if !data.data.is_empty() {
                    self.chunks.push_back(data.data);
                }
                Wakers::wake(&self.wakers.read);
            }
            yas_wire::transfer::kind::CREDIT => {
                let credit = Credit::decode(&frame.payload)?;
                if credit.cumulative_limit < self.credit {
                    return Err(Error::protocol("YAS Net Transfer credit moved backwards"));
                }
                if credit.cumulative_limit > self.sent.saturating_add(self.max_credit_ahead) {
                    return Err(Error::protocol(
                        "YAS Net peer granted more credit than its bounded window",
                    ));
                }
                self.credit = credit.cumulative_limit;
                Wakers::wake(&self.wakers.write);
            }
            yas_wire::transfer::kind::CLOSE => {
                let close = TransferClose::decode(&frame.payload)?;
                if self.read_done || close.final_data_bytes != self.received {
                    return Err(Error::protocol(format!(
                        "YAS Net Transfer {transfer_id:#010x} CLOSE length mismatch"
                    )));
                }
                if close.status != Status::Ok.code() {
                    return Err(Error::status_from(
                        format!("YAS Net stream {transfer_id:#010x}"),
                        Status::from_code(close.status),
                        detail_extensions(&close.detail),
                    ));
                }
                self.read_done = true;
                if self.write_closed {
                    self.release();
                }
                Wakers::wake(&self.wakers.read);
            }
            yas_wire::transfer::kind::RESET => {
                let reset = Reset::decode(&frame.payload)?;
                self.release();
                return Err(Error::status_from(
                    format!("YAS Net stream {transfer_id:#010x} (reset)"),
                    Status::from_code(reset.status),
                    detail_extensions(&reset.detail),
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// Grant another window once less than half of one is outstanding.
    fn grant(&mut self) -> Result<()> {
        if self.read_done || self.granted - self.consumed >= self.window / 2 {
            return Ok(());
        }
        let limit = self.consumed.saturating_add(self.window);
        if limit <= self.granted {
            return Ok(());
        }
        self.granted = limit;
        self.client.send_event(
            family::TRANSFER,
            yas_wire::transfer::kind::CREDIT,
            &Credit {
                transfer_id: self.descriptor.transfer_id,
                cumulative_limit: limit,
            },
            self.descriptor
                .requires_sensitive_frame(yas_wire::transfer::kind::CREDIT)
                .unwrap_or(false),
        )
    }

    fn send_close(&mut self) -> Result<()> {
        self.write_closed = true;
        let result = self.client.send_event(
            family::TRANSFER,
            yas_wire::transfer::kind::CLOSE,
            &TransferClose {
                transfer_id: self.descriptor.transfer_id,
                final_data_bytes: self.sent,
                status: Status::Ok.code(),
                detail: Vec::new(),
            },
            self.descriptor
                .requires_sensitive_frame(yas_wire::transfer::kind::CLOSE)
                .unwrap_or(true),
        );
        if self.read_done {
            self.release();
        }
        result
    }
}

impl tokio::io::AsyncRead for NetStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            if let Some(chunk) = this.chunks.front() {
                let count = out.remaining().min(chunk.len() - this.position);
                out.put_slice(&chunk[this.position..this.position + count]);
                this.position += count;
                if this.position == chunk.len() {
                    this.chunks.pop_front();
                    this.position = 0;
                }
                this.consumed += count as u64;
                if let Err(error) = this.grant() {
                    this.fail(error);
                }
                return Poll::Ready(Ok(()));
            }
            if let Some(error) = &this.failed {
                return Poll::Ready(Err(io_error(error)));
            }
            if this.read_done {
                return Poll::Ready(Ok(()));
            }
            // Register before pumping: a frame arriving in between wakes us.
            Wakers::set(&this.wakers.read, cx.waker());
            let before = (this.chunks.len(), this.read_done);
            this.pump();
            if (this.chunks.len(), this.read_done) == before && this.failed.is_none() {
                return Poll::Pending;
            }
        }
    }
}

impl tokio::io::AsyncWrite for NetStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        loop {
            if let Some(error) = &this.failed {
                return Poll::Ready(Err(io_error(error)));
            }
            if this.write_closed {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "YAS Net stream already shut down for writing",
                )));
            }
            let room = this.credit.saturating_sub(this.sent);
            if room > 0 {
                let count = usize::try_from(room)
                    .unwrap_or(usize::MAX)
                    .min(buf.len())
                    .min(this.descriptor.max_chunk_bytes as usize);
                let sent = this.client.send_event(
                    family::TRANSFER,
                    yas_wire::transfer::kind::BYTE_DATA,
                    &ByteData {
                        transfer_id: this.descriptor.transfer_id,
                        offset: this.sent,
                        data: buf[..count].to_vec(),
                    },
                    this.descriptor
                        .requires_sensitive_frame(yas_wire::transfer::kind::BYTE_DATA)
                        .unwrap_or(true),
                );
                if let Err(error) = sent {
                    this.fail(error);
                    continue;
                }
                this.sent += count as u64;
                return Poll::Ready(Ok(count));
            }
            Wakers::set(&this.wakers.write, cx.waker());
            let before = this.credit;
            this.pump();
            if this.credit == before && this.failed.is_none() {
                return Poll::Pending;
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Written bytes are queued to the session's writer in order.
        let this = self.get_mut();
        match &this.failed {
            Some(error) => Poll::Ready(Err(io_error(error))),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.write_closed {
            return Poll::Ready(Ok(()));
        }
        if let Some(error) = &this.failed {
            return Poll::Ready(Err(io_error(error)));
        }
        match this.send_close() {
            Ok(()) => Poll::Ready(Ok(())),
            Err(error) => {
                let io = io_error(&error);
                this.fail(error);
                Poll::Ready(Err(io))
            }
        }
    }
}

impl Drop for NetStream {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if self.read_done && self.failed.is_none() {
            // The peer finished: finish ours as TcpStream's drop would.
            if !self.write_closed {
                let _ = self.send_close();
            }
            self.release();
        } else {
            self.abort_now();
        }
    }
}

/// One datagram flow's receive side, fed by the session reader.
pub(crate) struct DatagramState {
    delivery: DatagramDelivery,
    max_payload: usize,
    max_queue: usize,
    receive: std::sync::Mutex<DatagramReceive>,
    notify: Notify,
}

struct DatagramReceive {
    queue: VecDeque<Vec<u8>>,
    queued_bytes: usize,
    last_sequence: Option<u64>,
    stats: Option<DatagramStats>,
    error: Option<Error>,
}

impl DatagramState {
    fn new(delivery: DatagramDelivery, max_payload: usize, max_queue: usize) -> Self {
        Self {
            delivery,
            max_payload,
            max_queue: max_queue.max(1),
            receive: std::sync::Mutex::new(DatagramReceive {
                queue: VecDeque::new(),
                queued_bytes: 0,
                last_sequence: None,
                stats: None,
                error: None,
            }),
            notify: Notify::new(),
        }
    }

    fn push(&self, datagram: Datagram) -> Result<()> {
        if datagram.payload.len() > self.max_payload {
            return Err(Error::protocol(
                "YAS Net datagram exceeds its flow's maximum",
            ));
        }
        let mut receive = self.receive.lock().unwrap();
        if self.delivery == DatagramDelivery::ReliableTunnel {
            if receive
                .last_sequence
                .is_some_and(|sequence| datagram.sequence <= sequence)
            {
                return Err(Error::protocol(
                    "YAS Net datagram sequence did not increase",
                ));
            }
            receive.last_sequence = Some(datagram.sequence);
        }
        while receive.queue.len() >= self.max_queue
            || receive.queued_bytes + datagram.payload.len() > DATAGRAM_QUEUE_BYTES
        {
            let Some(dropped) = receive.queue.pop_front() else {
                break;
            };
            receive.queued_bytes = receive.queued_bytes.saturating_sub(dropped.len());
        }
        if datagram.payload.len() <= DATAGRAM_QUEUE_BYTES {
            receive.queued_bytes += datagram.payload.len();
            receive.queue.push_back(datagram.payload);
        }
        drop(receive);
        self.notify.notify_one();
        Ok(())
    }

    fn stats(&self, stats: DatagramStats) {
        let mut receive = self.receive.lock().unwrap();
        if receive
            .stats
            .as_ref()
            .is_none_or(|previous| stats.revision > previous.revision)
        {
            receive.stats = Some(stats);
        }
        drop(receive);
        self.notify.notify_waiters();
    }

    pub(crate) fn fail(&self, error: Error) {
        let mut receive = self.receive.lock().unwrap();
        if receive.error.is_none() {
            receive.error = Some(error);
        }
        drop(receive);
        self.notify.notify_waiters();
    }
}

/// Route one Net Event from the session reader.
pub(crate) fn dispatch_event(
    flows: &std::sync::Mutex<std::collections::HashMap<u64, Arc<DatagramState>>>,
    frame: Frame,
    transport_datagram: bool,
) -> Result<()> {
    // Lossy sideband packets that are malformed, stale or misplaced are
    // dropped: they must never end the reliable session.
    let lossy = transport_datagram;
    if !frame.header.sensitive {
        return if lossy {
            Ok(())
        } else {
            Err(Error::protocol("YAS Net event omitted the SENSITIVE flag"))
        };
    }
    match frame.header.kind {
        wire::event_kind::DATAGRAM => {
            let datagram = match Datagram::decode(&frame.payload) {
                Ok(datagram) => datagram,
                Err(_) if lossy => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            let flow = flows.lock().unwrap().get(&datagram.flow_handle).cloned();
            let Some(flow) = flow else {
                return Ok(());
            };
            // A native flow also takes reliable-stream datagrams (the
            // fallback once its sideband closed); a tunnelled one never
            // takes sideband packets.
            if lossy && flow.delivery != DatagramDelivery::Native {
                return Ok(());
            }
            match flow.push(datagram) {
                Err(_) if lossy => Ok(()),
                other => other,
            }
        }
        wire::event_kind::DATAGRAM_STATS => {
            if lossy {
                return Ok(());
            }
            let stats = DatagramStats::decode(&frame.payload)?;
            let flow = flows.lock().unwrap().get(&stats.flow_handle).cloned();
            if let Some(flow) = flow {
                flow.stats(stats);
            }
            Ok(())
        }
        _ if lossy => Ok(()),
        other => Err(Error::protocol(format!(
            "unexpected YAS Net event {other:#06x}"
        ))),
    }
}

/// A UDP (or Unix datagram) flow the server opened. Cheap to clone; the
/// last clone dropped closes it.
#[derive(Clone)]
pub struct DatagramFlow {
    inner: Arc<FlowInner>,
}

struct FlowInner {
    client: Client,
    registry: Datagrams,
    endpoint: Endpoint,
    state: Arc<DatagramState>,
    send_sequence: AtomicU64,
    closed: AtomicBool,
}

impl Drop for FlowInner {
    fn drop(&mut self) {
        self.registry.remove(self.endpoint.flow_handle);
        if !self.closed.swap(true, Ordering::AcqRel) && !self.finished() {
            abort_flow(&self.client, self.endpoint.flow_handle);
        }
    }
}

impl FlowInner {
    fn finished(&self) -> bool {
        let receive = self.state.receive.lock().unwrap();
        receive.error.is_some()
            || receive
                .stats
                .as_ref()
                .is_some_and(|stats| stats.final_stats)
    }
}

impl std::fmt::Debug for DatagramFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatagramFlow")
            .field("flow_handle", &self.inner.endpoint.flow_handle)
            .field("peer", &self.inner.endpoint.peer_address)
            .field("delivery", &self.inner.state.delivery)
            .finish_non_exhaustive()
    }
}

impl DatagramFlow {
    /// What the server reported when it opened the flow.
    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    /// How datagrams travel: natively (lossy sideband) or tunnelled in
    /// order over the session.
    pub fn delivery(&self) -> DatagramDelivery {
        self.inner.state.delivery
    }

    /// The largest payload this flow carries.
    pub fn max_payload(&self) -> usize {
        self.inner.state.max_payload
    }

    /// Send one datagram. Congestion drops it, as UDP would, without an
    /// error; an oversized one is an [`Error::Invalid`].
    pub fn send(&self, payload: &[u8]) -> Result<()> {
        if payload.len() > self.inner.state.max_payload {
            return Err(Error::invalid(format!(
                "UDP datagram is {} bytes; the flow carries at most {}",
                payload.len(),
                self.inner.state.max_payload
            )));
        }
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let sequence = self.inner.send_sequence.fetch_add(1, Ordering::Relaxed);
        let mut header = FrameHeader::event(family::NET, wire::event_kind::DATAGRAM);
        header.sensitive = true;
        let frame = Frame {
            header,
            payload: Datagram {
                flow_handle: self.inner.endpoint.flow_handle,
                sequence,
                payload: payload.to_vec(),
            }
            .encode()?,
        };
        if self.inner.state.delivery == DatagramDelivery::Native {
            match self
                .inner
                .client
                .try_send_datagram(&frame, yas_wire::frame::DatagramContext::NetNativeFlow)?
            {
                crate::transport::DatagramSend::Sent | crate::transport::DatagramSend::Dropped => {
                    return Ok(());
                }
                // The sideband closed: keep the boundary in one reliable Event.
                crate::transport::DatagramSend::Closed => {}
            }
        }
        self.inner.client.send_frame(frame)
    }

    /// The next datagram from the peer; `None` once the server closed the
    /// flow (its idle timeout, a [`DatagramFlow::close`]).
    pub async fn recv(&self) -> Result<Option<Vec<u8>>> {
        let state = &self.inner.state;
        loop {
            let notified = state.notify.notified();
            {
                let mut receive = state.receive.lock().unwrap();
                if let Some(payload) = receive.queue.pop_front() {
                    receive.queued_bytes = receive.queued_bytes.saturating_sub(payload.len());
                    return Ok(Some(payload));
                }
                if let Some(error) = &receive.error {
                    return Err(error.clone());
                }
                if receive
                    .stats
                    .as_ref()
                    .is_some_and(|stats| stats.final_stats)
                {
                    return Ok(None);
                }
            }
            notified.await;
        }
    }

    /// Whether the flow has ended (closed by the server, or the session lost).
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire) || self.inner.finished()
    }

    /// The latest counters the server reported (delivered, dropped).
    pub fn stats(&self) -> Option<DatagramStats> {
        self.inner.state.receive.lock().unwrap().stats.clone()
    }

    /// The final counters, once the flow has ended.
    pub fn final_stats(&self) -> Option<DatagramStats> {
        self.stats().filter(|stats| stats.final_stats)
    }

    /// Close the flow (Net `CLOSE`) without waiting for the answer;
    /// [`DatagramFlow::recv`] then ends once the server confirms.
    pub fn close(&self) {
        if !self.inner.closed.swap(true, Ordering::AcqRel) && !self.inner.finished() {
            abort_flow(&self.inner.client, self.inner.endpoint.flow_handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(sequence: u64) -> Datagram {
        Datagram {
            flow_handle: 1,
            sequence,
            payload: vec![sequence as u8],
        }
    }

    #[test]
    fn host_display_brackets_only_ipv6() {
        assert_eq!(bracket("127.0.0.1"), "127.0.0.1");
        assert_eq!(bracket("example.test"), "example.test");
        assert_eq!(bracket("::1"), "[::1]");
    }

    #[test]
    fn datagram_queue_drops_oldest_under_its_bound() {
        let state = DatagramState::new(DatagramDelivery::ReliableTunnel, 1500, 2);
        for sequence in 0..3 {
            state.push(datagram(sequence)).unwrap();
        }
        let receive = state.receive.lock().unwrap();
        assert_eq!(receive.queue.len(), 2);
        assert_eq!(receive.queue.front().unwrap(), &[1]);
    }

    #[test]
    fn tunnelled_datagrams_must_increase_and_native_ones_need_not() {
        let tunnelled = DatagramState::new(DatagramDelivery::ReliableTunnel, 1500, 8);
        tunnelled.push(datagram(4)).unwrap();
        assert!(tunnelled.push(datagram(4)).is_err());

        let native = DatagramState::new(DatagramDelivery::Native, 1500, 8);
        for sequence in [4, 2, 4, 3] {
            native.push(datagram(sequence)).unwrap();
        }
        let receive = native.receive.lock().unwrap();
        assert_eq!(
            receive.queue.iter().cloned().collect::<Vec<_>>(),
            vec![vec![4], vec![2], vec![4], vec![3]]
        );
    }

    #[test]
    fn sideband_packets_reach_only_native_flows_and_never_end_the_session() {
        let flows = std::sync::Mutex::new(std::collections::HashMap::new());
        let tunnelled = Arc::new(DatagramState::new(
            DatagramDelivery::ReliableTunnel,
            1500,
            8,
        ));
        flows.lock().unwrap().insert(1, Arc::clone(&tunnelled));
        let frame = |sensitive: bool, payload: Vec<u8>| {
            let mut header = FrameHeader::event(family::NET, wire::event_kind::DATAGRAM);
            header.sensitive = sensitive;
            Frame { header, payload }
        };
        let packet = datagram(1).encode().unwrap();
        dispatch_event(&flows, frame(true, packet.clone()), true).unwrap();
        assert!(tunnelled.receive.lock().unwrap().queue.is_empty());
        dispatch_event(&flows, frame(true, packet.clone()), false).unwrap();
        assert_eq!(tunnelled.receive.lock().unwrap().queue.len(), 1);
        // Garbage on the sideband is loss; on the reliable stream, a protocol error.
        dispatch_event(&flows, frame(true, vec![1, 2]), true).unwrap();
        assert!(dispatch_event(&flows, frame(true, vec![1, 2]), false).is_err());
        assert!(dispatch_event(&flows, frame(false, packet), false).is_err());
    }

    #[test]
    fn net_failures_name_what_a_caller_acts_on() {
        let status = |status| Error::status_from("YAS Net OPEN", status, Extensions::default());
        assert_eq!(
            status(Status::Unavailable).net_failure(),
            NetFailure::Denied
        );
        assert_eq!(status(Status::NotFound).net_failure(), NetFailure::NotFound);
        assert_eq!(status(Status::Io).net_failure(), NetFailure::Refused);
        assert_eq!(status(Status::Timeout).net_failure(), NetFailure::Timeout);
        assert_eq!(
            Error::Timeout("local".into()).net_failure(),
            NetFailure::Timeout
        );
        assert_eq!(
            status(Status::ResourceExhausted).net_failure(),
            NetFailure::Exhausted
        );
        assert_eq!(Error::Closed.net_failure(), NetFailure::Other);
    }
}
