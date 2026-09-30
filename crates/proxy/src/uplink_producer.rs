//! The producer side of a YAS uplink (`yas uplink`, docs/uplink.md), as a
//! library: publish a local YAS server through an untrusted relay.
//!
//! A [`Producer`] authenticates to an HTTPS control endpoint, receives a pool
//! of relays, holds a session with one of them, and bridges each consumer the
//! relay sends it to a fresh connection to the local server, once the consumer
//! has passed end-to-end Noise IK authentication against pinned keys. The
//! session is carried either by WebTransport (HTTP/3 over UDP) or by
//! WebSockets (over TCP, where networks block UDP); [`Transport`] chooses, and
//! [`Transport::Auto`] falls back from one to the other by itself.
//!
//! ```no_run
//! # async fn run() -> Result<(), String> {
//! use yas_proxy::uplink_producer::{Local, Producer, Transport};
//!
//! let identity = yas_uplink::Identity::from_base64(&std::env::var("YAS_UPLINK_IDENTITY").unwrap())?;
//! let allowed = vec!["CLIENT_PUBLIC_KEY_43_CHARACTERS_OF_BASE64URL".parse()?];
//! Producer::new(
//!     "https://relay.example/uplink/control",
//!     &std::env::var("YAS_UPLINK_TOKEN").unwrap(),
//!     identity.server_config(allowed)?,
//!     Local::socket("/run/user/1000/yas/yas.sock"),
//! )?
//! .transport(Transport::from_env()?)
//! .on_event(|event| eprintln!("[uplink] {event}"))
//! .run_until(async { tokio::signal::ctrl_c().await.ok(); })
//! .await
//! # }
//! ```

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use web_transport_quinn as wt;

/// The keys a producer is configured with.
pub use yas_uplink::{Identity, PublicKey, ServerConfig};

/// The WebSocket subprotocol of the uplink's WebSocket relay sessions and
/// streams (docs/uplink.md, "WebSocket relay session"), with its version.
pub const WEBSOCKET_SUBPROTOCOL: &str = "yas-uplink.v1";

/// The environment variable [`Transport::from_env`] reads.
pub const TRANSPORT_ENV: &str = "YAS_UPLINK_TRANSPORT";

/// WebTransport application code closing a session on shutdown.
const CODE_SHUTDOWN: u32 = 2;
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const MAX_DATAGRAM_ROUTES: usize = 4_096;
const DATAGRAM_QUEUE: usize = 64;
/// Consumer streams authenticating at once on one session.
const MAX_PENDING: usize = 64;
/// Liveness of a session: a keepalive this often, dead after this long silent.
const KEEPALIVE: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a WebSocket (session or stream) may take to open.
const WEBSOCKET_CONNECT: Duration = Duration::from_secs(10);
/// How long [`Transport::Auto`] waits for a WebTransport session before it
/// tries the pool's WebSocket relays. Where UDP is dropped rather than
/// refused, QUIC would otherwise wait out its whole idle timeout.
const WEBTRANSPORT_CONNECT_AUTO: Duration = Duration::from_secs(5);
/// A WebTransport session shorter than this counts as WebTransport failing.
const SHORT_SESSION: Duration = Duration::from_secs(60);
/// How long [`Transport::Auto`] tries WebSockets first after WebTransport failed.
const PREFER_WEBSOCKET_FOR: Duration = Duration::from_secs(600);
/// The largest WebSocket message either way.
const MAX_WEBSOCKET_MESSAGE: usize = 64 << 10;

type DatagramRoutes = yas_composite_transport::RoutedDatagramRoutes;
type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// What carries the relay session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    /// WebTransport when it works, else WebSockets (when the control endpoint
    /// offers WebSocket relays): the default.
    #[default]
    Auto,
    /// WebTransport (HTTP/3 over UDP) only.
    WebTransport,
    /// WebSockets (over TCP) only.
    WebSocket,
}

impl Transport {
    /// `YAS_UPLINK_TRANSPORT` (`auto`, `webtransport` or `websocket`), else
    /// [`Transport::Auto`].
    pub fn from_env() -> Result<Self, String> {
        match std::env::var(TRANSPORT_ENV) {
            Ok(value) if !value.trim().is_empty() => value.parse(),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(Self::Auto),
            Err(std::env::VarError::NotUnicode(_)) => Err(format!(
                "{TRANSPORT_ENV} must be auto, webtransport or websocket"
            )),
        }
    }
}

impl std::str::FromStr for Transport {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "webtransport" => Ok(Self::WebTransport),
            "websocket" => Ok(Self::WebSocket),
            other => Err(format!(
                "unknown uplink transport {other:?}: auto, webtransport or websocket"
            )),
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::WebTransport => "webtransport",
            Self::WebSocket => "websocket",
        })
    }
}

/// What carries one relay session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carrier {
    WebTransport,
    WebSocket,
}

impl fmt::Display for Carrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WebTransport => "WebTransport",
            Self::WebSocket => "WebSocket",
        })
    }
}

type Connect = dyn Fn() -> Pin<Box<dyn Future<Output = Result<(BoxRead, BoxWrite), String>> + Send>>
    + Send
    + Sync;

