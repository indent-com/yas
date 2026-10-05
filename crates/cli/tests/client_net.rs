//! End-to-end tests of `yas_client::net` against real, private YAS servers:
//! TCP streams (half-close, credit windows, many on one session), UDP flows,
//! why opens fail, and `--net-only` / `--allow-forward-strict` servers.

#![cfg(unix)]

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use yas_client::host::{HostOptions, HostedServer};
use yas_client::net::NetFailure;
use yas_client::process::Command;
use yas_client::{Error, wire::family};

const TIMEOUT: Duration = Duration::from_secs(60);

fn options() -> HostOptions {
    HostOptions::new(env!("CARGO_BIN_EXE_yas"))
        .arg("--no-persistent-extensions")
        .env("YAS_EXT", "0")
        .env("YAS_CHANNEL", "0")
        .env("YAS_FONTS", "0")
        .env("YAS_AUDIO", "0")
}

async fn start(options: HostOptions) -> HostedServer {
    tokio::time::timeout(TIMEOUT, HostedServer::start(options))
        .await
        .expect("hosted server start timed out")
        .expect("hosted server starts")
}

/// A TCP peer that reads to end of file, then answers with what it read,
/// reversed, and closes: a half-close test in one function.
async fn reversing_peer() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).await.unwrap();
                bytes.reverse();
                stream.write_all(&bytes).await.unwrap();
            });
        }
    });
    (address, task)
}

