// Shared by the uplink E2E test and the browser fixture, which use different parts.
#![allow(dead_code)]

use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::Command,
    sync::{mpsc, oneshot},
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{Request, Response},
};
use web_transport_quinn as wt;

/// The WebSocket relay protocol's subprotocol (docs/uplink.md).
const WEBSOCKET_SUBPROTOCOL: &str = "yas-uplink.v1";

pub fn cli(binary: &Path, directory: &Path, ca: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .kill_on_drop(true)
        .env("SSL_CERT_FILE", ca)
        .env("SSL_CERT_DIR", directory.join("empty-certs"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .env("XDG_CACHE_HOME", directory.join("cache"))
        .env("XDG_CONFIG_HOME", directory.join("config"))
        .env("YAS_SOCK", directory.join("yas.sock"))
        .env("YAS_KV_PATH", directory.join("state/kv.redb"))
        .env(
            "YAS_EXTENSION_PATH",
            directory.join("state/extensions.redb"),
        )
        .env("YAS_PROXY", "0")
        // Fixture edges use WebSocket; the uplink relay owns its QUIC listener.
        .env("YAS_EDGE", "0")
        .env("YAS_SHARE", "0")
        .env("YAS_WEBTRANSPORT", "0")
        .env("YAS_SKIP_COMPOSITOR", "1")
        .env("YAS_AUDIO", "0")
        .env("YAS_FONTS", "0")
        .env("YAS_RELAY", "0")
        .env("YAS_EXT", "0")
        .env("YAS_CHANNEL", "0")
        .env("YAS_REMOTES", directory.join("remotes"))
        .env_remove("YAS_TARGET")
        .env_remove("YAS_UPLINK_TRANSPORT")
        .stdin(Stdio::null());
    command
}

/// What carries the producer's relay session in a fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carrier {
    /// A pool of WebTransport relays only, as older control endpoints give.
    WebTransport,
    /// A pool of both kinds, the producer forced onto WebSockets.
    WebSocket,
    /// A pool of both kinds whose WebTransport relay's UDP goes nowhere: the
    /// producer (on its default transport) must fall back to WebSockets.
    Fallback,
}

/// The producer's relay session, as the relay holds it.
#[derive(Clone)]
enum ProducerLink {
    WebTransport(Box<wt::Session>),
    /// Stream requests to send on the session WebSocket.
    WebSocket(mpsc::UnboundedSender<Message>),
}

type StreamSocket =
    tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;

#[derive(Default)]
struct RelayState {
    producer: Option<ProducerLink>,
    next_stream: u64,
    streams: HashMap<String, oneshot::Sender<StreamSocket>>,
}

pub struct Fixture {
    pub directory: tempfile::TempDir,
    pub ca: std::path::PathBuf,
    pub uri: String,
    pub producer_public: yas_uplink::PublicKey,
    pub consumer_key: String,
    pub capture: Arc<Mutex<Vec<u8>>>,
    pub requests: Arc<Mutex<Vec<&'static str>>>,
    pub producer: tokio::process::Child,
    /// What the producer printed on stderr (it goes to the test's stderr too).
    pub producer_log: Arc<Mutex<Vec<String>>>,
    _server: tokio::process::Child,
    _black_hole: Option<tokio::net::UdpSocket>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Fixture {
    pub async fn start(binary: &Path) -> Self {
        Self::start_with(binary, Carrier::WebTransport).await
    }

    pub async fn start_with(binary: &Path, carrier: Carrier) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for name in ["state", "cache", "config", "empty-certs"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let ca = root.join("ca.pem");
        std::fs::write(&ca, cert.cert.pem()).unwrap();
        let key =
            || rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into();
        let tls = TlsAcceptor::from(Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.cert.der().clone()], key())
                .unwrap(),
        ));
        let mut worker = wt::ServerBuilder::new()
            .with_addr("127.0.0.1:0".parse().unwrap())
            .with_certificate(vec![cert.cert.der().clone()], key())
            .unwrap();
        // A UDP socket that never answers stands for a network dropping UDP.
        let black_hole = (carrier == Carrier::Fallback)
            .then(|| std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
            .map(|socket| {
                socket.set_nonblocking(true).unwrap();
                tokio::net::UdpSocket::from_std(socket).unwrap()
            });
        let relay_url = match &black_hole {
            Some(hole) => format!(
                "https://127.0.0.1:{}/producer",
                hole.local_addr().unwrap().port()
            ),
            None => format!(
                "https://127.0.0.1:{}/producer",
                worker.local_addr().unwrap().port()
            ),
        };
        let control = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_url = format!("https://{}", control.local_addr().unwrap());
        let websocket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let websocket_origin = format!("wss://{}", websocket.local_addr().unwrap());
        let ws_url = format!("{websocket_origin}/consumer");
        let pool = match carrier {
            Carrier::WebTransport => serde_json::json!({ "relays": [relay_url] }),
            Carrier::WebSocket | Carrier::Fallback => serde_json::json!({
                "relays": [relay_url],
                "websockets": [format!("{websocket_origin}/producer")],
            }),
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let control_tls = tls.clone();
        let control_task = tokio::spawn(async move {
            loop {
                let (stream, _) = control.accept().await.unwrap();
                let tls = control_tls.clone();
                let pool = pool.to_string();
                let ws_url = ws_url.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = tls.accept(stream).await else {
                        return;
                    };
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(stream.read_u8().await.unwrap());
                        assert!(header.len() < 16384);
                    }
                    let header = String::from_utf8(header).unwrap().to_ascii_lowercase();
                    let (status, body) = if header.starts_with("get /allocate ")
                        && header.contains("authorization: bearer producer-token\r\n")
                    {
                        seen.lock().unwrap().push("allocate");
                        ("200 OK", pool)
                    } else if header.starts_with("get /attach ")
                        && header.contains("authorization: bearer consumer-token\r\n")
                    {
                        seen.lock().unwrap().push("attach");
                        ("200 OK", serde_json::json!({"ws": ws_url}).to_string())
                    } else {
                        seen.lock().unwrap().push("rejected");
                        ("403 Forbidden", "{}".into())
                    };
                    stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                });
            }
        });

        let (producer_key, producer_public) = yas_uplink::Identity::generate().unwrap();
        // Base64url keys can begin with '-': exercise the documented spaced
        // --allow-client argument, not the equals-form workaround.
        let (consumer_key, consumer_public) = (0..4096)
            .map(|_| yas_uplink::Identity::generate().unwrap())
            .find(|(_, public)| public.to_string().starts_with('-'))
            .expect("generate a hyphen-prefixed client public key");
        let mut server = cli(binary, root, &ca)
            .args(["server", "--name", "uplink-e2e", "--socket"])
            .arg(root.join("yas.sock"))
            .arg("--no-persistent-extensions")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        while tokio::net::UnixStream::connect(root.join("yas.sock"))
            .await
            .is_err()
        {
            assert!(
                server.try_wait().unwrap().is_none(),
                "isolated YAS server exited"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let state = Arc::new(Mutex::new(RelayState::default()));
        let connected = Arc::new(tokio::sync::Notify::new());
        let capture = Arc::new(Mutex::new(Vec::new()));
        let relay_task = tokio::spawn(serve_websockets(
            websocket,
            tls,
            websocket_origin,
            state.clone(),
            connected.clone(),
            capture.clone(),
        ));

        let mut command = cli(binary, root, &ca);
        command
            .env("YAS_UPLINK_IDENTITY", &producer_key)
            .env("YAS_UPLINK_TOKEN", "producer-token")
            .args([
                "uplink",
                &format!("{control_url}/allocate"),
                "--allow-client",
                &consumer_public.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if carrier == Carrier::WebSocket {
            command.env("YAS_UPLINK_TRANSPORT", "websocket");
        }
        let mut producer = command.spawn().unwrap();
        let producer_log = Arc::new(Mutex::new(Vec::new()));
        let log = producer_log.clone();
        let stderr = producer.stderr.take().unwrap();
        let log_task = tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("{line}");
                log.lock().unwrap().push(line);
            }
        });
        match carrier {
            Carrier::WebTransport => {
                let session = tokio::select! {
                    session = async { worker.accept().await.unwrap().ok().await.unwrap() } => session,
                    status = producer.wait() => panic!("uplink producer exited before connecting: {status:?}"),
                };
                state.lock().unwrap().producer =
                    Some(ProducerLink::WebTransport(Box::new(session)));
            }
            Carrier::WebSocket | Carrier::Fallback => {
                let waiting = async {
                    loop {
                        let notified = connected.notified();
                        if state.lock().unwrap().producer.is_some() {
                            break;
                        }
                        notified.await;
                    }
                };
                tokio::select! {
                    () = waiting => {}
                    status = producer.wait() => panic!("uplink producer exited before connecting: {status:?}"),
                }
            }
        }
        // Invoke the documented URL generator; the consumer resolves /attach
        // and uses the normal WSS connector, with no test-only transport hooks.
        let generated = cli(binary, root, &ca)
            .env("YAS_UPLINK_IDENTITY", &producer_key)
            .args([
                "uplink-url",
                &control_url,
                "--client-token",
                "consumer-token",
            ])
            .output()
            .await
            .unwrap();
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let uri = String::from_utf8(generated.stdout)
            .unwrap()
            .trim()
            .to_owned();

        Self {
            directory,
            ca,
            uri,
            producer_public,
            consumer_key: consumer_key.to_string(),
            capture,
            requests,
            producer,
            producer_log,
            _server: server,
            _black_hole: black_hole,
            tasks: vec![control_task, relay_task, log_task],
        }
    }
}

