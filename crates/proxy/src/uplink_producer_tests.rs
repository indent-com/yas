use super::*;
use tokio::io::AsyncReadExt;

fn events() -> (Events, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let events: Events = Arc::new(move |event: &Event| {
        log.lock().unwrap().push(event.to_string());
    });
    (events, seen)
}

fn shared(crypto: Arc<yas_uplink::ServerConfig>) -> Arc<Shared> {
    Arc::new(Shared {
        token: Mutex::new("token".into()),
        crypto: Mutex::new(crypto),
    })
}

struct Keys {
    server: Arc<yas_uplink::ServerConfig>,
    server_public: yas_uplink::PublicKey,
    server_identity: yas_uplink::Identity,
    client: Arc<yas_uplink::ClientConfig>,
    client_public: yas_uplink::PublicKey,
    other: Arc<yas_uplink::ClientConfig>,
    other_public: yas_uplink::PublicKey,
}

fn keys() -> Keys {
    let (server_key, server_public) = yas_uplink::Identity::generate().unwrap();
    let (client_key, client_public) = yas_uplink::Identity::generate().unwrap();
    let (other_key, other_public) = yas_uplink::Identity::generate().unwrap();
    let server_identity = yas_uplink::Identity::from_base64(&server_key).unwrap();
    Keys {
        server: server_identity.server_config(vec![client_public]).unwrap(),
        server_public,
        server_identity,
        client: yas_uplink::Identity::from_base64(&client_key)
            .unwrap()
            .client_config(server_public)
            .unwrap(),
        client_public,
        other: yas_uplink::Identity::from_base64(&other_key)
            .unwrap()
            .client_config(server_public)
            .unwrap(),
        other_public,
    }
}

