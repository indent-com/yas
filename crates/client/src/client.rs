//! The multiplexed, cloneable [`Client`].
//!
//! One [`Client`] owns one YAS session and any number of concurrent calls on
//! it. Two background tasks drive the session:
//!
//! - a **writer** that sends every outbound frame in the order it was queued
//!   (requests, Transfer data and credit, State acknowledgements);
//! - a **reader** that answers Core `PING`, applies catalogue updates, and
//!   routes each inbound frame: a `Result` to the call waiting on its request
//!   ID, a Transfer event to the stream that owns its transfer ID, a State
//!   event to the subscription that owns its `(family, subscription_id)`.
//!
//! Routes for the transfers and subscriptions a `Result` announces are
//! registered by the reader *before* it reads the next frame, and frames that
//! arrive for a route nobody has claimed yet are parked (bounded) until it is
//! claimed, so a stream never misses its first bytes.
//!
//! Dropping the last clone of a [`Client`] (and of every stream or handle
//! made from it) closes the session. On the server, closing a session
//! terminates every ordinary process it spawned; see [`crate::process`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use yas_wire::{
    Class, Decode, Encode, Frame, FrameHeader,
    core::{Cancel, ResultPrefix, ServerHello, Status},
    family,
};

use crate::error::{Error, Result};
use crate::native::{NativeClient, NativeFrameReader, NativeFrameSender};
use crate::transport::Transport;
use crate::{ConnectOptions, HelloOptions};

/// Default local deadline for requests that the server answers promptly.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on frames parked for transfers or subscriptions nobody claimed yet.
const MAX_ORPHAN_BYTES: usize = yas_wire::schema::transport::RECOMMENDED_BUFFERED as usize;
/// Remembered IDs of released transfers whose late frames are dropped.
const MAX_RELEASED_TRANSFERS: usize = 4096;

/// A route the reader delivers inbound events on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Route {
    /// Transfer events for one transfer ID.
    Transfer(u32),
    /// State events for one `(family, subscription_id)`.
    State(u16, u32),
    /// Process EXIT events for one process handle (a SPAWN with REPORT_EXIT).
    ProcessExit(u64),
}

/// Registered by a call: run by the reader on an OK `Result`, before any later
/// frame is routed, to name the routes the `Result` announces.
pub(crate) type Hook = Box<dyn FnOnce(&ResultPrefix) -> Vec<Route> + Send>;

pub(crate) type FrameReceiver = mpsc::UnboundedReceiver<Frame>;

/// A completed call: the `Result` and receivers for the routes its hook named.
pub(crate) struct Reply {
    pub(crate) prefix: ResultPrefix,
    routes: Vec<(Route, FrameReceiver)>,
    /// With a [`Route::ProcessExit`]: told if that process's attachment goes first.
    report_lost: Option<Arc<ReportLost>>,
}

impl Reply {
    pub(crate) fn take(&mut self, route: Route) -> Option<FrameReceiver> {
        let index = self.routes.iter().position(|(key, _)| *key == route)?;
        Some(self.routes.swap_remove(index).1)
    }

    pub(crate) fn take_report_lost(&mut self) -> Option<Arc<ReportLost>> {
        self.report_lost.take()
    }
}

/// Marked when the attachment that would report a process's exit (SPAWN_REPORT_EXIT) goes
/// before it: a Transfer RESET, sent or received, on any of its streams, or a DETACH. The
/// server then sends no EXIT, and the exit is asked for with WAIT.
#[derive(Debug, Default)]
pub(crate) struct ReportLost {
    lost: AtomicBool,
    notify: tokio::sync::Notify,
}

impl ReportLost {
    pub(crate) fn mark(&self) {
        self.lost.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// Once marked.
    pub(crate) async fn lost(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_lost() {
                return;
            }
            notified.await;
        }
    }
}

struct Pending {
    reply: oneshot::Sender<Result<Reply>>,
    hook: Option<Hook>,
}

#[derive(Default)]
struct Router {
    requests: HashMap<u32, Pending>,
    routes: HashMap<Route, mpsc::UnboundedSender<Frame>>,
    orphans: HashMap<Route, Vec<Frame>>,
    orphan_order: VecDeque<Route>,
    orphan_bytes: usize,
    released: HashSet<Route>,
    released_order: VecDeque<Route>,
    /// The open transfers of processes that report their exit: a RESET on one marks it.
    report_watch: HashMap<u32, Arc<ReportLost>>,
    closed: Option<Error>,
}