/// How the producer reaches the local YAS server, once per consumer (twice for
/// a consumer with a datagram lane).
#[derive(Clone)]
pub struct Local {
    label: String,
    connect: Arc<Connect>,
}

impl Local {
    /// The YAS server listening at `path`: a Unix socket, or a named pipe on
    /// Windows.
    pub fn socket(path: impl Into<String>) -> Self {
        let path = path.into();
        let label = path.clone();
        Self {
            label,
            connect: Arc::new(move || {
                let path = path.clone();
                Box::pin(async move { connect_socket_split(&path).await })
            }),
        }
    }

    /// Any other way to reach the server (an in-process one, say): `connect`
    /// opens a fresh byte stream to it each time; `label` names it in events.
    pub fn custom<F, Fut, S>(label: impl Into<String>, connect: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::io::Result<S>> + Send + 'static,
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            label: label.into(),
            connect: Arc::new(move || {
                let connecting = connect();
                Box::pin(async move {
                    let stream = connecting.await.map_err(|error| error.to_string())?;
                    let (reader, writer) = tokio::io::split(stream);
                    Ok((Box::new(reader) as BoxRead, Box::new(writer) as BoxWrite))
                })
            }),
        }
    }

    async fn connect(&self) -> Result<(BoxRead, BoxWrite), String> {
        (self.connect)().await
    }
}

impl fmt::Debug for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Local").field("label", &self.label).finish()
    }
}

async fn connect_socket_split(path: &str) -> Result<(BoxRead, BoxWrite), String> {
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|error| error.to_string())?;
        let (reader, writer) = tokio::io::split(stream);
        Ok((Box::new(reader), Box::new(writer)))
    }
    #[cfg(windows)]
    {
        // Every instance may be taken for a while (the server makes its next
        // one only once it accepts the last): wait out ERROR_PIPE_BUSY.
        use tokio::net::windows::named_pipe::ClientOptions;
        const ERROR_PIPE_BUSY: i32 = 231;
        let deadline = Instant::now() + Duration::from_secs(10);
        let pipe = loop {
            match ClientOptions::new().open(path) {
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                opened => break opened.map_err(|error| error.to_string())?,
            }
        };
        let (reader, writer) = tokio::io::split(pipe);
        Ok((Box::new(reader), Box::new(writer)))
    }
}

/// What happens to a producer, for logs and status. Displayed, each is the
/// text `yas uplink` prints after `[uplink] `.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Event {
    /// A relay session is up: consumers can reach the server. `relay` is the
    /// relay's `host:port` (its URL is a credential and never shown).
    Connected { relay: String, carrier: Carrier },
    /// The session that was up ended; the producer asks the control endpoint
    /// for a new pool at once.
    SessionEnded { carrier: Carrier, reason: String },
    /// A relay of the pool didn't take a session; the next one is tried.
    RelayFailed {
        relay: String,
        carrier: Carrier,
        error: String,
    },
    /// No relay of the pool took a session.
    PoolExhausted { retry_in: Duration },
    /// The control endpoint couldn't give a pool (unreachable, an error, a
    /// pool naming no relay the transport may use).
    ControlRetry { reason: String, retry_in: Duration },
    /// An authenticated consumer couldn't reach the local server.
    LocalUnavailable { local: String, error: String },
    /// A consumer stream couldn't be served (a WebSocket stream the relay
    /// asked for that didn't open, a datagram lane this session can't carry).
    StreamFailed { error: String },
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let over = |carrier: &Carrier| match carrier {
            Carrier::WebTransport => "",
            Carrier::WebSocket => " over WebSocket",
        };
        match self {
            Self::Connected { relay, carrier } => {
                write!(f, "connected to relay {relay}{}", over(carrier))
            }
            Self::SessionEnded { reason, .. } => write!(f, "session ended: {reason}"),
            Self::RelayFailed {
                relay,
                carrier,
                error,
            } => write!(f, "relay {relay}{}: {error}", over(carrier)),
            Self::PoolExhausted { retry_in } => write!(
                f,
                "relay pool exhausted; re-querying in {}s",
                retry_in.as_secs()
            ),
            Self::ControlRetry { reason, retry_in } => {
                write!(f, "{reason}; retrying in {}s", retry_in.as_secs())
            }
            Self::LocalUnavailable { local, error } => {
                write!(f, "local yas server unavailable at {local}: {error}")
            }
            Self::StreamFailed { error } => write!(f, "consumer stream failed: {error}"),
        }
    }
}

type Events = Arc<dyn Fn(&Event) + Send + Sync>;

/// A YAS server published through a relay. See the module documentation.
#[derive(Clone)]
pub struct Producer {
    control: url::Url,
    shared: Arc<Shared>,
    local: Local,
    transport: Transport,
    events: Events,
}

/// What a running producer takes from its [`Handle`].
struct Shared {
    token: Mutex<String>,
    crypto: Mutex<Arc<yas_uplink::ServerConfig>>,
}