#[cfg(unix)]
fn local_socket(dir: &std::path::Path) -> (Local, tokio::net::UnixListener) {
    let socket = dir.join("local.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    (Local::socket(socket.to_str().unwrap()), listener)
}

#[cfg(unix)]
async fn nothing_accepted(listener: &tokio::net::UnixListener) -> bool {
    tokio::time::timeout(Duration::from_millis(25), listener.accept())
        .await
        .is_err()
}

#[cfg(unix)]
#[tokio::test]
async fn producer_authenticates_before_ipc_and_encrypts_datagrams() {
    use yas_composite_transport::{Offer, Role};
    yas_webrtc_forwarder::tls::install_default_provider();
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let keys = keys();
        let (local, listener) = local_socket(dir.path());
        let (events, _) = events();

        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let hash = wt::crypto::sha256(&yas_webrtc_forwarder::tls::provider(), cert.cert.der());
        let mut worker = wt::ServerBuilder::new()
            .with_addr("127.0.0.1:0".parse().unwrap())
            .with_certificate(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                    .into(),
            )
            .unwrap();
        let outer = webtransport_client(Some(hash.as_ref())).unwrap();
        let url: url::Url = format!("https://127.0.0.1:{}/", worker.local_addr().unwrap().port())
            .parse()
            .unwrap();
        let (connected, remote) = tokio::join!(outer.connect(url), async {
            worker.accept().await.unwrap().ok().await.unwrap()
        });
        let session = connected.unwrap();
        let routes = DatagramRoutes::new(16);
        let datagrams = tokio::spawn(distribute_datagrams(session.clone(), routes.clone()));

        for case in 0..4 {
            let lane = Lane::WebTransport {
                session: Box::new(session.clone()),
                routes: routes.clone(),
            };
            let shared = shared(keys.server.clone());
            let (local, events) = (local.clone(), events.clone());
            let accepting = session.clone();
            let producer = tokio::spawn(async move {
                let (send, recv) = accepting.accept_bi().await.unwrap();
                let permit = Arc::new(tokio::sync::Semaphore::new(1))
                    .acquire_owned()
                    .await
                    .unwrap();
                bridge(
                    tokio::io::join(recv, send),
                    lane,
                    shared,
                    permit,
                    local,
                    events,
                )
                .await;
            });
            let (send, recv) = remote.open_bi().await.unwrap();
            let mut stream = tokio::io::join(recv, send);
            if case == 0 {
                // A relay can synthesize protocol bytes, but those bytes must
                // never open a local socket before Noise authentication.
                stream
                    .write_all(yas_wire::PREFACE.as_slice())
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
                drop(stream);
                producer.await.unwrap();
                assert!(nothing_accepted(&listener).await);
                continue;
            }
            if case == 1 {
                assert!(
                    yas_uplink::connect(stream, keys.other.clone())
                        .await
                        .is_err()
                );
                producer.await.unwrap();
                assert!(nothing_accepted(&listener).await);
                continue;
            }
            let mut stream = yas_uplink::connect(stream, keys.client.clone())
                .await
                .unwrap();
            // Authentication alone also does not open IPC: the encrypted YAS
            // preface or composite selector must follow first.
            assert!(nothing_accepted(&listener).await);
            if case == 2 {
                stream
                    .write_all(yas_wire::PREFACE.as_slice())
                    .await
                    .unwrap();
                stream.write_all(b"command").await.unwrap();
                stream.flush().await.unwrap();
                let (mut ipc, _) = listener.accept().await.unwrap();
                let mut received = vec![0; yas_wire::PREFACE.len() + 7];
                ipc.read_exact(&mut received).await.unwrap();
                assert_eq!(
                    received,
                    [yas_wire::PREFACE.as_slice(), b"command"].concat()
                );
                ipc.write_all(b"reply").await.unwrap();
                ipc.shutdown().await.unwrap();
                let mut reply = Vec::new();
                stream.read_to_end(&mut reply).await.unwrap();
                assert_eq!(reply, b"reply");
                stream.shutdown().await.unwrap();
                producer.await.unwrap();
                continue;
            }

            let token = [0x35; 16];
            let maximum = 512;
            let material = stream.datagram_key_material();
            let (mut sender, mut receiver) = yas_uplink::datagram_pair(material, token, true);
            yas_composite_transport::write_offer(
                &mut stream,
                Offer::new(Role::Main, token, maximum).unwrap(),
            )
            .await
            .unwrap();
            stream.flush().await.unwrap();
            let (mut main, _) = listener.accept().await.unwrap();
            let (mut side, _) = listener.accept().await.unwrap();
            for (socket, expected) in [(&mut main, Role::Main), (&mut side, Role::Datagram)] {
                match yas_composite_transport::classify(socket).await.unwrap() {
                    yas_composite_transport::Ingress::Composite { offer, .. } => {
                        assert_eq!(offer.role, expected);
                        assert_eq!(offer.token, token);
                        assert_eq!(offer.max_datagram, maximum);
                    }
                    _ => panic!("missing local composite selector"),
                }
            }
            // A reliable reply also synchronizes route registration.
            main.write_all(b"ready").await.unwrap();
            let mut ready = [0; 5];
            stream.read_exact(&mut ready).await.unwrap();
            assert_eq!(&ready, b"ready");

            let encrypted = sender.seal(b"private datagram").unwrap();
            let wire_maximum = maximum + yas_uplink::DATAGRAM_OVERHEAD as u32;
            let mut forged = encrypted.clone();
            *forged.last_mut().unwrap() ^= 1;
            for bytes in [&forged, &encrypted, &encrypted] {
                let routed =
                    yas_composite_transport::encode_routed_datagram(token, bytes, wire_maximum)
                        .unwrap();
                remote.send_datagram(routed.into()).unwrap();
            }
            assert_eq!(
                yas_composite_transport::read_datagram(&mut side, maximum)
                    .await
                    .unwrap(),
                b"private datagram"
            );
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(50),
                    yas_composite_transport::read_datagram(&mut side, maximum)
                )
                .await
                .is_err()
            );

            yas_composite_transport::write_datagram(&mut side, b"private reply", maximum)
                .await
                .unwrap();
            let packet = remote.read_datagram().await.unwrap();
            let (received_token, ciphertext) =
                yas_composite_transport::split_routed_datagram(&packet).unwrap();
            assert_eq!(received_token, token);
            assert!(!ciphertext.windows(13).any(|w| w == b"private reply"));
            assert_eq!(receiver.open(ciphertext).unwrap(), b"private reply");
            stream.shutdown().await.unwrap();
            main.shutdown().await.unwrap();
            producer.await.unwrap();
        }
        datagrams.abort();
        session.close(0, b"test complete");
    })
    .await
    .expect("producer uplink test stalled");
}