impl Router {
    fn register(&mut self, route: Route) -> FrameReceiver {
        let (sender, receiver) = mpsc::unbounded_channel();
        if let Some(frames) = self.orphans.remove(&route) {
            for frame in frames {
                self.orphan_bytes = self.orphan_bytes.saturating_sub(frame.payload.len());
                let _ = sender.send(frame);
            }
            self.orphan_order.retain(|key| *key != route);
        }
        self.released.remove(&route);
        self.routes.insert(route, sender);
        receiver
    }

    fn release(&mut self, route: Route) {
        self.routes.remove(&route);
        if let Route::Transfer(transfer_id) = route {
            self.report_watch.remove(&transfer_id);
        }
        if let Some(frames) = self.orphans.remove(&route) {
            for frame in frames {
                self.orphan_bytes = self.orphan_bytes.saturating_sub(frame.payload.len());
            }
            self.orphan_order.retain(|key| *key != route);
        }
        if self.released.insert(route) {
            self.released_order.push_back(route);
            while self.released_order.len() > MAX_RELEASED_TRANSFERS {
                if let Some(old) = self.released_order.pop_front() {
                    self.released.remove(&old);
                }
            }
        }
    }

    /// A transfer closed, or reset (either way): a reset one's process, if it reports its
    /// exit, will not.
    fn transfer_ended(&mut self, transfer_id: u32, reset: bool) {
        if let Some(lost) = self.report_watch.remove(&transfer_id)
            && reset
        {
            lost.mark();
        }
    }

    fn deliver(&mut self, route: Route, frame: Frame) {
        if let Some(sender) = self.routes.get(&route) {
            if sender.send(frame).is_err() {
                self.routes.remove(&route);
            }
            return;
        }
        if self.released.contains(&route) {
            return;
        }
        // Park until claimed; evict whole routes, oldest first, past the bound.
        self.orphan_bytes = self.orphan_bytes.saturating_add(frame.payload.len());
        let queue = self.orphans.entry(route).or_default();
        if queue.is_empty() {
            self.orphan_order.push_back(route);
        }
        queue.push(frame);
        while self.orphan_bytes > MAX_ORPHAN_BYTES {
            let Some(old) = self.orphan_order.pop_front() else {
                break;
            };
            if let Some(frames) = self.orphans.remove(&old) {
                for frame in frames {
                    self.orphan_bytes = self.orphan_bytes.saturating_sub(frame.payload.len());
                }
            }
        }
    }

    fn fail(&mut self, error: Error) {
        if self.closed.is_none() {
            self.closed = Some(error.clone());
        }
        for (_, pending) in self.requests.drain() {
            let _ = pending.reply.send(Err(error.clone()));
        }
        // Dropping the senders ends every stream and subscription; they then
        // report the session error.
        self.routes.clear();
        self.report_watch.clear();
        self.orphans.clear();
        self.orphan_order.clear();
        self.orphan_bytes = 0;
    }
}

struct Shared {
    hello: RwLock<ServerHello>,
    router: Mutex<Router>,
    closed: watch::Sender<Option<Error>>,
    /// Net datagram flows by flow handle: each a bounded, drop-oldest queue
    /// (a slow reader loses datagrams, as UDP does, instead of growing one).
    datagrams: Mutex<HashMap<u64, Arc<crate::net::DatagramState>>>,
    /// Bounds this session's Net opens in flight to the server's limit.
    net_opens: std::sync::OnceLock<Arc<tokio::sync::Semaphore>>,
}

impl Shared {
    fn close(&self, error: Error) {
        self.router.lock().unwrap().fail(error.clone());
        for (_, flow) in self.datagrams.lock().unwrap().drain() {
            flow.fail(error.clone());
        }
        self.closed.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(error);
                true
            } else {
                false
            }
        });
    }
}