impl Shared {
    fn token(&self) -> String {
        self.token.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn crypto(&self) -> Arc<yas_uplink::ServerConfig> {
        self.crypto
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Changes a running [`Producer`] takes without restarting its session.
#[derive(Clone)]
pub struct Handle(Arc<Shared>);

impl Handle {
    /// The token the next control request presents (a renewed one, say).
    pub fn set_token(&self, token: impl Into<String>) {
        *self.0.token.lock().unwrap_or_else(|p| p.into_inner()) = token.into();
    }

    /// Who may connect from now on: consumers that start their handshake
    /// afterwards are checked against `crypto`'s allowlist (and answered with
    /// its identity). Consumers already connected keep their authority until
    /// they disconnect, as they do across a restart.
    pub fn set_server_config(&self, crypto: Arc<yas_uplink::ServerConfig>) {
        *self.0.crypto.lock().unwrap_or_else(|p| p.into_inner()) = crypto;
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle").finish_non_exhaustive()
    }
}

impl fmt::Debug for Producer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The token and keys are credentials: never shown.
        f.debug_struct("Producer")
            .field("control", &label(&self.control))
            .field("local", &self.local)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

impl Producer {
    /// A producer authenticating to `control` (HTTPS, without userinfo or a
    /// fragment) with `token`, admitting the consumers `crypto` allows
    /// ([`yas_uplink::Identity::server_config`]), bridged to `local`.
    pub fn new(
        control: &str,
        token: &str,
        crypto: Arc<yas_uplink::ServerConfig>,
        local: Local,
    ) -> Result<Self, String> {
        let control = url::Url::parse(control).map_err(|_| "invalid uplink control URL")?;
        if control.scheme() != "https"
            || control.host_str().is_none()
            || !control.username().is_empty()
            || control.password().is_some()
            || control.fragment().is_some()
        {
            return Err("uplink control URL must be HTTPS without userinfo or a fragment".into());
        }
        if token.is_empty() {
            return Err("YAS_UPLINK_TOKEN is not set".into());
        }
        Ok(Self {
            control,
            shared: Arc::new(Shared {
                token: Mutex::new(token.to_owned()),
                crypto: Mutex::new(crypto),
            }),
            local,
            transport: Transport::Auto,
            events: Arc::new(|_| {}),
        })
    }

    /// Where to change the token and the allowlist while it runs.
    pub fn handle(&self) -> Handle {
        Handle(self.shared.clone())
    }

    /// What may carry the relay session ([`Transport::Auto`] by default).
    pub fn transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Hear what happens (nothing is printed otherwise).
    pub fn on_event(mut self, events: impl Fn(&Event) + Send + Sync + 'static) -> Self {
        self.events = Arc::new(events);
        self
    }

    /// Publish the server until a fatal error: the control endpoint refusing
    /// the token. Every other failure is retried.
    pub async fn run(self) -> Result<(), String> {
        self.run_until(std::future::pending()).await
    }

    /// [`Producer::run`] until `shutdown` completes, then close the relay
    /// session rather than let it idle out on the relay.
    pub async fn run_until(self, shutdown: impl Future<Output = ()>) -> Result<(), String> {
        let active = Active::default();
        tokio::select! {
            () = shutdown => {
                active.close().await;
                Ok(())
            }
            result = self.run_loop(&active) => result,
        }
    }

    fn emit(&self, event: Event) {
        (self.events)(&event);
    }

    async fn run_loop(&self, active: &Active) -> Result<(), String> {
        let http = crate::uplink_http_client()?;
        let mut backoff = INITIAL_BACKOFF;
        let mut prefer_websocket_until: Option<Instant> = None;
        loop {
            let token = self.shared.token();
            let pool = match fetch_pool(&http, &self.control, &token).await? {
                FetchOutcome::Pool(pool) => pool,
                FetchOutcome::Retry { after, reason } => {
                    let delay = after.unwrap_or(backoff);
                    self.emit(Event::ControlRetry {
                        reason,
                        retry_in: delay,
                    });
                    tokio::time::sleep(jittered(delay)).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            };
            let websocket_first =
                prefer_websocket_until.is_some_and(|until| Instant::now() < until);
            let relays = order(pool, self.transport, websocket_first);
            if relays.is_empty() {
                let reason = format!(
                    "the relay pool has no relay for the {} transport",
                    self.transport
                );
                self.emit(Event::ControlRetry {
                    reason,
                    retry_in: backoff,
                });
                tokio::time::sleep(jittered(backoff)).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
            let has_websocket = relays
                .iter()
                .any(|relay| relay.carrier == Carrier::WebSocket);
            // A session that was actually established ends with a fresh
            // control-plane query, per docs/uplink.md.
            let mut established = false;
            for relay in &relays {
                let end = match relay.carrier {
                    Carrier::WebTransport => {
                        let limit = (self.transport == Transport::Auto && has_websocket)
                            .then_some(WEBTRANSPORT_CONNECT_AUTO);
                        self.webtransport_session(relay, limit, active).await
                    }
                    Carrier::WebSocket => self.websocket_session(relay, active).await,
                };
                match end {
                    SessionEnd::Ended { reason, lasted } => {
                        self.emit(Event::SessionEnded {
                            carrier: relay.carrier,
                            reason,
                        });
                        if relay.carrier == Carrier::WebTransport && lasted < SHORT_SESSION {
                            prefer_websocket_until = Some(Instant::now() + PREFER_WEBSOCKET_FOR);
                        }
                        established = true;
                        break;
                    }
                    SessionEnd::NeverConnected(error) => {
                        if relay.carrier == Carrier::WebTransport {
                            prefer_websocket_until = Some(Instant::now() + PREFER_WEBSOCKET_FOR);
                        }
                        self.emit(Event::RelayFailed {
                            relay: relay.label.clone(),
                            carrier: relay.carrier,
                            error,
                        });
                    }
                }
            }
            if established {
                backoff = INITIAL_BACKOFF;
            } else {
                self.emit(Event::PoolExhausted { retry_in: backoff });
                tokio::time::sleep(jittered(backoff)).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }

    async fn webtransport_session(
        &self,
        relay: &Relay,
        limit: Option<Duration>,
        active: &Active,
    ) -> SessionEnd {
        let client = match webtransport_client(relay.cert_hash.as_deref()) {
            Ok(client) => client,
            Err(error) => return SessionEnd::NeverConnected(error),
        };
        // Careful: the URL is the credential — show `relay.label` only.
        let connecting = client.connect(relay.url.clone());
        let connected = match limit {
            Some(limit) => match tokio::time::timeout(limit, connecting).await {
                Ok(connected) => connected,
                Err(_) => {
                    return SessionEnd::NeverConnected(format!(
                        "no WebTransport session within {}s (is UDP blocked?)",
                        limit.as_secs()
                    ));
                }
            },
            None => connecting.await,
        };
        let session = match connected {
            Ok(session) => session,
            Err(error) => return SessionEnd::NeverConnected(format!("connect failed: {error}")),
        };
        let started = Instant::now();
        self.emit(Event::Connected {
            relay: relay.label.clone(),
            carrier: Carrier::WebTransport,
        });
        active.set(Session::WebTransport(Box::new(session.clone())));

        let routes = DatagramRoutes::new(MAX_DATAGRAM_ROUTES);
        let distributing = tokio::spawn(distribute_datagrams(session.clone(), routes.clone()));
        let pending = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING));
        let reason = loop {
            match session.accept_bi().await {
                Ok((send, recv)) => {
                    let Ok(permit) = pending.clone().try_acquire_owned() else {
                        continue;
                    };
                    let lane = Lane::WebTransport {
                        session: Box::new(session.clone()),
                        routes: routes.clone(),
                    };
                    tokio::spawn(bridge(
                        tokio::io::join(recv, send),
                        lane,
                        self.shared.clone(),
                        permit,
                        self.local.clone(),
                        self.events.clone(),
                    ));
                }
                Err(error) => break error.to_string(),
            }
        };
        distributing.abort();
        active.clear();
        SessionEnd::Ended {
            reason,
            lasted: started.elapsed(),
        }
    }

    async fn websocket_session(&self, relay: &Relay, active: &Active) -> SessionEnd {
        let socket = match connect_websocket(&relay.url, relay.cert_hash.as_deref()).await {
            Ok(socket) => socket,
            Err(error) => return SessionEnd::NeverConnected(error),
        };
        let started = Instant::now();
        self.emit(Event::Connected {
            relay: relay.label.clone(),
            carrier: Carrier::WebSocket,
        });
        let (sink, mut source) = socket.split();
        let sink = Arc::new(tokio::sync::Mutex::new(sink));
        active.set(Session::WebSocket(sink.clone()));

        let pending = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING));
        // Stream WebSockets are connections of their own: they end with the
        // session (dropped, the set aborts them), as WebTransport streams end
        // with their QUIC connection.
        let mut streams = tokio::task::JoinSet::new();
        let mut keepalive = tokio::time::interval(KEEPALIVE);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        keepalive.tick().await;
        let mut heard = Instant::now();
        let reason = loop {
            tokio::select! {
                message = source.next() => match message {
                    Some(Ok(Message::Text(text))) => {
                        heard = Instant::now();
                        let Some(stream) = stream_request(&text, &relay.url) else {
                            continue;
                        };
                        let Ok(permit) = pending.clone().try_acquire_owned() else {
                            continue;
                        };
                        streams.spawn(websocket_stream(
                            stream,
                            relay.cert_hash.clone(),
                            self.shared.clone(),
                            permit,
                            self.local.clone(),
                            self.events.clone(),
                        ));
                    }
                    Some(Ok(Message::Close(frame))) => {
                        break match frame {
                            Some(frame) if !frame.reason.is_empty() => {
                                format!("closed by the relay: {}", frame.reason)
                            }
                            _ => "closed by the relay".to_owned(),
                        };
                    }
                    Some(Ok(_)) => heard = Instant::now(),
                    Some(Err(error)) => break error.to_string(),
                    None => break "the relay closed the connection".to_owned(),
                },
                Some(_) = streams.join_next(), if !streams.is_empty() => {}
                _ = keepalive.tick() => {
                    if heard.elapsed() >= IDLE_TIMEOUT {
                        break format!("nothing from the relay for {}s", IDLE_TIMEOUT.as_secs());
                    }
                    let ping = Message::Ping(Default::default());
                    if sink.lock().await.send(ping).await.is_err() {
                        break "the connection to the relay failed".to_owned();
                    }
                }
            }
        };
        active.clear();
        SessionEnd::Ended {
            reason,
            lasted: started.elapsed(),
        }
    }
}

/// The relays to try, in order: WebTransport's then WebSocket's for
/// [`Transport::Auto`] (the other way round while WebTransport is failing),
/// each carrier's shuffled.
fn order(pool: Pool, transport: Transport, websocket_first: bool) -> Vec<Relay> {
    use rand::seq::SliceRandom;
    let Pool {
        mut webtransport,
        mut websocket,
    } = pool;
    webtransport.shuffle(&mut rand::rng());
    websocket.shuffle(&mut rand::rng());
    match transport {
        Transport::WebTransport => webtransport,
        Transport::WebSocket => websocket,
        Transport::Auto if websocket_first => websocket.into_iter().chain(webtransport).collect(),
        Transport::Auto => webtransport.into_iter().chain(websocket).collect(),
    }
}

// ---------------------------------------------------------------------------
// The active session, for a graceful shutdown
// ---------------------------------------------------------------------------

type WsSink = futures_util::stream::SplitSink<Socket, Message>;

enum Session {
    WebTransport(Box<wt::Session>),
    WebSocket(Arc<tokio::sync::Mutex<WsSink>>),
}

#[derive(Clone, Default)]
struct Active(Arc<Mutex<Option<Session>>>);

impl Active {
    fn set(&self, session: Session) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(session);
    }

    fn clear(&self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
    }

    async fn close(&self) {
        let session = self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
        match session {
            Some(Session::WebTransport(session)) => {
                session.close(CODE_SHUTDOWN, b"uplink shutting down");
            }
            Some(Session::WebSocket(sink)) => {
                let frame = tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: CloseCode::Away,
                    reason: "uplink shutting down".into(),
                };
                let _ = tokio::time::timeout(Duration::from_secs(1), async {
                    sink.lock().await.send(Message::Close(Some(frame))).await
                })
                .await;
            }
            None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Control plane
// ---------------------------------------------------------------------------

struct Relay {
    /// Connection URL as minted by the control plane (fragment stripped).
    /// The URL is the session credential — never show it; use `label`.
    url: url::Url,
    /// `host:port`, for events.
    label: String,
    /// SHA-256 of the relay's certificate (DER), from the URL's `#sha256=`
    /// fragment; pins TLS verification instead of using system trust roots.
    cert_hash: Option<Vec<u8>>,
    carrier: Carrier,
}

#[derive(Default)]
struct Pool {
    webtransport: Vec<Relay>,
    websocket: Vec<Relay>,
}

enum FetchOutcome {
    Pool(Pool),
    Retry {
        after: Option<Duration>,
        reason: String,
    },
}

/// Query the control endpoint. `Err` is fatal (bad token); every other
/// failure is a retryable `FetchOutcome::Retry`.
async fn fetch_pool(
    http: &reqwest::Client,
    url: &url::Url,
    token: &str,
) -> Result<FetchOutcome, String> {
    let retry = |after, reason| Ok(FetchOutcome::Retry { after, reason });
    let response = match http
        .get(url.clone())
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/json")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return retry(None, format!("control endpoint unreachable: {error}")),
    };
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(format!(
            "control endpoint rejected YAS_UPLINK_TOKEN ({status})"
        ));
    }
    if !status.is_success() {
        let after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return retry(after, format!("control endpoint returned {status}"));
    }
    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => return retry(None, format!("error reading relay pool: {error}")),
    };
    match parse_pool(&body) {
        Ok(pool) => Ok(FetchOutcome::Pool(pool)),
        Err(error) => retry(None, format!("bad relay pool: {error}")),
    }
}

/// `relays` (WebTransport, `https`) and `websockets` (`wss`): either may be
/// absent or empty, not both.
fn parse_pool(body: &str) -> Result<Pool, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("invalid JSON: {error}"))?;
    let list = |name: &str| -> Result<Vec<String>, String> {
        match value.get(name) {
            None | Some(serde_json::Value::Null) => Ok(Vec::new()),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("\"{name}\" entries must be URL strings"))
                })
                .collect(),
            Some(_) => Err(format!("\"{name}\" must be an array")),
        }
    };
    let pool = Pool {
        webtransport: list("relays")?
            .iter()
            .map(|url| parse_relay(url, Carrier::WebTransport))
            .collect::<Result<_, _>>()?,
        websocket: list("websockets")?
            .iter()
            .map(|url| parse_relay(url, Carrier::WebSocket))
            .collect::<Result<_, _>>()?,
    };
    if pool.webtransport.is_empty() && pool.websocket.is_empty() {
        return Err("the pool names no relay (\"relays\" and \"websockets\" are empty)".into());
    }
    Ok(pool)
}