/// The relay's WebSocket side: consumers (`/consumer`), the producer's session
/// (`/producer`) and the stream WebSockets it opens (`/stream/<n>`).
async fn serve_websockets(
    listener: TcpListener,
    tls: TlsAcceptor,
    origin: String,
    state: Arc<Mutex<RelayState>>,
    connected: Arc<tokio::sync::Notify>,
    capture: Arc<Mutex<Vec<u8>>>,
) {
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let tls = tls.clone();
        let origin = origin.clone();
        let state = state.clone();
        let connected = connected.clone();
        let capture = capture.clone();
        tokio::spawn(async move {
            let Ok(stream) = tls.accept(stream).await else {
                return;
            };
            let path = Arc::new(Mutex::new(String::new()));
            let seen = path.clone();
            // tungstenite's callback signature: its error is an HTTP response.
            #[allow(clippy::result_large_err)]
            let callback = move |request: &Request, mut response: Response| {
                *seen.lock().unwrap() = request.uri().path().to_owned();
                let offered = request
                    .headers()
                    .get("sec-websocket-protocol")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                if offered
                    .split(',')
                    .any(|each| each.trim() == WEBSOCKET_SUBPROTOCOL)
                {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        WEBSOCKET_SUBPROTOCOL.parse().unwrap(),
                    );
                }
                Ok(response)
            };
            let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                return;
            };
            let path = path.lock().unwrap().clone();
            if path == "/producer" {
                let (mut sink, mut source) = ws.split();
                let (requests, mut pending) = mpsc::unbounded_channel();
                state.lock().unwrap().producer = Some(ProducerLink::WebSocket(requests));
                connected.notify_waiters();
                loop {
                    tokio::select! {
                        request = pending.recv() => match request {
                            Some(message) => if sink.send(message).await.is_err() { break },
                            None => break,
                        },
                        message = source.next() => match message {
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                    }
                }
            } else if let Some(id) = path.strip_prefix("/stream/") {
                let waiting = state.lock().unwrap().streams.remove(id);
                if let Some(waiting) = waiting {
                    let _ = waiting.send(ws);
                }
            } else if path == "/consumer" {
                serve_consumer(ws, &origin, &state, &capture).await;
            }
        });
    }
}