struct Inner {
    shared: Arc<Shared>,
    outbound: mpsc::UnboundedSender<Frame>,
    next_request_id: AtomicU32,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// The session clock that input events' `client_monotonic_ns` count on.
    started: std::time::Instant,
    /// What this client offered to receive at once ([`HelloOptions::receive_budget`]).
    receive_budget: u64,
    /// The lossy datagram sideband's sender, when the transport has one.
    datagram_sender: Option<NativeFrameSender>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// A multiplexed YAS session: cheap to clone, safe to share across tasks.
///
/// ```no_run
/// # async fn demo() -> yas_client::Result<()> {
/// use yas_client::{Client, ConnectOptions, process::Command};
///
/// let client = Client::connect(Some("local:work"), &ConnectOptions::default()).await?;
/// let output = client
///     .spawn(&Command::new("uname").arg("-a"))
///     .await?
///     .output()
///     .await?;
/// println!("{}", String::from_utf8_lossy(&output.stdout));
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hello = self.inner.shared.hello.read().unwrap();
        f.debug_struct("Client")
            .field("server_name", &hello.server_name)
            .field("server_release", &hello.server_release)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connect to `target` (see [`crate::transport`] for URI forms; `None`
    /// uses `YAS_TARGET`, then `yas.target` in `yas.conf`, then `local`) and
    /// complete HELLO.
    pub async fn connect(target: Option<&str>, options: &ConnectOptions) -> Result<Self> {
        let native = NativeClient::connect(target, options).await?;
        Ok(Self::from_native(native))
    }

    /// Complete HELLO over an already connected [`Transport`].
    pub async fn from_transport(transport: Transport, options: &HelloOptions) -> Result<Self> {
        let native = NativeClient::connect_transport(transport, options).await?;
        Ok(Self::from_native(native))
    }

    /// Complete HELLO over any connected byte stream carrying native YAS: a
    /// socketpair end (see [`crate::host`]), an SSH channel, a pipe pair.
    pub async fn from_stream<S>(stream: S, options: &HelloOptions) -> Result<Self>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        Self::from_transport(Transport::from_stream(stream), options).await
    }

    /// Adopt a connected sequential session. Must be called within a Tokio
    /// runtime: it spawns the reader and writer tasks.
    pub fn from_native(native: NativeClient) -> Self {
        let hello = native.hello().clone();
        let receive_budget = native.receive_budget();
        let supports_datagrams = native.supports_datagrams();
        let (reader, sender) = native.into_framed();
        let datagram_sender = supports_datagrams.then(|| sender.clone());
        let (closed, _) = watch::channel(None);
        let shared = Arc::new(Shared {
            hello: RwLock::new(hello),
            router: Mutex::new(Router::default()),
            closed,
            datagrams: Mutex::new(HashMap::new()),
            net_opens: std::sync::OnceLock::new(),
        });
        let (outbound, outbound_rx) = mpsc::unbounded_channel();
        let writer = tokio::spawn(write_loop(sender, outbound_rx, Arc::clone(&shared)));
        let reader = tokio::spawn(read_loop(reader, Arc::clone(&shared)));
        Self {
            inner: Arc::new(Inner {
                shared,
                outbound,
                next_request_id: AtomicU32::new(3),
                tasks: vec![writer, reader],
                started: std::time::Instant::now(),
                receive_budget,
                datagram_sender,
            }),
        }
    }

    /// The server's HELLO answer, updated by catalogue changes.
    pub fn hello(&self) -> ServerHello {
        self.inner.shared.hello.read().unwrap().clone()
    }

    /// How many bytes the server may have on their way to this session at
    /// once, as this client offered in HELLO
    /// ([`HelloOptions::receive_budget`]).
    pub fn receive_budget(&self) -> u64 {
        self.inner.receive_budget
    }

    /// The server instance name (`default`, or the `--name` it runs under).
    pub fn server_name(&self) -> String {
        self.inner.shared.hello.read().unwrap().server_name.clone()
    }

    /// The server boot ID. Process handles, terminal IDs and staged writes are
    /// only meaningful within one boot.
    pub fn boot_id(&self) -> [u8; 16] {
        self.inner.shared.hello.read().unwrap().boot_id
    }

    /// This session's ID (the `owner_session` of processes it spawns).
    pub fn session_id(&self) -> [u8; 16] {
        self.inner.shared.hello.read().unwrap().session_id
    }

    /// Whether the negotiated catalogue includes `family_id` and it is
    /// currently available.
    pub fn has_family(&self, family_id: u16) -> bool {
        self.inner
            .shared
            .hello
            .read()
            .unwrap()
            .families
            .iter()
            .any(|descriptor| {
                descriptor.family_id == family_id
                    && descriptor.runtime_state == yas_wire::core::RuntimeState::Available
            })
    }