fn parse_relay(value: &str, carrier: Carrier) -> Result<Relay, String> {
    let mut url = url::Url::parse(value).map_err(|error| format!("bad relay URL: {error}"))?;
    let scheme = match carrier {
        Carrier::WebTransport => "https",
        Carrier::WebSocket => "wss",
    };
    if url.scheme() != scheme {
        return Err(format!(
            "{carrier} relay URL scheme must be {scheme}, got {}",
            url.scheme()
        ));
    }
    if url.host_str().is_none() {
        return Err("relay URL has no host".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("relay URL must not have userinfo".into());
    }
    let label = label(&url);
    // A malformed pin must be an error, never a silent fall-back to system
    // roots — that would defeat the pinning.
    let cert_hash = match url.fragment() {
        None | Some("") => None,
        Some(fragment) => {
            let hash = fragment
                .strip_prefix("sha256=")
                .ok_or("relay URL fragment must be sha256=<base64url hash>")?;
            let bytes =
                base64url_decode(hash).ok_or("relay certificate pin is not valid base64url")?;
            if bytes.len() != 32 {
                return Err("relay certificate pin must be a SHA-256 (32 bytes)".into());
            }
            Some(bytes)
        }
    };
    // Fragments are client-side only; strip before connecting.
    url.set_fragment(None);
    Ok(Relay {
        url,
        label,
        cert_hash,
        carrier,
    })
}

/// `host:port`: what may be shown of a URL that is a credential.
fn label(url: &url::Url) -> String {
    let host = match url.host() {
        Some(url::Host::Ipv6(address)) => format!("[{address}]"),
        Some(host) => host.to_string(),
        None => String::new(),
    };
    format!("{host}:{}", url.port_or_known_default().unwrap_or(443))
}

fn base64url_decode(value: &str) -> Option<Vec<u8>> {
    let value = value.trim_end_matches('=');
    let mut out = Vec::with_capacity(value.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in value.bytes() {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(digit);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // Leftover bits must be padding zeros of a valid encoding.
    if bits > 0 && (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

fn jittered(base: Duration) -> Duration {
    use rand::RngExt as _;
    let ms = base.as_millis().max(1) as u64;
    // 0.75x–1.25x
    Duration::from_millis(rand::rng().random_range(ms * 3 / 4..=ms * 5 / 4))
}

// ---------------------------------------------------------------------------
// Relay sessions
// ---------------------------------------------------------------------------

enum SessionEnd {
    /// Handshake or CONNECT failed — try the next relay in the pool.
    NeverConnected(String),
    /// The session was established and later died — re-query the control
    /// plane for a fresh pool.
    Ended { reason: String, lasted: Duration },
}

/// A WebTransport client with the liveness settings of docs/uplink.md (10 s
/// keepalive, 30 s idle timeout), on YAS's rustls provider, verifying the
/// relay with the platform's roots or a certificate pin.
fn webtransport_client(cert_hash: Option<&[u8]>) -> Result<wt::Client, String> {
    let provider = yas_webrtc_forwarder::tls::provider();
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| format!("TLS config: {error}"))?;
    let mut crypto = match cert_hash {
        Some(hash) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::CertificateHash {
                provider,
                hash: hash.to_vec(),
            }))
            .with_no_client_auth(),
        None => builder
            .with_root_certificates(yas_webrtc_forwarder::tls::native_roots())
            .with_no_client_auth(),
    };
    crypto.alpn_protocols = vec![wt::ALPN.as_bytes().to_vec()];
    let crypto = wt::quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|error| format!("QUIC TLS config: {error}"))?;
    let mut config = wt::quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = wt::quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(KEEPALIVE));
    transport.max_idle_timeout(Some(
        wt::quinn::IdleTimeout::try_from(IDLE_TIMEOUT).expect("30s fits in an idle timeout"),
    ));
    config.transport_config(Arc::new(transport));
    let endpoint = wt::quinn::Endpoint::client((std::net::Ipv6Addr::UNSPECIFIED, 0).into())
        .or_else(|_| wt::quinn::Endpoint::client((std::net::Ipv4Addr::UNSPECIFIED, 0).into()))
        .map_err(|error| format!("UDP socket: {error}"))?;
    Ok(wt::Client::new(endpoint, config))
}