fn payload(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_streams_half_close_and_share_one_session() {
    let (peer, _peer_task) = reversing_peer().await;
    let server = start(options()).await;
    let client = server.connect().await.unwrap();
    let net = client.net().unwrap();

    // Several MiB each way, past every credit window, on eight concurrent
    // streams of one session.
    let streams = (0..8u8).map(|seed| {
        let net = net.clone();
        tokio::spawn(async move {
            let sent = payload(3 * 1024 * 1024 + usize::from(seed) * 4099, seed);
            let stream = net.open_tcp("127.0.0.1", peer.port()).await.unwrap();
            assert_eq!(stream.negotiated_alpn(), b"");
            let (mut reader, mut writer) = tokio::io::split(stream);
            let upload = {
                let sent = sent.clone();
                tokio::spawn(async move {
                    writer.write_all(&sent).await.unwrap();
                    // Half-close: the peer reads end of file and answers.
                    writer.shutdown().await.unwrap();
                    writer
                })
            };
            let mut received = Vec::new();
            reader.read_to_end(&mut received).await.unwrap();
            let _writer = upload.await.unwrap();
            let mut expected = sent;
            expected.reverse();
            assert_eq!(received.len(), expected.len());
            assert!(received == expected, "stream {seed} bytes differ");
        })
    });
    for stream in streams {
        tokio::time::timeout(TIMEOUT, stream)
            .await
            .expect("stream timed out")
            .unwrap();
    }
    // The session still works after its streams ended.
    let mut stream = net.open_tcp("127.0.0.1", peer.port()).await.unwrap();
    stream.write_all(b"olleh").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.unwrap();
    assert_eq!(answer, "hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aborted_stream_resets_its_peer_and_spares_the_session() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = start(options()).await;
    let client = server.connect().await.unwrap();
    let net = client.net().unwrap();

    let mut stream = net.open_tcp("127.0.0.1", port).await.unwrap();
    let (mut accepted, _) = listener.accept().await.unwrap();
    stream.write_all(b"partial").await.unwrap();
    let mut first = [0u8; 7];
    accepted.read_exact(&mut first).await.unwrap();
    stream.abort();
    let mut rest = Vec::new();
    // The peer sees its connection end (EOF or reset), not a hang.
    let ended = tokio::time::timeout(TIMEOUT, accepted.read_to_end(&mut rest)).await;
    assert!(ended.is_ok(), "peer never saw the abort");

    let mut again = net.open_tcp("127.0.0.1", port).await.unwrap();
    let (mut accepted, _) = listener.accept().await.unwrap();
    accepted.write_all(b"still here").await.unwrap();
    drop(accepted);
    let mut answer = String::new();
    again.read_to_string(&mut answer).await.unwrap();
    assert_eq!(answer, "still here");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_flows_relay_whole_datagrams() {
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = peer.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 65536];
        loop {
            let (count, from) = peer.recv_from(&mut buffer).await.unwrap();
            buffer[..count].reverse();
            peer.send_to(&buffer[..count], from).await.unwrap();
        }
    });
    let server = start(options()).await;
    let client = server.connect().await.unwrap();
    let net = client.net().unwrap();
    let flow = net.open_udp("127.0.0.1", port).await.unwrap();
    for message in [&b"ping"[..], &b"a longer datagram"[..]] {
        flow.send(message).unwrap();
        let answer = tokio::time::timeout(TIMEOUT, flow.recv())
            .await
            .expect("no UDP answer")
            .unwrap()
            .unwrap();
        let mut expected = message.to_vec();
        expected.reverse();
        assert_eq!(answer, expected);
    }
    assert!(flow.send(&vec![0; flow.max_payload() + 1]).is_err());
    flow.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_opens_say_why() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listed = listener.local_addr().unwrap().port();
    let closed = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let server = start(
        options()
            .arg("--allow-forward-strict")
            .arg("--allow-forward")
            .arg(format!("127.0.0.1:{listed},{closed}"))
            .arg("--allow-forward")
            .arg("*.invalid"),
    )
    .await;
    let client = server.connect().await.unwrap();
    let net = client.net().unwrap();

    let failure = |result: yas_client::Result<_>| match result {
        Ok(_) => panic!("open succeeded"),
        Err(error) => error.net_failure(),
    };
    // Strict: loopback is reachable only where listed.
    assert_eq!(
        failure(net.open_tcp("127.0.0.1", listed.wrapping_add(1)).await),
        NetFailure::Denied
    );
    assert_eq!(
        failure(net.open_tcp("localhost", listed.wrapping_add(1)).await),
        NetFailure::Denied
    );
    assert_eq!(
        failure(net.open_tcp("127.0.0.1", closed).await),
        NetFailure::Refused
    );
    assert_eq!(
        failure(net.open_tcp("nothing.invalid", 443).await),
        NetFailure::NotFound
    );
    let stream = net.open_tcp("127.0.0.1", listed).await.unwrap();
    drop(stream);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_net_only_server_offers_net_alone() {
    let (peer, _peer_task) = reversing_peer().await;
    let server = start(
        options()
            .arg("--net-only")
            .arg("--allow-forward")
            .arg(format!("127.0.0.1:{}", peer.port())),
    )
    .await;
    let client = server.connect().await.unwrap();
    let offered: Vec<u16> = client
        .hello()
        .families
        .iter()
        .map(|descriptor| descriptor.family_id)
        .collect();
    assert_eq!(offered, [family::CORE, family::TRANSFER, family::NET]);

    // Every other family is refused at once, with a clear error.
    let spawn = client.spawn(&Command::new("true")).await;
    assert!(matches!(spawn, Err(Error::Unsupported(_))), "{spawn:?}");
    let environment = client.environment().await;
    assert!(
        matches!(environment, Err(Error::Unsupported(_))),
        "{environment:?}"
    );
    assert!(!client.has_family(family::TERMINAL));
    assert!(!client.has_family(family::FS));

    // Net works, to the listed target only (strict: unlisted loopback too).
    let net = client.net().unwrap();
    let mut stream = net.open_tcp("127.0.0.1", peer.port()).await.unwrap();
    stream.write_all(b"ten-ocnoc").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.unwrap();
    assert_eq!(answer, "conco-net");
    let denied = net.open_tcp("127.0.0.1", peer.port().wrapping_add(1)).await;
    assert_eq!(denied.unwrap_err().net_failure(), NetFailure::Denied);
}