    /// Whether the server accepts `kind` of `class` in `family_id`.
    pub fn supports(&self, family_id: u16, class: Class, kind: u16) -> bool {
        self.inner
            .shared
            .hello
            .read()
            .unwrap()
            .families
            .iter()
            .find(|descriptor| descriptor.family_id == family_id)
            .and_then(|descriptor| descriptor.operation(class, kind))
            .is_some_and(|operation| operation.server_accepts)
    }

    /// Whether the session has ended, and why.
    pub fn closed_reason(&self) -> Option<Error> {
        self.inner.shared.closed.borrow().clone()
    }

    /// Wait until the session ends and return why.
    pub async fn closed(&self) -> Error {
        let mut receiver = self.inner.shared.closed.subscribe();
        loop {
            if let Some(error) = receiver.borrow_and_update().clone() {
                return error;
            }
            if receiver.changed().await.is_err() {
                return Error::Closed;
            }
        }
    }

    /// Close the session now. Other clones and streams then fail with
    /// [`Error::Closed`]; the server terminates this session's ordinary
    /// processes.
    pub fn close(&self) {
        self.inner.shared.close(Error::Closed);
        for task in &self.inner.tasks {
            task.abort();
        }
    }

    /// Send one Request and wait for its `Result` without interpreting its
    /// status. `sensitive` follows the operation's schema policy.
    ///
    /// This is the escape hatch for families this crate has no typed API for;
    /// payloads are `yas_wire` values.
    pub async fn request_raw(
        &self,
        family_id: u16,
        kind: u16,
        payload: Vec<u8>,
        timeout: Option<Duration>,
    ) -> Result<ResultPrefix> {
        Ok(self
            .call(family_id, kind, payload, timeout, None)
            .await?
            .prefix)
    }

    /// Send a typed Request and decode its OK `Result` body.
    pub async fn request<Q: Encode, R: Decode>(
        &self,
        family_id: u16,
        kind: u16,
        request: &Q,
    ) -> Result<R> {
        let reply = self
            .call_ok(
                family_id,
                kind,
                request.encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                None,
            )
            .await?;
        Ok(R::decode(&reply.prefix.body)?)
    }

    /// Replace the identifier this session reported in HELLO (the `identifier`
    /// of its [`HelloOptions`](crate::HelloOptions)) with Core CLIENT_UPDATE.
    /// Client catalogue watchers see it at their next refresh. A read-only
    /// session cannot: its HELLO identifier stays.
    pub async fn set_identifier(&self, identifier: &str) -> Result<()> {
        let extension = yas_wire::core::client_identifier_extension(identifier)
            .map_err(|error| Error::invalid(format!("invalid client identifier: {error}")))?;
        let extensions = yas_wire::Extensions(vec![extension]);
        self.call_ok(
            yas_wire::family::CORE,
            yas_wire::core::request_kind::CLIENT_UPDATE,
            extensions.encode()?,
            Some(DEFAULT_REQUEST_TIMEOUT),
            None,
        )
        .await?;
        Ok(())
    }

    /// Nanoseconds on this session's clock, for input events.
    pub(crate) fn monotonic_ns(&self) -> u64 {
        u64::try_from(self.inner.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Whether the transport carries a lossy datagram sideband.
    pub(crate) fn supports_datagrams(&self) -> bool {
        self.inner.datagram_sender.is_some()
    }

    /// Try one frame on the lossy datagram sideband.
    pub(crate) fn try_send_datagram(
        &self,
        frame: &Frame,
        context: yas_wire::frame::DatagramContext,
    ) -> Result<crate::transport::DatagramSend> {
        let Some(sender) = &self.inner.datagram_sender else {
            return Ok(crate::transport::DatagramSend::Closed);
        };
        let maximum = self.inner.shared.hello.read().unwrap().receive.max_datagram;
        sender.try_send_datagram(frame, maximum, context)
    }

    /// The registry Net datagram flows are delivered through.
    pub(crate) fn datagrams(&self) -> Datagrams {
        Datagrams(Arc::downgrade(&self.inner.shared))
    }

    /// The semaphore bounding Net opens in flight, made on first use.
    pub(crate) fn net_opens(&self, permits: usize) -> Arc<tokio::sync::Semaphore> {
        self.inner
            .shared
            .net_opens
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(permits.max(1))))
            .clone()
    }