/// A WebSocket to `url` (`wss`, a session's or a stream's) speaking
/// [`WEBSOCKET_SUBPROTOCOL`], TLS verified by the platform's roots or a pin.
async fn connect_websocket(url: &url::Url, cert_hash: Option<&[u8]>) -> Result<Socket, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| "bad WebSocket URL".to_owned())?;
    request.headers_mut().insert(
        "sec-websocket-protocol",
        tokio_tungstenite::tungstenite::http::HeaderValue::from_static(WEBSOCKET_SUBPROTOCOL),
    );
    let connector = match cert_hash {
        Some(hash) => {
            let provider = yas_webrtc_forwarder::tls::provider();
            let config = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .map_err(|error| format!("TLS config: {error}"))?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(crate::CertificateHash {
                    provider,
                    hash: hash.to_vec(),
                }))
                .with_no_client_auth();
            tokio_tungstenite::Connector::Rustls(Arc::new(config))
        }
        None => yas_webrtc_forwarder::tls::websocket_connector(),
    };
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_WEBSOCKET_MESSAGE))
        .max_frame_size(Some(MAX_WEBSOCKET_MESSAGE));
    let connecting = tokio_tungstenite::connect_async_tls_with_config(
        request,
        Some(config),
        true,
        Some(connector),
    );
    // Errors may quote the URL, a credential: say what failed, not where.
    let (socket, response) = match tokio::time::timeout(WEBSOCKET_CONNECT, connecting).await {
        Err(_) => {
            return Err(format!(
                "no WebSocket within {}s",
                WEBSOCKET_CONNECT.as_secs()
            ));
        }
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
            return Err(format!("the relay answered HTTP {}", response.status()));
        }
        // tungstenite checks the answer's subprotocol against the one offered.
        Ok(Err(tokio_tungstenite::tungstenite::Error::Protocol(
            tokio_tungstenite::tungstenite::error::ProtocolError::SecWebSocketSubProtocolError(_),
        ))) => {
            return Err(format!("the relay doesn't speak {WEBSOCKET_SUBPROTOCOL}"));
        }
        Ok(Err(error)) => return Err(websocket_error(&error)),
        Ok(Ok(connected)) => connected,
    };
    let selected = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok());
    if selected != Some(WEBSOCKET_SUBPROTOCOL) {
        return Err(format!("the relay doesn't speak {WEBSOCKET_SUBPROTOCOL}"));
    }
    Ok(socket)
}