async fn serve_consumer(
    mut ws: StreamSocket,
    origin: &str,
    state: &Arc<Mutex<RelayState>>,
    captured: &Arc<Mutex<Vec<u8>>>,
) {
    assert_eq!(
        ws.next().await.unwrap().unwrap(),
        Message::Text("consumer-token".into())
    );
    ws.send(Message::Text("ok".into())).await.unwrap();
    let link = state.lock().unwrap().producer.clone().expect("a producer");
    let (mut recv, mut send): (
        Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    ) = match link {
        ProducerLink::WebTransport(session) => {
            let (send, recv) = session.open_bi().await.unwrap();
            (Box::new(recv), Box::new(send))
        }
        ProducerLink::WebSocket(requests) => {
            let (tx, rx) = oneshot::channel();
            let id = {
                let mut state = state.lock().unwrap();
                state.next_stream += 1;
                let id = state.next_stream.to_string();
                state.streams.insert(id.clone(), tx);
                id
            };
            let url = format!("{origin}/stream/{id}");
            requests
                .send(Message::Text(
                    serde_json::json!({ "stream": url }).to_string().into(),
                ))
                .unwrap();
            let stream = tokio::time::timeout(Duration::from_secs(10), rx)
                .await
                .expect("the producer opened no stream WebSocket")
                .unwrap();
            let (ours, theirs) = tokio::io::duplex(128 << 10);
            tokio::spawn(pump(stream, ours));
            let (read, write) = tokio::io::split(theirs);
            (Box::new(read), Box::new(write))
        }
    };
    let (mut ws_send, mut ws_recv) = ws.split();
    let upstream = async {
        while let Some(Ok(Message::Binary(bytes))) = ws_recv.next().await {
            captured.lock().unwrap().extend_from_slice(&bytes);
            if send.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = send.shutdown().await;
    };
    let downstream = async {
        let mut buffer = [0; 16384];
        loop {
            match recv.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    captured.lock().unwrap().extend_from_slice(&buffer[..count]);
                    if ws_send
                        .send(Message::Binary(buffer[..count].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = ws_send.close().await;
    };
    tokio::join!(upstream, downstream);
}

/// Bytes between a stream WebSocket and a duplex pipe, as a relay forwards them.
async fn pump(socket: StreamSocket, pipe: tokio::io::DuplexStream) {
    let (mut sink, mut source) = socket.split();
    let (mut read, mut write) = tokio::io::split(pipe);
    let up = async {
        while let Some(Ok(message)) = source.next().await {
            match message {
                Message::Binary(bytes) => {
                    if write.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = write.shutdown().await;
    };
    let down = async {
        let mut buffer = vec![0; 16 << 10];
        loop {
            match read.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let chunk = buffer[..count].to_vec();
                    if sink.send(Message::Binary(chunk.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    };
    tokio::join!(up, down);
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.producer.start_kill();
        let _ = self._server.start_kill();
        for task in &self.tasks {
            task.abort();
        }
    }
}