    /// Send one Request whose `Result` nobody waits for (a fire-and-forget
    /// Net `CLOSE` from a destructor): its late `Result` is ignored.
    pub(crate) fn send_request_detached(&self, family_id: u16, kind: u16, payload: Vec<u8>) {
        if !self.supports(family_id, Class::Request, kind) {
            return;
        }
        let request_id = self.next_request_id();
        let mut header = FrameHeader::request(family_id, kind, request_id);
        header.sensitive = default_sensitive(family_id, Class::Request, kind);
        let _ = self.send_frame(Frame { header, payload });
    }

    pub(crate) fn release(&self, route: Route) {
        self.inner.shared.router.lock().unwrap().release(route);
    }

    pub(crate) fn send_frame(&self, frame: Frame) -> Result<()> {
        if let Some(error) = self.closed_reason() {
            return Err(error);
        }
        if frame.header.kind == yas_wire::transfer::kind::RESET
            && let Some(transfer_id) = transfer_event_id(&frame)
        {
            self.inner
                .shared
                .router
                .lock()
                .unwrap()
                .transfer_ended(transfer_id, true);
        }
        self.inner
            .outbound
            .send(frame)
            .map_err(|_| self.closed_reason().unwrap_or(Error::Closed))
    }

    pub(crate) fn send_event<E: Encode>(
        &self,
        family_id: u16,
        kind: u16,
        event: &E,
        sensitive: bool,
    ) -> Result<()> {
        let mut header = FrameHeader::event(family_id, kind);
        header.sensitive = sensitive;
        self.send_frame(Frame {
            header,
            payload: event.encode()?,
        })
    }

    /// Like [`Client::call`], but a non-OK status becomes [`Error::Status`].
    pub(crate) async fn call_ok(
        &self,
        family_id: u16,
        kind: u16,
        payload: Vec<u8>,
        timeout: Option<Duration>,
        hook: Option<Hook>,
    ) -> Result<Reply> {
        let reply = self.call(family_id, kind, payload, timeout, hook).await?;
        if reply.prefix.status != Status::Ok {
            return Err(Error::status_from(
                format!("YAS request {family_id:#06x}/{kind:#06x}"),
                reply.prefix.status,
                reply.prefix.detail,
            ));
        }
        Ok(reply)
    }

    /// Send one Request; `hook` runs in the reader on an OK `Result` to name
    /// the transfer and State routes it announces.
    pub(crate) async fn call(
        &self,
        family_id: u16,
        kind: u16,
        payload: Vec<u8>,
        timeout: Option<Duration>,
        hook: Option<Hook>,
    ) -> Result<Reply> {
        if !self.supports(family_id, Class::Request, kind) {
            return Err(Error::Unsupported(format!(
                "this YAS session does not offer request {family_id:#06x}/{kind:#06x} \
                 (family not negotiated, unavailable, or read-only session)"
            )));
        }
        let request_id = self.next_request_id();
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut router = self.inner.shared.router.lock().unwrap();
            if let Some(error) = &router.closed {
                return Err(error.clone());
            }
            router.requests.insert(
                request_id,
                Pending {
                    reply: reply_tx,
                    hook,
                },
            );
        }
        let mut header = FrameHeader::request(family_id, kind, request_id);
        header.sensitive = default_sensitive(family_id, Class::Request, kind);
        if let Err(error) = self.send_frame(Frame { header, payload }) {
            self.inner
                .shared
                .router
                .lock()
                .unwrap()
                .requests
                .remove(&request_id);
            return Err(error);
        }
        let outcome = match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, reply_rx).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    self.abandon(request_id);
                    return Err(Error::Timeout(format!(
                        "timed out waiting for {family_id:#06x}/{kind:#06x} Result"
                    )));
                }
            },
            None => reply_rx.await,
        };
        outcome.map_err(|_| self.closed_reason().unwrap_or(Error::Closed))?
    }

    /// Forget a request whose caller stopped waiting, and ask the server to
    /// cancel it. A late `Result` is then ignored.
    fn abandon(&self, request_id: u32) {
        self.inner
            .shared
            .router
            .lock()
            .unwrap()
            .requests
            .remove(&request_id);
        if let Ok(payload) = (Cancel {
            target_request_id: request_id,
        })
        .encode()
        {
            let id = self.next_request_id();
            let _ = self.send_frame(Frame {
                header: FrameHeader::request(
                    family::CORE,
                    yas_wire::core::request_kind::CANCEL,
                    id,
                ),
                payload,
            });
        }
    }

    pub(crate) fn next_request_id(&self) -> u32 {
        loop {
            let id = self.inner.next_request_id.fetch_add(2, Ordering::Relaxed) | 1;
            // 1 is HELLO; keep client IDs odd and above it across wraparound.
            if id > 1 {
                return id;
            }
        }
    }
}

