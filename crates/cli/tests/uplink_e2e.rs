//! Full CLI path through the shared local uplink fixture.
#![cfg(unix)]

#[path = "support/uplink.rs"]
mod uplink;
use std::{path::Path, time::Duration};
use uplink::{Carrier, Fixture, cli};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uplink_cli_remote_execution_and_authentication() {
    tokio::time::timeout(
        Duration::from_secs(45),
        exercise_uplink(Carrier::WebTransport),
    )
    .await
    .expect("uplink E2E stalled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uplink_over_websockets_when_forced() {
    tokio::time::timeout(Duration::from_secs(45), exercise_uplink(Carrier::WebSocket))
        .await
        .expect("uplink E2E over WebSockets stalled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uplink_falls_back_to_websockets_where_udp_goes_nowhere() {
    tokio::time::timeout(Duration::from_secs(60), exercise_uplink(Carrier::Fallback))
        .await
        .expect("uplink E2E falling back to WebSockets stalled");
}

async fn exercise_uplink(carrier: Carrier) {
    let binary = Path::new(env!("CARGO_BIN_EXE_yas"));
    let mut fixture = Fixture::start_with(binary, carrier).await;
    {
        // The relay sees the session before the producer's line about it is read.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let log = loop {
            let log = fixture.producer_log.lock().unwrap().clone();
            let connected = log
                .iter()
                .any(|line| line.starts_with("[uplink] connected to relay"));
            if connected || std::time::Instant::now() > deadline {
                assert!(connected, "producer log: {log:?}");
                break log;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let over_websocket = log.iter().any(|line| {
            line.starts_with("[uplink] connected to relay") && line.ends_with(" over WebSocket")
        });
        assert_eq!(
            over_websocket,
            carrier != Carrier::WebTransport,
            "producer log: {log:?}"
        );
        let gave_up_on_udp = log.iter().any(|line| line.contains("(is UDP blocked?)"));
        assert_eq!(
            gave_up_on_udp,
            carrier == Carrier::Fallback,
            "producer log: {log:?}"
        );
    }
    let root = fixture.directory.path();
    let ca = &fixture.ca;
    let uri = &fixture.uri;
    let consumer_key = &fixture.consumer_key;
    let producer_public = fixture.producer_public;
    let capture = &fixture.capture;
    let requests = &fixture.requests;
    let (attacker_key, attacker_public) = yas_uplink::Identity::generate().unwrap();
    let marker = "uplink-e2e-private-payload-781d";
    let output = cli(binary, root, ca)
        .env("YAS_UPLINK_IDENTITY", consumer_key)
        .args([
            "--on",
            uri,
            "run",
            "/bin/sh",
            "-c",
            &format!("printf '{marker}'; printf 'remote-stderr' >&2; exit 7"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, marker.as_bytes());
    assert_eq!(output.stderr, b"remote-stderr");
    assert!(
        !capture
            .lock()
            .unwrap()
            .windows(marker.len())
            .any(|bytes| bytes == marker.as_bytes()),
        "relay observed plaintext command"
    );

    for (identity, target, expected_error) in [
        (
            attacker_key.as_str(),
            uri.to_string(),
            "end-to-end authentication failed",
        ),
        (
            consumer_key.as_str(),
            uri.replace(&producer_public.to_string(), &attacker_public.to_string()),
            "end-to-end authentication failed",
        ),
        (
            consumer_key.as_str(),
            uri.replace("consumer-token", "invalid-token"),
            "token rejected",
        ),
    ] {
        let forbidden = root.join("forbidden");
        let output = cli(binary, root, ca)
            .env("YAS_UPLINK_IDENTITY", identity)
            .args(["--on", &target, "run", "/usr/bin/touch"])
            .arg(&forbidden)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success(), "unauthorized consumer succeeded");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "unexpected rejection: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!forbidden.exists(), "unauthorized command reached YAS");
    }
    // An explicit CA override must still verify the HTTPS server certificate.
    let unrelated = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let unrelated_ca = root.join("unrelated-ca.pem");
    std::fs::write(&unrelated_ca, unrelated.cert.pem()).unwrap();
    let output = cli(binary, root, &unrelated_ca)
        .env("YAS_UPLINK_IDENTITY", consumer_key)
        .args(["--on", uri, "run", "/bin/sh", "-c", "exit 0"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("attach request failed"));

    assert!(fixture.producer.try_wait().unwrap().is_none());
    let seen = requests.lock().unwrap().clone();
    assert_eq!(seen.iter().filter(|&&r| r == "allocate").count(), 1);
    assert_eq!(seen.iter().filter(|&&r| r == "attach").count(), 3);
    assert_eq!(seen.iter().filter(|&&r| r == "rejected").count(), 1);
}