fn websocket_error(error: &tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Io(error) => format!("connect failed: {error}"),
        Error::Tls(error) => format!("TLS: {error}"),
        Error::Url(_) => "bad WebSocket URL".to_owned(),
        Error::Protocol(error) => format!("WebSocket handshake: {error}"),
        other => format!("WebSocket: {other}"),
    }
}

/// The stream URL of a relay's request (`{"stream": "wss://…"}`), when it is
/// one: a `wss` URL on the session's own origin (so the same TLS trust
/// applies), without userinfo or a fragment. Anything else is ignored, for
/// relays newer than this producer.
fn stream_request(text: &str, session: &url::Url) -> Option<url::Url> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let stream = url::Url::parse(value.get("stream")?.as_str()?).ok()?;
    (stream.scheme() == "wss"
        && stream.host() == session.host()
        && stream.port_or_known_default() == session.port_or_known_default()
        && stream.username().is_empty()
        && stream.password().is_none()
        && stream.fragment().is_none())
    .then_some(stream)
}

/// One consumer on a WebSocket session: open the stream the relay asked for,
/// then serve it as a WebTransport stream is served, without datagrams.
async fn websocket_stream(
    url: url::Url,
    cert_hash: Option<Vec<u8>>,
    shared: Arc<Shared>,
    permit: tokio::sync::OwnedSemaphorePermit,
    local: Local,
    events: Events,
) {
    let socket = match connect_websocket(&url, cert_hash.as_deref()).await {
        Ok(socket) => socket,
        Err(error) => {
            events(&Event::StreamFailed { error });
            return;
        }
    };
    let (sink, source) = socket.split();
    // Opaque chunks of Noise records both ways. A stream's write side
    // finishing doesn't close the WebSocket (Noise carries the half-close).
    let relay = tokio::io::join(
        crate::WsFrameReader {
            inner: source,
            buf: bytes::Bytes::new(),
            frame_lengths: false,
        },
        crate::WsByteWriter { inner: sink },
    );
    bridge(relay, Lane::None, shared, permit, local, events).await;
}