/// Default sensitivity for a frame: required → set, forbidden → clear,
/// allowed → set (paths, arguments and file contents are private).
pub(crate) fn default_sensitive(family_id: u16, class: Class, kind: u16) -> bool {
    let class = match class {
        Class::Request => yas_wire::schema::transport::class::REQUEST,
        Class::Event => yas_wire::schema::transport::class::EVENT,
        Class::Result => return true,
    };
    yas_wire::schema::operation(family_id, class, kind).is_none_or(|operation| {
        operation.sensitive != yas_wire::schema::transport::policy::FORBIDDEN
    })
}

async fn write_loop(
    sender: NativeFrameSender,
    mut outbound: mpsc::UnboundedReceiver<Frame>,
    shared: Arc<Shared>,
) {
    while let Some(frame) = outbound.recv().await {
        if let Err(error) = sender.send(frame).await {
            shared.close(error);
            return;
        }
    }
}

async fn read_loop(mut reader: NativeFrameReader, shared: Arc<Shared>) {
    let mut revision = reader.hello().catalog_revision;
    loop {
        let (frame, transport_datagram) = match reader.next_with_source().await {
            Ok(frame) => frame,
            Err(error) => {
                shared.close(error);
                return;
            }
        };
        if reader.hello().catalog_revision != revision {
            revision = reader.hello().catalog_revision;
            *shared.hello.write().unwrap() = reader.hello().clone();
        }
        if let Err(error) = dispatch(&shared, frame, transport_datagram) {
            shared.close(error);
            return;
        }
    }
}

fn dispatch(shared: &Shared, frame: Frame, transport_datagram: bool) -> Result<()> {
    match frame.header.class {
        Class::Result => {
            let Some(request_id) = frame.header.request_id else {
                return Err(Error::protocol("YAS Result without a request ID"));
            };
            let mut router = shared.router.lock().unwrap();
            let Some(pending) = router.requests.remove(&request_id) else {
                // A Result for a request the caller abandoned (or a CANCEL).
                return Ok(());
            };
            let prefix = ResultPrefix::decode(&frame.payload)?;
            let mut routes = Vec::new();
            if prefix.status == Status::Ok
                && let Some(hook) = pending.hook
            {
                for route in hook(&prefix) {
                    let receiver = router.register(route);
                    routes.push((route, receiver));
                }
            }
            // A process that reports its exit: any of its transfers reset (its attachment
            // went) means no EXIT comes.
            let report_lost = routes
                .iter()
                .any(|(route, _)| matches!(route, Route::ProcessExit(_)))
                .then(|| {
                    let lost = Arc::new(ReportLost::default());
                    for (route, _) in &routes {
                        if let Route::Transfer(transfer_id) = route {
                            router.report_watch.insert(*transfer_id, lost.clone());
                        }
                    }
                    lost
                });
            let reply = Reply {
                prefix,
                routes,
                report_lost,
            };
            if pending.reply.send(Ok(reply)).is_err() {
                // The caller went away between sending and now.
            }
            Ok(())
        }
        Class::Event => {
            if frame.header.family == family::TRANSFER {
                if let Some(transfer_id) = transfer_event_id(&frame) {
                    let mut router = shared.router.lock().unwrap();
                    let kind = frame.header.kind;
                    if matches!(
                        kind,
                        yas_wire::transfer::kind::CLOSE | yas_wire::transfer::kind::RESET
                    ) {
                        router.transfer_ended(transfer_id, kind == yas_wire::transfer::kind::RESET);
                    }
                    router.deliver(Route::Transfer(transfer_id), frame);
                }
                return Ok(());
            }
            if frame.header.family == family::NET {
                return crate::net::dispatch_event(&shared.datagrams, frame, transport_datagram);
            }
            if frame.header.family == family::PROCESS
                && frame.header.kind == yas_wire::process::event_kind::EXIT
            {
                if let Some(handle) = yas_wire::process::ExitReport::handle_of(&frame.payload) {
                    shared
                        .router
                        .lock()
                        .unwrap()
                        .deliver(Route::ProcessExit(handle), frame);
                }
                return Ok(());
            }
            if frame.header.kind == 0 && family_has_state(frame.header.family) {
                if let Some(subscription) = state_subscription_id(&frame.payload) {
                    shared
                        .router
                        .lock()
                        .unwrap()
                        .deliver(Route::State(frame.header.family, subscription), frame);
                }
                return Ok(());
            }
            // Views (terminal/surface frames) and other unsolicited events
            // are not used by this client.
            Ok(())
        }
        Class::Request => Err(Error::protocol(format!(
            "YAS server sent an unsupported peer Request {:#06x}/{:#06x}",
            frame.header.family, frame.header.kind
        ))),
    }
}