/// A relay for WebSocket sessions, pinned by its certificate's hash: it
/// takes one session, then answers `/stream/…` WebSockets, handing each to the
/// test as a byte stream the consumer's side of Noise runs on.
#[cfg(unix)]
struct WebSocketRelay {
    url: url::Url,
    hash: Vec<u8>,
    /// Stream requests sent on the session: what the test writes there.
    requests: tokio::sync::mpsc::UnboundedSender<Message>,
    /// Stream WebSockets the producer opened, as duplex byte streams.
    streams: tokio::sync::mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>,
    /// What the relay offered as the session's subprotocol, and saw.
    session_protocols: Arc<Mutex<Vec<String>>>,
    _task: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl WebSocketRelay {
    async fn start() -> Self {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        yas_webrtc_forwarder::tls::install_default_provider();
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let hash = wt::crypto::sha256(&yas_webrtc_forwarder::tls::provider(), cert.cert.der())
            .as_ref()
            .to_vec();
        let tls = tokio_rustls::TlsAcceptor::from(Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                        .into(),
                )
                .unwrap(),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url: url::Url = format!(
            "wss://127.0.0.1:{}/session",
            listener.local_addr().unwrap().port()
        )
        .parse()
        .unwrap();
        let (requests, mut pending) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let (streams_tx, streams) = tokio::sync::mpsc::unbounded_channel();
        let session_protocols = Arc::new(Mutex::new(Vec::new()));
        let protocols = session_protocols.clone();
        let task = tokio::spawn(async move {
            let mut session_taken = false;
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let Ok(tls) = tls.accept(tcp).await else {
                    continue;
                };
                let path = Arc::new(Mutex::new(String::new()));
                let seen = path.clone();
                let offered = protocols.clone();
                // tungstenite's callback signature: its error is an HTTP response.
                #[allow(clippy::result_large_err)]
                let callback = move |request: &Request, mut response: Response| {
                    *seen.lock().unwrap() = request.uri().path().to_owned();
                    let protocol = request
                        .headers()
                        .get("sec-websocket-protocol")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    offered.lock().unwrap().push(protocol.clone());
                    if protocol
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
                let Ok(socket) = tokio_tungstenite::accept_hdr_async(tls, callback).await else {
                    continue;
                };
                let path = path.lock().unwrap().clone();
                if path == "/session" && !session_taken {
                    session_taken = true;
                    let (mut sink, mut source) = socket.split();
                    let mut requests =
                        std::mem::replace(&mut pending, tokio::sync::mpsc::unbounded_channel().1);
                    tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                request = requests.recv() => match request {
                                    Some(message) => {
                                        if sink.send(message).await.is_err() {
                                            break;
                                        }
                                    }
                                    None => break,
                                },
                                message = source.next() => match message {
                                    Some(Ok(_)) => {}
                                    _ => break,
                                },
                            }
                        }
                    });
                } else if let Some(stream) = path.strip_prefix("/stream/") {
                    let (ours, theirs) = tokio::io::duplex(128 << 10);
                    tokio::spawn(pump(socket, ours));
                    let _ = streams_tx.send((stream.to_owned(), theirs));
                }
            }
        });
        Self {
            url,
            hash,
            requests,
            streams,
            session_protocols,
            _task: task,
        }
    }

    fn relay(&self) -> Relay {
        Relay {
            url: self.url.clone(),
            label: label(&self.url),
            cert_hash: Some(self.hash.clone()),
            carrier: Carrier::WebSocket,
        }
    }

    fn ask(&self, stream: &str) {
        let url = format!(
            "wss://127.0.0.1:{}/stream/{stream}",
            self.url.port().unwrap()
        );
        self.requests
            .send(Message::text(
                serde_json::json!({ "stream": url }).to_string(),
            ))
            .unwrap();
    }

    async fn stream(&mut self) -> (String, tokio::io::DuplexStream) {
        tokio::time::timeout(Duration::from_secs(5), self.streams.recv())
            .await
            .expect("the producer opened no stream WebSocket")
            .unwrap()
    }
}