async fn distribute_datagrams(session: wt::Session, routes: DatagramRoutes) {
    while let Ok(bytes) = session.read_datagram().await {
        let _ = routes.route(&bytes);
    }
    routes.clear();
}

// ---------------------------------------------------------------------------
// Bridging a consumer to the local server
// ---------------------------------------------------------------------------

/// Where a consumer's datagrams go: a WebTransport session's datagrams, or
/// nowhere (WebSocket sessions carry none).
enum Lane {
    WebTransport {
        session: Box<wt::Session>,
        routes: DatagramRoutes,
    },
    None,
}

/// Bridge one consumer stream to one local YAS server connection.
/// Authentication precedes both ingress classification and local IPC. The
/// relay sees only Noise records; plaintext selectors cannot bypass admission.
async fn bridge<S>(
    relay: S,
    lane: Lane,
    shared: Arc<Shared>,
    permit: tokio::sync::OwnedSemaphorePermit,
    local: Local,
    events: Events,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let relay = match yas_uplink::accept(relay, shared.crypto()).await {
        Ok(relay) => relay,
        Err(_) => return,
    };
    // Bind each sideband's keys to its authenticated main stream. The random
    // route token remains authenticated as AEAD AAD on every datagram.
    let material = relay.datagram_key_material();
    let ingress = tokio::time::timeout(
        Duration::from_secs(5),
        yas_composite_transport::classify(relay),
    )
    .await;
    drop(permit);
    match ingress {
        Ok(Ok(yas_composite_transport::Ingress::Direct(relay))) => {
            bridge_direct(relay, &local, &events).await
        }
        Ok(Ok(yas_composite_transport::Ingress::Composite { offer, stream }))
            if offer.role == yas_composite_transport::Role::Main =>
        {
            let Lane::WebTransport { session, routes } = lane else {
                events(&Event::StreamFailed {
                    error: "rejected a datagram lane: a WebSocket session carries no datagrams"
                        .into(),
                });
                return;
            };
            let (sender, receiver) = yas_uplink::datagram_pair(material, offer.token, false);
            let lane = Sideband {
                session: *session,
                routes,
                sender,
                receiver,
            };
            bridge_composite(lane, offer, stream, &local, &events).await;
        }
        _ => {}
    }
}