/// The Net datagram flows of a session, held weakly: a flow keeps its
/// registry entry, never the session.
#[derive(Clone)]
pub(crate) struct Datagrams(std::sync::Weak<Shared>);

impl Datagrams {
    pub(crate) fn insert(&self, handle: u64, flow: Arc<crate::net::DatagramState>) {
        if let Some(shared) = self.0.upgrade() {
            if let Some(error) = shared.closed.borrow().clone() {
                flow.fail(error);
                return;
            }
            shared.datagrams.lock().unwrap().insert(handle, flow);
        }
    }

    pub(crate) fn get(&self, handle: u64) -> Option<Arc<crate::net::DatagramState>> {
        self.0
            .upgrade()
            .and_then(|shared| shared.datagrams.lock().unwrap().get(&handle).cloned())
    }

    pub(crate) fn remove(&self, handle: u64) {
        if let Some(shared) = self.0.upgrade() {
            shared.datagrams.lock().unwrap().remove(&handle);
        }
    }
}

/// Families whose event kind 0 is a State event.
fn family_has_state(family_id: u16) -> bool {
    yas_wire::schema::operation(family_id, yas_wire::schema::transport::class::EVENT, 0)
        .is_some_and(|operation| operation.name == "STATE")
}

fn state_subscription_id(payload: &[u8]) -> Option<u32> {
    payload
        .get(..4)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("four bytes")))
        .filter(|id| *id != 0)
}

pub(crate) fn transfer_event_id(frame: &Frame) -> Option<u32> {
    (frame.header.class == Class::Event
        && frame.header.family == family::TRANSFER
        && frame.payload.len() >= 4)
        .then(|| u32::from_le_bytes(frame.payload[..4].try_into().expect("four bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_with_state_are_found_from_the_schema() {
        assert!(family_has_state(family::PROCESS));
        assert!(family_has_state(family::FS));
        assert!(family_has_state(family::KV));
        assert!(family_has_state(family::TERMINAL));
        assert!(!family_has_state(family::TRANSFER));
        assert!(!family_has_state(family::ENV));
    }

    #[test]
    fn process_spawn_is_sensitive() {
        assert!(default_sensitive(
            family::PROCESS,
            Class::Request,
            yas_wire::process::request_kind::SPAWN
        ));
    }

    #[test]
    fn orphans_are_delivered_when_claimed_and_dropped_after_release() {
        let mut router = Router::default();
        let frame = |byte: u8| Frame {
            header: FrameHeader::event(family::TRANSFER, 0),
            payload: vec![7, 0, 0, 0, byte],
        };
        router.deliver(Route::Transfer(7), frame(1));
        router.deliver(Route::Transfer(7), frame(2));
        let mut receiver = router.register(Route::Transfer(7));
        assert_eq!(receiver.try_recv().unwrap().payload[4], 1);
        assert_eq!(receiver.try_recv().unwrap().payload[4], 2);
        router.release(Route::Transfer(7));
        router.deliver(Route::Transfer(7), frame(3));
        assert!(router.orphans.is_empty());
        assert_eq!(router.orphan_bytes, 0);
    }

    #[test]
    fn orphans_are_bounded() {
        let mut router = Router::default();
        let big = MAX_ORPHAN_BYTES / 2 + 1;
        for id in 0..4 {
            router.deliver(
                Route::Transfer(id),
                Frame {
                    header: FrameHeader::event(family::TRANSFER, 0),
                    payload: vec![0; big],
                },
            );
        }
        assert!(router.orphan_bytes <= MAX_ORPHAN_BYTES);
        assert_eq!(router.orphans.len(), 1);
        assert!(router.orphans.contains_key(&Route::Transfer(3)));
    }
}
