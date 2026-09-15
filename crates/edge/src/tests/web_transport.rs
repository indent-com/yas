#[path = "simulated_link.rs"]
mod simulated_link;

use super::*;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::sync::oneshot;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

struct Peer {
    session: web_transport_quinn::Session,
    send: web_transport_quinn::SendStream,
    recv: web_transport_quinn::RecvStream,
}

struct UdpRelay {
    blackhole: Arc<AtomicBool>,
    link: Option<simulated_link::Link>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for UdpRelay {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn udp_relay(server_addr: SocketAddr) -> (SocketAddr, UdpRelay) {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = socket.local_addr().unwrap();
    let blackhole = Arc::new(AtomicBool::new(false));
    let drop_packets = blackhole.clone();
    let task = tokio::spawn(async move {
        let mut client_addr = None;
        let mut buffer = vec![0; 65_536];
        loop {
            let (length, from) = socket.recv_from(&mut buffer).await.unwrap();
            if drop_packets.load(Ordering::SeqCst) {
                continue;
            }
            let destination = if from == server_addr {
                client_addr.unwrap()
            } else {
                client_addr = Some(from);
                server_addr
            };
            socket
                .send_to(&buffer[..length], destination)
                .await
                .unwrap();
        }
    });
    (
        relay_addr,
        UdpRelay {
            blackhole,
            link: None,
            task: Some(task),
        },
    )
}

async fn peers() -> (Peer, Peer, UdpRelay) {
    peers_with_link(Duration::ZERO, 0).await
}

async fn peers_with_link(delay: Duration, bytes_per_second: u64) -> (Peer, Peer, UdpRelay) {
    timeout(TEST_TIMEOUT, async {
        let identity = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let certificate = directory.path().join("certificate.pem");
        let private_key = directory.path().join("key.pem");
        std::fs::write(&certificate, identity.cert.pem()).unwrap();
        std::fs::write(&private_key, identity.signing_key.serialize_pem()).unwrap();
        let link = (!delay.is_zero()).then(|| simulated_link::Link::new(delay, bytes_per_second));
        let (mut server, _, _) = prepare_web_transport_with_endpoint(
            Some(WebTransportOptions {
                addr: "127.0.0.1:0".into(),
                public_port: 0,
                certificate: Some(certificate),
                private_key: Some(private_key),
                pin_certificate: false,
            }),
            |config, addr| match &link {
                Some(link) => link.server(config),
                None => web_transport_quinn::quinn::Endpoint::server(config, addr),
            },
        )
        .unwrap()
        .unwrap();
        let server_addr = server.local_addr().unwrap();
        let (relay_addr, relay) = if let Some(link) = &link {
            (
                server_addr,
                UdpRelay {
                    blackhole: link.blackhole.clone(),
                    link: Some(link.clone()),
                    task: None,
                },
            )
        } else {
            udp_relay(server_addr).await
        };
        let url: url::Url = format!("https://{relay_addr}/edge").parse().unwrap();
        // The default test client's 1.25 MB per-stream receive window is also
        // too small for a fast 200 ms path. Give this receiver ample credit.
        let mut roots = rustls::RootCertStore::empty();
        roots.add(identity.cert.der().clone()).unwrap();
        let mut tls = rustls::ClientConfig::builder_with_provider(
            web_transport_quinn::crypto::default_provider(),
        )
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];
        let crypto =
            web_transport_quinn::quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
        let mut config = web_transport_quinn::quinn::ClientConfig::new(Arc::new(crypto));
        let mut transport = web_transport_quinn::quinn::TransportConfig::default();
        transport
            .stream_receive_window((32u32 * 1024 * 1024).into())
            .receive_window((64u32 * 1024 * 1024).into());
        config.transport_config(Arc::new(transport));
        let endpoint = match &link {
            Some(link) => link.client().unwrap(),
            None => web_transport_quinn::quinn::Endpoint::client("127.0.0.1:0".parse().unwrap())
                .unwrap(),
        };
        let client = web_transport_quinn::Client::new(endpoint, config);
        let (client, edge) = tokio::join!(
            async {
                let session = client.connect(url).await.unwrap();
                let (mut send, recv) = session.open_bi().await.unwrap();
                // A QUIC stream is not visible to the peer until it sends data.
                send.write_all(&[0]).await.unwrap();
                Peer {
                    session,
                    send,
                    recv,
                }
            },
            async {
                let session = server.accept().await.unwrap().ok().await.unwrap();
                let (send, mut recv) = session.accept_bi().await.unwrap();
                assert_eq!(recv.read_u8().await.unwrap(), 0);
                Peer {
                    session,
                    send,
                    recv,
                }
            },
        );
        (client, edge, relay)
    })
    .await
    .expect("loopback WebTransport handshake")
}

fn start_bridge(
    edge: Peer,
    composite: bool,
    capacity: usize,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::io::DuplexStream,
    tokio::io::DuplexStream,
) {
    let (main, home) = tokio::io::duplex(capacity);
    let (main_reader, main_writer) = tokio::io::split(main);
    let (datagram, home_datagram) = tokio::io::duplex(4096);
    let (datagram_reader, datagram_writer) = tokio::io::split(datagram);
    let bridge = tokio::spawn(async move {
        if composite {
            bridge_composite_web_transport(
                edge.session,
                edge.recv,
                edge.send,
                CompositeHome {
                    main_reader: Box::new(main_reader),
                    main_writer: Box::new(main_writer),
                    datagram_reader: Box::new(datagram_reader),
                    datagram_writer: Box::new(datagram_writer),
                },
                1200,
            )
            .await;
        } else {
            bridge_reliable_web_transport(
                edge.session,
                edge.recv,
                edge.send,
                Box::new(main_reader),
                Box::new(main_writer),
            )
            .await;
        }
    });
    (bridge, home, home_datagram)
}

struct FailedDatagramWriter(Option<oneshot::Sender<()>>);

impl AsyncWrite for FailedDatagramWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Some(failed) = self.0.take() {
            let _ = failed.send(());
        }
        Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn datagram_failure_preserves_reliable_forwarding_and_eof_cleanup() {
    let (mut client, edge, _relay) = peers().await;
    let (main, mut home) = tokio::io::duplex(4096);
    let (main_reader, main_writer) = tokio::io::split(main);
    let (failed, failure) = oneshot::channel();
    let bridge = tokio::spawn(bridge_composite_web_transport(
        edge.session,
        edge.recv,
        edge.send,
        CompositeHome {
            main_reader: Box::new(main_reader),
            main_writer: Box::new(main_writer),
            datagram_reader: Box::new(tokio::io::empty()),
            datagram_writer: Box::new(FailedDatagramWriter(Some(failed))),
        },
        1200,
    ));

    client
        .session
        .send_datagram(b"lost".to_vec().into())
        .unwrap();
    timeout(TEST_TIMEOUT, failure).await.unwrap().unwrap();

    // Both optional directions have ended. The authoritative stream must
    // still forward bytes in both directions and notice its eventual EOF.
    client.send.write_all(b"request").await.unwrap();
    let mut request = [0; 7];
    timeout(TEST_TIMEOUT, home.read_exact(&mut request))
        .await
        .expect("reliable input stalled after datagram failure")
        .unwrap();
    assert_eq!(&request, b"request");
    home.write_all(b"reply").await.unwrap();
    let mut reply = [0; 5];
    timeout(TEST_TIMEOUT, client.recv.read_exact(&mut reply))
        .await
        .expect("reliable output stalled after datagram failure")
        .unwrap();
    assert_eq!(&reply, b"reply");

    client.send.shutdown().await.unwrap();
    timeout(TEST_TIMEOUT, bridge)
        .await
        .expect("stream EOF did not stop the bridge")
        .unwrap();
    assert_eq!(
        home.read_u8().await.unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
}

#[tokio::test]
async fn session_close_releases_backpressured_home_connections() {
    backpressured_disconnect(false).await;
}

#[tokio::test]
async fn silent_peer_timeout_releases_backpressured_home_connections() {
    backpressured_disconnect(true).await;
}

async fn backpressured_disconnect(blackhole: bool) {
    for composite in [false, true] {
        let (mut client, edge, relay) = peers().await;
        let (bridge, mut home, mut home_datagram) = start_bridge(edge, composite, 1);

        // Fill the copy buffer as well as the home buffer, so copy cannot
        // read ahead and discover EOF/the connection error while blocked.
        client.send.write_all(&vec![b'b'; 64 * 1024]).await.unwrap();
        client.send.finish().unwrap();
        timeout(TEST_TIMEOUT, client.send.stopped())
            .await
            .expect("peer did not acknowledge buffered stream data")
            .unwrap();
        assert_eq!(
            timeout(TEST_TIMEOUT, home.read_u8())
                .await
                .unwrap()
                .unwrap(),
            b'b'
        );
        // Leave the one-byte home buffer full. Stream copying cannot reach
        // the next QUIC read to discover that the browser has closed.
        let deadline = if blackhole {
            relay.blackhole.store(true, Ordering::SeqCst);
            tokio::time::pause();
            Duration::from_secs(32)
        } else {
            client.session.close(0, b"browser gone");
            TEST_TIMEOUT
        };
        timeout(deadline, bridge)
            .await
            .expect("closed session retained a backpressured home connection")
            .unwrap();
        timeout(TEST_TIMEOUT, home.read_to_end(&mut Vec::new()))
            .await
            .expect("home connection was not released")
            .unwrap();
        if composite {
            assert_eq!(
                timeout(TEST_TIMEOUT, home_datagram.read_u8())
                    .await
                    .unwrap()
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::UnexpectedEof
            );
        }
        if blackhole {
            tokio::time::resume();
        }
    }
}

#[tokio::test]
async fn silent_peer_timeout_is_not_extended_by_late_server_traffic() {
    for composite in [false, true] {
        let (mut client, edge, relay) = peers().await;
        let (bridge, mut home, home_datagram) = start_bridge(edge, composite, 4096);
        // Lose the optional path before the peer disappears, too.
        drop(home_datagram);

        // Let handshake housekeeping finish, then leave the client as the
        // last sender of application data.
        tokio::time::sleep(Duration::from_millis(100)).await;
        client.send.write_all(b"last input").await.unwrap();
        let mut input = [0; 10];
        timeout(TEST_TIMEOUT, home.read_exact(&mut input))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Drop packets without delivering CONNECTION_CLOSE or an ICMP error.
        relay.blackhole.store(true, Ordering::SeqCst);
        tokio::time::pause();
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert!(!bridge.is_finished());
        // QUIC permits the first ack-eliciting send after receiving a packet
        // to restart its idle timer, even though the peer is already gone.
        home.write_all(b"late update").await.unwrap();
        timeout(Duration::from_secs(12), bridge)
            .await
            .expect("silent peer survived more than 30 seconds after packet loss")
            .unwrap();
        assert_eq!(
            home.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        tokio::time::resume();
        drop(client);
    }
}

#[tokio::test]
async fn keep_alive_preserves_quiet_sessions_without_application_traffic() {
    for composite in [false, true] {
        let (client, edge, _relay) = peers().await;
        let connection = (*edge.session).clone();
        let (bridge, mut home, home_datagram) = start_bridge(edge, composite, 4096);
        drop(home_datagram);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let initial = connection.stats().frame_rx;
        for _ in 0..4 {
            let acks = connection.stats().frame_rx.acks;
            tokio::time::pause();
            tokio::time::sleep(WEBTRANSPORT_KEEP_ALIVE_INTERVAL).await;
            // Let real UDP I/O deliver the keepalive and its ACK before
            // advancing the virtual clock to the next probe.
            tokio::time::resume();
            timeout(TEST_TIMEOUT, async {
                while connection.stats().frame_rx.acks == acks {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("idle client did not acknowledge a QUIC keepalive");
            assert!(
                !bridge.is_finished(),
                "healthy idle session was disconnected"
            );
        }
        let received = connection.stats().frame_rx;
        assert_eq!(received.stream, initial.stream);
        assert_eq!(received.datagram, initial.datagram);
        client.session.close(0, b"test complete");
        timeout(TEST_TIMEOUT, bridge).await.unwrap().unwrap();
        assert_eq!(
            home.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }
}

#[tokio::test]
async fn congested_video_cannot_fill_a_multi_megabyte_quic_send_queue() {
    let (client, edge, relay) = peers().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    relay.blackhole.store(true, Ordering::SeqCst);
    let mut send = web_transport_send::Writer::new(&edge.session, edge.send);
    let video = vec![0x5a; 2 * 1024 * 1024];
    let accepted = timeout(TEST_TIMEOUT, send.write(&video))
        .await
        .unwrap()
        .unwrap();
    assert!(
        accepted <= 80 * 1024,
        "QUIC accepted {accepted} bytes before applying backpressure"
    );
    assert!(
        timeout(Duration::from_millis(100), send.write_all(&video))
            .await
            .is_err(),
        "a vanished reader must backpressure video, not absorb megabytes"
    );
    edge.session.close(0, b"test complete");
    client.session.close(0, b"test complete");
}

struct VideoConditions {
    bytes_per_second: u64,
    queue_delay_ms: u64,
    drop_every: u64,
    warmup: Duration,
    changed_rate: Option<u64>,
    path_delay: Duration,
    changed_delay: Option<Duration>,
}

async fn video_control_measurement(
    composite: bool,
    conditions: VideoConditions,
) -> (f64, Duration) {
    let (mut client, edge, relay) =
        peers_with_link(Duration::from_millis(100), conditions.bytes_per_second).await;
    let link = relay.link.as_ref().unwrap();
    link.set_queue_delay(conditions.queue_delay_ms);
    link.set_loss(conditions.drop_every);
    link.set_delay(conditions.path_delay);
    let (bridge, home, _datagram) = start_bridge(edge, composite, 16 * 1024);
    let (mut input, mut output) = tokio::io::split(home);
    let producer = tokio::spawn(async move {
        let video = vec![0x5a; 16 * 1024];
        loop {
            // Match the native priority writer: control goes before the next
            // video fragment, once the current fragment has finished writing.
            let request = tokio::select! {
                biased;
                request = input.read_u8() => match request {
                    Ok(request) => request,
                    Err(_) => break,
                },
                _ = tokio::task::yield_now() => 0,
            };
            if output.write_all(&[request]).await.is_err() {
                break;
            }
            if request == 0 && output.write_all(&video).await.is_err() {
                break;
            }
        }
    });
    let bytes = Arc::new(AtomicU64::new(0));
    let received = bytes.clone();
    let (pongs, mut replies) = tokio::sync::mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        let mut video = vec![0; 16 * 1024];
        while let Ok(kind) = client.recv.read_u8().await {
            if kind == 0 {
                if client.recv.read_exact(&mut video).await.is_err() {
                    break;
                }
                received.fetch_add(video.len() as u64, Ordering::Relaxed);
            } else if pongs.send(kind).is_err() {
                break;
            }
        }
    });
    tokio::time::sleep(conditions.warmup).await;
    if let Some(rate) = conditions.changed_rate {
        link.set_rate(rate);
        // Bytes already in the network must drain at its new capacity. Test
        // recovery after that transient, not instantaneous removal of bytes
        // a router has already accepted.
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    if let Some(delay) = conditions.changed_delay {
        link.set_delay(delay);
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let start_bytes = bytes.load(Ordering::Relaxed);
    let start = tokio::time::Instant::now();
    let mut worst_rtt = Duration::ZERO;
    for id in 1..=30 {
        let sent = tokio::time::Instant::now();
        client.send.write_all(&[id]).await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(5), replies.recv())
                .await
                .expect("control queued behind seconds of video"),
            Some(id),
        );
        worst_rtt = worst_rtt.max(sent.elapsed());
    }
    let goodput =
        (bytes.load(Ordering::Relaxed) - start_bytes) as f64 / start.elapsed().as_secs_f64();
    eprintln!(
        "WebTransport composite={composite}, rate={}, queue={}ms, drop_every={}, path={:?} each way: {:.1} Mbit/s, worst control RTT {:.1} ms",
        conditions
            .changed_rate
            .unwrap_or(conditions.bytes_per_second),
        conditions.queue_delay_ms,
        conditions.drop_every,
        conditions.changed_delay.unwrap_or(conditions.path_delay),
        goodput * 8.0 / 1_000_000.0,
        worst_rtt.as_secs_f64() * 1000.0,
    );
    client.session.close(0, b"test complete");
    timeout(TEST_TIMEOUT, bridge).await.unwrap().unwrap();
    producer.await.unwrap();
    reader.await.unwrap();
    assert!(
        worst_rtt >= Duration::from_millis(200),
        "the simulated path did not apply its propagation delay"
    );
    (goodput, worst_rtt)
}

#[tokio::test(start_paused = true)]
async fn long_fat_pipe_carries_video_without_delaying_control_by_seconds() {
    for composite in [false, true] {
        // 80 Mbit/s, 100 ms each way: the path needs 2 MB in flight.
        let (goodput, worst_rtt) = video_control_measurement(
            composite,
            VideoConditions {
                bytes_per_second: 10_000_000,
                queue_delay_ms: 50,
                drop_every: 0,
                warmup: Duration::from_secs(3),
                changed_rate: None,
                path_delay: Duration::from_millis(100),
                changed_delay: None,
            },
        )
        .await;
        assert!(
            goodput > 4_000_000.0,
            "fast long path throttled to {goodput:.0} B/s"
        );
        assert!(
            worst_rtt < Duration::from_millis(750),
            "control RTT {worst_rtt:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
#[ignore = "Known saturated-router bufferbloat; RTT-only congestion backoff was unsafe"]
async fn deep_network_buffers_do_not_add_seconds_to_control() {
    for composite in [false, true] {
        // A router can retain seconds of video even when the local QUIC
        // waiting queue is bounded. Loss-only CUBIC reached 4.1 seconds here.
        let (goodput, worst_rtt) = video_control_measurement(
            composite,
            VideoConditions {
                bytes_per_second: 1_000_000,
                queue_delay_ms: 2_000,
                drop_every: 0,
                warmup: Duration::from_secs(10),
                changed_rate: None,
                path_delay: Duration::from_millis(100),
                changed_delay: None,
            },
        )
        .await;
        assert!(goodput > 400_000.0, "video throttled to {goodput:.0} B/s");
        assert!(
            worst_rtt < Duration::from_millis(750),
            "network buffering raised RTT to {worst_rtt:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
#[ignore = "Known saturated-router bufferbloat; RTT-only congestion backoff was unsafe"]
async fn long_path_recovers_after_bandwidth_drops() {
    let (goodput, worst_rtt) = video_control_measurement(
        true,
        VideoConditions {
            bytes_per_second: 10_000_000,
            queue_delay_ms: 2_000,
            drop_every: 0,
            warmup: Duration::from_secs(3),
            changed_rate: Some(1_000_000),
            path_delay: Duration::from_millis(100),
            changed_delay: None,
        },
    )
    .await;
    assert!(
        goodput > 400_000.0,
        "video did not recover: {goodput:.0} B/s"
    );
    assert!(
        worst_rtt < Duration::from_millis(750),
        "queue did not drain: {worst_rtt:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn long_path_with_packet_loss_keeps_control_responsive() {
    let (goodput, worst_rtt) = video_control_measurement(
        true,
        VideoConditions {
            bytes_per_second: 1_000_000,
            queue_delay_ms: 2_000,
            drop_every: 200,
            warmup: Duration::from_secs(10),
            changed_rate: None,
            path_delay: Duration::from_millis(100),
            changed_delay: None,
        },
    )
    .await;
    assert!(goodput > 100_000.0, "loss stalled video: {goodput:.0} B/s");
    // A lost stream packet can require several recovery round trips. Keep
    // that transient bounded without treating retransmission time as a queue
    // the application could discard.
    assert!(
        worst_rtt < Duration::from_millis(1_500),
        "loss stalled control: {worst_rtt:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_warm_fast_pipe_does_not_turn_into_a_large_waiting_queue_on_loss() {
    let (mut client, edge, relay) = peers_with_link(Duration::from_millis(100), 10_000_000).await;
    let mut send = web_transport_send::Writer::new(&edge.session, edge.send);
    let accepted = Arc::new(AtomicU64::new(0));
    let sent = accepted.clone();
    let writer = tokio::spawn(async move {
        let video = vec![0x5a; 16 * 1024];
        while let Ok(count) = send.write(&video).await {
            sent.fetch_add(count as u64, Ordering::Relaxed);
        }
    });
    let reader = tokio::spawn(async move {
        let mut buffer = vec![0; 64 * 1024];
        while matches!(client.recv.read(&mut buffer).await, Ok(Some(_))) {}
    });
    tokio::time::sleep(Duration::from_secs(3)).await;
    let before = accepted.load(Ordering::Relaxed);
    relay.blackhole.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let extra = accepted.load(Ordering::Relaxed) - before;
    edge.session.close(0, b"test complete");
    client.session.close(0, b"test complete");
    timeout(TEST_TIMEOUT, writer).await.unwrap().unwrap();
    timeout(TEST_TIMEOUT, reader).await.unwrap().unwrap();
    assert!(before > 4 * 1024 * 1024, "the fast path never warmed up");
    assert!(
        extra <= 256 * 1024,
        "queued {extra} extra bytes after the pipe stopped"
    );
    eprintln!("warm WebTransport path admitted {extra} extra bytes after packet loss");
}

#[tokio::test(start_paused = true)]
async fn a_higher_path_rtt_does_not_collapse_video_throughput() {
    for composite in [false, true] {
        for change_after_warmup in [false, true] {
            // The handshake measured 200 ms. Later packets take 300 ms even
            // on an uncongested path. Treating every ACK above the historical
            // minimum as another congestion event collapses CUBIC's window.
            let (goodput, worst_rtt) = video_control_measurement(
                composite,
                VideoConditions {
                    bytes_per_second: 10_000_000,
                    queue_delay_ms: 50,
                    drop_every: 0,
                    warmup: Duration::from_secs(10),
                    changed_rate: None,
                    path_delay: Duration::from_millis(if change_after_warmup { 100 } else { 150 }),
                    changed_delay: change_after_warmup.then_some(Duration::from_millis(150)),
                },
            )
            .await;
            assert!(
                goodput > 4_000_000.0,
                "changed path throttled video to {goodput:.0} B/s"
            );
            // CUBIC can incur packet loss at the bottleneck during probing.
            // Permit recovery round trips, but catch the multi-second stalls
            // caused by treating a higher propagation RTT as perpetual loss.
            assert!(
                worst_rtt < Duration::from_millis(900),
                "control RTT {worst_rtt:?}"
            );
        }
    }
}