async fn bridge_direct<S>(relay: S, local: &Local, events: &Events)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sock_read, mut sock_write) = match local.connect().await {
        Ok(halves) => halves,
        Err(error) => {
            events(&Event::LocalUnavailable {
                local: local.label.clone(),
                error,
            });
            return;
        }
    };
    let (mut recv, mut send) = tokio::io::split(relay);
    let down = async move {
        tokio::io::copy(&mut recv, &mut sock_write).await?;
        sock_write.shutdown().await
    };
    let up = async move {
        tokio::io::copy(&mut sock_read, &mut send).await?;
        send.shutdown().await
    };
    let _ = tokio::try_join!(down, up);
}

/// A consumer's datagram lane on a WebTransport session.
struct Sideband {
    session: wt::Session,
    routes: DatagramRoutes,
    sender: yas_uplink::DatagramSender,
    receiver: yas_uplink::DatagramReceiver,
}

async fn bridge_composite<S>(
    lane: Sideband,
    offer: yas_composite_transport::Offer,
    relay: S,
    local: &Local,
    events: &Events,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Sideband {
        session,
        routes,
        mut sender,
        mut receiver,
    } = lane;
    let physical_maximum = session
        .max_datagram_size()
        .saturating_sub(yas_composite_transport::ROUTED_DATAGRAM_HEADER)
        .saturating_sub(yas_uplink::DATAGRAM_OVERHEAD)
        .min(yas_composite_transport::HARD_MAX_DATAGRAM as usize - yas_uplink::DATAGRAM_OVERHEAD);
    if offer.max_datagram as usize > physical_maximum {
        events(&Event::StreamFailed {
            error: format!(
                "rejected composite maximum {} above path maximum {physical_maximum}",
                offer.max_datagram
            ),
        });
        return;
    }
    let (mut main_read, mut main_write) = match local.connect().await {
        Ok(halves) => halves,
        Err(error) => {
            events(&Event::LocalUnavailable {
                local: local.label.clone(),
                error,
            });
            return;
        }
    };
    let (mut side_read, mut side_write) = match local.connect().await {
        Ok(halves) => halves,
        Err(error) => {
            events(&Event::LocalUnavailable {
                local: format!("{} (datagram sideband)", local.label),
                error,
            });
            return;
        }
    };
    let main_offer = yas_composite_transport::Offer::new(
        yas_composite_transport::Role::Main,
        offer.token,
        offer.max_datagram,
    )
    .expect("the classified offer is valid");
    let side_offer = yas_composite_transport::Offer::new(
        yas_composite_transport::Role::Datagram,
        offer.token,
        offer.max_datagram,
    )
    .expect("the classified offer is valid");
    if yas_composite_transport::write_offer(&mut main_write, main_offer)
        .await
        .is_err()
        || yas_composite_transport::write_offer(&mut side_write, side_offer)
            .await
            .is_err()
    {
        return;
    }

    let encrypted_maximum = offer.max_datagram + yas_uplink::DATAGRAM_OVERHEAD as u32;
    let Ok(mut route_rx) = routes.register(offer.token, encrypted_maximum, DATAGRAM_QUEUE) else {
        return;
    };

    let (mut relay_read, mut relay_write) = tokio::io::split(relay);
    let down = async {
        tokio::io::copy(&mut relay_read, &mut main_write).await?;
        main_write.shutdown().await
    };
    let up = async {
        tokio::io::copy(&mut main_read, &mut relay_write).await?;
        relay_write.shutdown().await
    };
    let side_routes = routes.clone();
    let side_token = offer.token;
    let side_task = tokio::spawn(async move {
        let side_out = async {
            loop {
                let Ok(frame) =
                    yas_composite_transport::read_datagram(&mut side_read, offer.max_datagram)
                        .await
                else {
                    break;
                };
                let Some(encrypted) = sender.seal(&frame) else {
                    break;
                };
                let Ok(routed) = yas_composite_transport::encode_routed_datagram(
                    offer.token,
                    &encrypted,
                    encrypted_maximum,
                ) else {
                    break;
                };
                // Congestion is ordinary datagram loss; never await reliable
                // transport capacity or fall back to the main stream here.
                let _ = session.send_datagram(routed.into());
            }
        };
        let side_in = async {
            while let Some(frame) = route_rx.recv().await {
                let Some(frame) = receiver.open(&frame) else {
                    continue;
                };
                if yas_composite_transport::write_datagram(
                    &mut side_write,
                    &frame,
                    offer.max_datagram,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
        };
        tokio::select! {
            _ = side_out => {}
            _ = side_in => {}
        }
        side_routes.remove(side_token);
    });

    // The authoritative reliable splice owns the connection lifetime. An
    // optional sideband ending only removes its route and cannot drop `main`.
    let _ = tokio::try_join!(down, up);
    routes.remove(offer.token);
    side_task.abort();
}

#[cfg(test)]
#[path = "uplink_producer_tests.rs"]
mod tests;
