//! `yas forward` and `yas socks` on `yas_client::net`: one native session
//! carries every TCP stream and UDP flow; this module adds what the CLI's
//! listeners need around it (waiting for the session, relaying one accepted
//! socket).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use yas_client::net::{DatagramFlow, Net, StreamOptions, Tls};
use yas_wire::core::Status;

pub(crate) use yas_client::net::bracket;

pub(crate) const DEFAULT_BIND: &str = "127.0.0.1";

const TEAR_DOWN_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Default)]
pub(crate) struct TlsConfig {
    pub(crate) alpn: Vec<String>,
    pub(crate) insecure: bool,
}

impl TlsConfig {
    fn native(&self) -> Tls {
        Tls {
            server_name: None,
            alpn: self
                .alpn
                .iter()
                .map(|value| value.as_bytes().to_vec())
                .collect(),
            insecure: self.insecure,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Connection {
    net: Net,
    relays: Arc<Relays>,
}

#[derive(Default)]
struct Relays {
    active: AtomicUsize,
    idle: Notify,
}

impl Connection {
    pub(crate) async fn connect(on: Option<&str>, hub: &str) -> Result<Self, String> {
        let native = crate::yas_native::connect(on, hub).await?;
        let client = yas_client::Client::from_native(native);
        let net = client.net().map_err(|error| error.to_string())?;
        Ok(Self {
            net,
            relays: Arc::default(),
        })
    }

    /// Until the session ends, then a short grace for relays to report.
    pub(crate) async fn wait_closed(&self) {
        let _ = self.net.client().closed().await;
        let _ = tokio::time::timeout(TEAR_DOWN_GRACE, async {
            loop {
                let idle = self.relays.idle.notified();
                if self.relays.active.load(Ordering::Acquire) == 0 {
                    break;
                }
                idle.await;
            }
        })
        .await;
    }

    pub(crate) fn relay_guard(&self) -> RelayGuard {
        self.relays.active.fetch_add(1, Ordering::AcqRel);
        RelayGuard {
            relays: Arc::clone(&self.relays),
        }
    }

    pub(crate) async fn open_udp(&self, host: &str, port: u16) -> yas_client::Result<DatagramFlow> {
        self.net.open_udp(host, port).await
    }

    pub(crate) fn max_datagram_payload(&self) -> usize {
        self.net.max_datagram_payload()
    }
}

pub(crate) struct RelayGuard {
    relays: Arc<Relays>,
}

impl Drop for RelayGuard {
    fn drop(&mut self) {
        if self.relays.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.relays.idle.notify_waiters();
        }
    }
}

pub(crate) enum OnOpen {
    Report {
        announce_alpn: Option<Arc<AtomicBool>>,
    },
    Answer(fn(Status) -> Vec<u8>),
}

/// Relay one accepted local socket to `host:port` from the server: half-close
/// mapped to half-close, an abnormal end to a reset on both sides.
pub(crate) async fn relay_tcp(
    mut local: tokio::net::TcpStream,
    connection: Connection,
    host: String,
    port: u16,
    tls: Option<TlsConfig>,
    on_open: OnOpen,
) -> Result<(), String> {
    let _guard = connection.relay_guard();
    let _ = local.set_nodelay(true);
    let target = format!("{}:{port}", bracket(&host));
    // A forward's client usually speaks first: carry its first bytes in the
    // open, saving a round trip (docs/design/net.md § One round trip).
    let limit = connection.net.limits().max_early_data_bytes.min(16_384) as usize;
    let early_data = if matches!(&on_open, OnOpen::Report { .. }) && limit != 0 {
        let mut early = vec![0; limit];
        match local.try_read(&mut early) {
            Ok(count) => {
                early.truncate(count);
                early
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Vec::new(),
            Err(error) => return Err(format!("reading local client: {error}")),
        }
    } else {
        Vec::new()
    };
    let options = StreamOptions {
        tls: tls.as_ref().map(TlsConfig::native),
        early_data,
        ..StreamOptions::default()
    };
    let mut stream = match connection.net.open_tcp_with(&host, port, &options).await {
        Ok(stream) => stream,
        Err(error) => {
            if let OnOpen::Answer(reply) = on_open {
                let status = error.status().unwrap_or(Status::Internal);
                let _ = local.write_all(&reply(status)).await;
            }
            return Err(format!("{target}: {error}"));
        }
    };
    if let OnOpen::Answer(reply) = &on_open {
        local
            .write_all(&reply(Status::Ok))
            .await
            .map_err(|error| format!("writing local handshake: {error}"))?;
    }
    if let OnOpen::Report {
        announce_alpn: Some(announced),
    } = &on_open
        && !announced.swap(true, Ordering::Relaxed)
    {
        let alpn = String::from_utf8_lossy(stream.negotiated_alpn());
        eprintln!(
            "yas: tls to {target} established ({})",
            if alpn.is_empty() {
                "no alpn".to_owned()
            } else {
                format!("alpn={alpn}")
            }
        );
    }
    match tokio::io::copy_bidirectional(&mut local, &mut stream).await {
        Ok(_) => Ok(()),
        Err(error) => {
            stream.abort();
            let _ = local.set_zero_linger();
            Err(error.to_string())
        }
    }
}