/// Bytes between a stream WebSocket and a duplex pipe, as a relay forwards them.
#[cfg(unix)]
async fn pump<S>(socket: tokio_tungstenite::WebSocketStream<S>, pipe: tokio::io::DuplexStream)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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
                    let chunk = bytes::Bytes::copy_from_slice(&buffer[..count]);
                    if sink.send(Message::Binary(chunk)).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    };
    tokio::join!(up, down);
}

#[cfg(unix)]
#[tokio::test]
async fn websocket_session_serves_consumers_and_takes_allowlist_changes() {
    use yas_composite_transport::{Offer, Role};
    tokio::time::timeout(Duration::from_secs(20), async {
        let dir = tempfile::tempdir().unwrap();
        let keys = keys();
        let (local, listener) = local_socket(dir.path());
        let mut relay = WebSocketRelay::start().await;
        let producer = Producer::new(
            "https://127.0.0.1:1/control",
            "token",
            keys.server.clone(),
            local,
        )
        .unwrap();
        let (events, seen) = events();
        let producer = Producer { events, ..producer };
        let handle = producer.handle();
        let active = Active::default();
        let session = {
            let (producer, active, relay) = (producer.clone(), active.clone(), relay.relay());
            tokio::spawn(async move { producer.websocket_session(&relay, &active).await })
        };
        // The session is up once it reports it.
        for _ in 0..200 {
            if seen
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.starts_with("connected"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            seen.lock().unwrap()[0],
            format!(
                "connected to relay 127.0.0.1:{} over WebSocket",
                relay.url.port().unwrap()
            )
        );
        assert_eq!(
            relay.session_protocols.lock().unwrap()[0],
            WEBSOCKET_SUBPROTOCOL
        );

        // A stream elsewhere than the session's origin is ignored.
        relay
            .requests
            .send(Message::text(
                r#"{"stream":"wss://127.0.0.2:1/stream/elsewhere"}"#.to_owned(),
            ))
            .unwrap();
        relay
            .requests
            .send(Message::text("{\"hello\":1}".to_owned()))
            .unwrap();

        // A consumer: authenticated, then its bytes reach the local server.
        relay.ask("first");
        let (name, stream) = relay.stream().await;
        assert_eq!(name, "first");
        assert_eq!(
            relay.session_protocols.lock().unwrap()[1],
            WEBSOCKET_SUBPROTOCOL
        );
        assert!(nothing_accepted(&listener).await);
        let mut first = yas_uplink::connect(stream, keys.client.clone())
            .await
            .unwrap();
        first.write_all(yas_wire::PREFACE.as_slice()).await.unwrap();
        first.write_all(b"command").await.unwrap();
        first.flush().await.unwrap();
        let (mut ipc, _) = listener.accept().await.unwrap();
        let mut received = vec![0; yas_wire::PREFACE.len() + 7];
        ipc.read_exact(&mut received).await.unwrap();
        assert_eq!(
            received,
            [yas_wire::PREFACE.as_slice(), b"command"].concat()
        );

        // A key it doesn't allow is refused before the local server hears of it.
        relay.ask("stranger");
        let (_, stream) = relay.stream().await;
        assert!(
            yas_uplink::connect(stream, keys.other.clone())
                .await
                .is_err()
        );
        assert!(nothing_accepted(&listener).await);

        // The allowlist changes while it runs: the other key only, now.
        handle.set_server_config(
            keys.server_identity
                .server_config(vec![keys.other_public])
                .unwrap(),
        );
        relay.ask("revoked");
        let (_, stream) = relay.stream().await;
        assert!(
            yas_uplink::connect(stream, keys.client.clone())
                .await
                .is_err()
        );
        relay.ask("added");
        let (_, stream) = relay.stream().await;
        let mut added = yas_uplink::connect(stream, keys.other.clone())
            .await
            .unwrap();
        added.write_all(yas_wire::PREFACE.as_slice()).await.unwrap();
        added.flush().await.unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        let mut preface = vec![0; yas_wire::PREFACE.len()];
        second.read_exact(&mut preface).await.unwrap();
        assert_eq!(preface, yas_wire::PREFACE.as_slice());
        drop(second);
        drop(added);
        let _ = keys.client_public;

        // The consumer connected before keeps its session, both ways,
        // including the local server finishing first (Noise's half-close).
        ipc.write_all(b"reply").await.unwrap();
        ipc.shutdown().await.unwrap();
        let mut reply = [0; 5];
        first.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"reply");
        first.write_all(b" and more").await.unwrap();
        first.flush().await.unwrap();
        let mut more = [0; 9];
        ipc.read_exact(&mut more).await.unwrap();
        assert_eq!(&more, b" and more");
        first.shutdown().await.unwrap();
        drop(ipc);

        // A datagram lane can't be carried: refused, nothing opened locally.
        relay.ask("composite");
        let (_, stream) = relay.stream().await;
        let mut composite = yas_uplink::connect(stream, keys.other.clone())
            .await
            .unwrap();
        yas_composite_transport::write_offer(
            &mut composite,
            Offer::new(Role::Main, [7; 16], 512).unwrap(),
        )
        .await
        .unwrap();
        composite.flush().await.unwrap();
        for _ in 0..200 {
            if seen
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("datagram lane"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(nothing_accepted(&listener).await);
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("a WebSocket session carries no datagrams"))
        );

        // Shutting down closes the session with a close frame.
        active.close().await;
        match tokio::time::timeout(Duration::from_secs(5), session).await {
            Ok(Ok(SessionEnd::Ended { reason, .. })) => {
                assert!(!reason.is_empty());
            }
            _ => panic!("the session didn't end"),
        }
        let _ = keys.server_public;
    })
    .await
    .expect("WebSocket session test stalled");
}

#[tokio::test]
async fn websocket_relay_must_speak_the_subprotocol() {
    // A WebSocket server that selects no subprotocol is not an uplink relay.
    yas_webrtc_forwarder::tls::install_default_provider();
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let hash = wt::crypto::sha256(&yas_webrtc_forwarder::tls::provider(), cert.cert.der())
        .as_ref()
        .to_vec();
    let tls = tokio_rustls::TlsAcceptor::from(Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                    .into(),
            )
            .unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = tls.accept(tcp).await.unwrap();
        let _ = tokio_tungstenite::accept_async(tls).await;
    });
    let url: url::Url = format!("wss://127.0.0.1:{port}/session").parse().unwrap();
    let error = connect_websocket(&url, Some(&hash)).await.err().unwrap();
    assert!(error.contains("doesn't speak yas-uplink.v1"), "{error}");
    // A wrong pin fails TLS, and the error never quotes the URL.
    let error = connect_websocket(&url, Some(&[0; 32])).await.err().unwrap();
    assert!(!error.contains("/session"), "{error}");
}

#[tokio::test]
async fn webtransport_gives_up_quickly_where_udp_goes_nowhere() {
    yas_webrtc_forwarder::tls::install_default_provider();
    // A UDP socket that never answers: what a network dropping UDP looks like.
    let hole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let url: url::Url = format!(
        "https://127.0.0.1:{}/t/x",
        hole.local_addr().unwrap().port()
    )
    .parse()
    .unwrap();
    let relay = Relay {
        label: label(&url),
        url,
        cert_hash: Some(vec![0; 32]),
        carrier: Carrier::WebTransport,
    };
    let keys = keys();
    let dir = tempfile::tempdir().unwrap();
    let producer = Producer::new(
        "https://127.0.0.1:1/control",
        "token",
        keys.server,
        Local::socket(dir.path().join("none").to_str().unwrap()),
    )
    .unwrap();
    let started = Instant::now();
    let end = producer
        .webtransport_session(&relay, Some(Duration::from_millis(300)), &Active::default())
        .await;
    match end {
        SessionEnd::NeverConnected(error) => assert!(error.contains("is UDP blocked?"), "{error}"),
        SessionEnd::Ended { .. } => panic!("a session through a black hole"),
    }
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn parse_pool_accepts_plain_and_pinned_relay_urls() {
    let pool = parse_pool(
        r#"{"relays":[
            "https://relay-1.indent.com:4443/t/kfV3aB",
            "https://[2001:db8::7]/session?key=x#sha256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        ],"ttl":60}"#,
    )
    .unwrap();
    assert_eq!(pool.webtransport.len(), 2);
    assert!(pool.websocket.is_empty());
    let relays = &pool.webtransport;
    assert_eq!(
        relays[0].url.as_str(),
        "https://relay-1.indent.com:4443/t/kfV3aB"
    );
    assert_eq!(relays[0].label, "relay-1.indent.com:4443");
    assert!(relays[0].cert_hash.is_none());
    assert_eq!(relays[1].label, "[2001:db8::7]:443");
    assert_eq!(relays[1].cert_hash.as_ref().unwrap().len(), 32);
    // The pin must be stripped before the URL is used to connect.
    assert_eq!(relays[1].url.fragment(), None);
    assert_eq!(relays[1].url.query(), Some("key=x"));
}

#[test]
fn parse_pool_takes_websocket_relays() {
    let pool = parse_pool(
        r#"{"relays":["https://relay.example:4433/t/a"],
            "websockets":["wss://relay.example/uplink/producer/b",
                          "wss://relay.example:8443/p#sha256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"]}"#,
    )
    .unwrap();
    assert_eq!(pool.webtransport.len(), 1);
    assert_eq!(pool.websocket.len(), 2);
    assert_eq!(pool.websocket[0].label, "relay.example:443");
    assert_eq!(pool.websocket[0].carrier, Carrier::WebSocket);
    assert_eq!(pool.websocket[1].cert_hash.as_ref().unwrap().len(), 32);
    assert_eq!(pool.websocket[1].url.fragment(), None);
    // WebSocket relays alone make a pool; so does an empty WebTransport list.
    let pool = parse_pool(r#"{"relays":[],"websockets":["wss://relay.example/p"]}"#).unwrap();
    assert!(pool.webtransport.is_empty());
    let pool = parse_pool(r#"{"websockets":["wss://relay.example/p"]}"#).unwrap();
    assert_eq!(pool.websocket.len(), 1);
}

#[test]
fn parse_pool_rejects_bad_input() {
    assert!(parse_pool("not json").is_err());
    assert!(parse_pool(r#"{"relays":[]}"#).is_err());
    assert!(parse_pool(r#"{"relays":[],"websockets":[]}"#).is_err());
    assert!(parse_pool(r#"{"relays":[{"host":"h"}]}"#).is_err());
    assert!(parse_pool(r#"{"relays":"https://relay.example/t"}"#).is_err());
    assert!(
        parse_relay("http://relay.example/t/x", Carrier::WebTransport).is_err(),
        "non-https scheme must be rejected"
    );
    assert!(
        parse_relay(
            "https://relay.example/t/x#sha256=AAAA",
            Carrier::WebTransport
        )
        .is_err(),
        "a pin of the wrong length must be rejected, not ignored"
    );
    assert!(
        parse_relay("https://relay.example/t/x#pin=abc", Carrier::WebTransport).is_err(),
        "an unrecognized fragment must be rejected, not ignored"
    );
    assert!(
        parse_relay("ws://relay.example/p", Carrier::WebSocket).is_err(),
        "a plaintext WebSocket relay must be rejected: its URL is a credential"
    );
    assert!(parse_relay("https://relay.example/p", Carrier::WebSocket).is_err());
    assert!(parse_relay("wss://user:pass@relay.example/p", Carrier::WebSocket).is_err());
    // One bad WebSocket relay spoils the pool, as a bad WebTransport one does.
    assert!(
        parse_pool(
            r#"{"relays":["https://relay.example/t"],"websockets":["ws://relay.example/p"]}"#
        )
        .is_err()
    );
}

fn pool(webtransport: usize, websocket: usize) -> Pool {
    let relay = |carrier, n| {
        let url = match carrier {
            Carrier::WebTransport => format!("https://wt{n}.example/t"),
            Carrier::WebSocket => format!("wss://ws{n}.example/p"),
        };
        parse_relay(&url, carrier).unwrap()
    };
    Pool {
        webtransport: (0..webtransport)
            .map(|n| relay(Carrier::WebTransport, n))
            .collect(),
        websocket: (0..websocket)
            .map(|n| relay(Carrier::WebSocket, n))
            .collect(),
    }
}

fn carriers(relays: &[Relay]) -> Vec<Carrier> {
    relays.iter().map(|relay| relay.carrier).collect()
}

#[test]
fn order_puts_webtransport_first_unless_it_is_failing() {
    use Carrier::{WebSocket as S, WebTransport as T};
    assert_eq!(
        carriers(&order(pool(2, 1), Transport::Auto, false)),
        [T, T, S]
    );
    assert_eq!(
        carriers(&order(pool(2, 1), Transport::Auto, true)),
        [S, T, T]
    );
    assert_eq!(
        carriers(&order(pool(2, 1), Transport::WebTransport, true)),
        [T, T]
    );
    assert_eq!(
        carriers(&order(pool(2, 1), Transport::WebSocket, false)),
        [S]
    );
    assert!(order(pool(0, 1), Transport::WebTransport, false).is_empty());
    assert!(order(pool(1, 0), Transport::WebSocket, false).is_empty());
}

#[test]
fn transport_parses() {
    assert_eq!("auto".parse::<Transport>().unwrap(), Transport::Auto);
    assert_eq!(
        " WebSocket ".parse::<Transport>().unwrap(),
        Transport::WebSocket
    );
    assert_eq!(
        "webtransport".parse::<Transport>().unwrap(),
        Transport::WebTransport
    );
    assert!("udp".parse::<Transport>().is_err());
    assert_eq!(Transport::WebSocket.to_string(), "websocket");
}

#[test]
fn stream_requests_stay_on_the_session_origin() {
    let session: url::Url = "wss://relay.example/uplink/producer/abc".parse().unwrap();
    let ask =
        |url: &str| stream_request(&serde_json::json!({ "stream": url }).to_string(), &session);
    assert_eq!(
        ask("wss://relay.example/uplink/stream/1").unwrap().as_str(),
        "wss://relay.example/uplink/stream/1"
    );
    assert!(ask("wss://relay.example:443/uplink/stream/1").is_some());
    assert!(ask("wss://relay.example:8443/uplink/stream/1").is_none());
    assert!(ask("wss://elsewhere.example/uplink/stream/1").is_none());
    assert!(ask("ws://relay.example/uplink/stream/1").is_none());
    assert!(ask("wss://user@relay.example/uplink/stream/1").is_none());
    assert!(ask("wss://relay.example/uplink/stream/1#x").is_none());
    assert!(stream_request("not json", &session).is_none());
    assert!(stream_request(r#"{"other":"wss://relay.example/x"}"#, &session).is_none());
}

#[test]
fn events_read_as_yas_uplink_prints_them() {
    let connected = |carrier| Event::Connected {
        relay: "relay.example:443".into(),
        carrier,
    };
    assert_eq!(
        connected(Carrier::WebTransport).to_string(),
        "connected to relay relay.example:443"
    );
    assert_eq!(
        connected(Carrier::WebSocket).to_string(),
        "connected to relay relay.example:443 over WebSocket"
    );
    assert_eq!(
        Event::PoolExhausted {
            retry_in: Duration::from_secs(4)
        }
        .to_string(),
        "relay pool exhausted; re-querying in 4s"
    );
}

#[test]
fn base64url_decodes() {
    assert_eq!(base64url_decode("aGVsbG8").unwrap(), b"hello");
    assert_eq!(base64url_decode("aGVsbG8=").unwrap(), b"hello");
    assert_eq!(base64url_decode("_-8").unwrap(), vec![0xff, 0xef]);
    assert!(base64url_decode("a+b").is_none());
    assert_eq!(base64url_decode("").unwrap(), Vec::<u8>::new());
}
