//! QUIC datagrams on a virtual clock: no OS buffers, scheduler timing, or real
//! UDP I/O in the bandwidth and packet-loss regression tests.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, Sleep};
use web_transport_quinn::quinn::{self, AsyncUdpSocket, UdpPoller, udp};

#[derive(Debug)]
pub(super) struct Link {
    pub(super) blackhole: Arc<AtomicBool>,
    server: Arc<Socket>,
    client: Arc<Socket>,
}

impl Link {
    pub(super) fn new(delay: Duration, bytes_per_second: u64) -> Self {
        let blackhole = Arc::new(AtomicBool::new(false));
        let (to_server, server_rx) = mpsc::unbounded_channel();
        let (to_client, client_rx) = mpsc::unbounded_channel();
        let socket = |port, outgoing, incoming, rate| {
            Arc::new(Socket {
                address: ([127, 0, 0, 1], port).into(),
                blackhole: blackhole.clone(),
                delay,
                bytes_per_second: rate,
                outgoing,
                incoming: Mutex::new(Incoming {
                    packets: incoming,
                    pending: BinaryHeap::new(),
                    timer: Box::pin(tokio::time::sleep(Duration::ZERO)),
                }),
                ready: Mutex::new(Instant::now()),
                sequence: AtomicU64::new(0),
            })
        };
        Self {
            server: socket(5000, to_client, server_rx, bytes_per_second),
            client: socket(5001, to_server, client_rx, 0),
            blackhole,
        }
    }

    pub(super) fn server(&self, config: quinn::ServerConfig) -> io::Result<quinn::Endpoint> {
        self.endpoint(Some(config), self.server.clone())
    }

    pub(super) fn client(&self) -> io::Result<quinn::Endpoint> {
        self.endpoint(None, self.client.clone())
    }

    fn endpoint(
        &self,
        config: Option<quinn::ServerConfig>,
        socket: Arc<Socket>,
    ) -> io::Result<quinn::Endpoint> {
        quinn::Endpoint::new_with_abstract_socket(
            Default::default(),
            config,
            socket,
            Arc::new(quinn::TokioRuntime),
        )
    }
}

type Packet = (Instant, u64, Vec<u8>);

#[derive(Debug)]
struct Incoming {
    packets: mpsc::UnboundedReceiver<Packet>,
    pending: BinaryHeap<Reverse<Packet>>,
    timer: Pin<Box<Sleep>>,
}

#[derive(Debug)]
struct Socket {
    address: SocketAddr,
    blackhole: Arc<AtomicBool>,
    delay: Duration,
    bytes_per_second: u64,
    outgoing: mpsc::UnboundedSender<Packet>,
    incoming: Mutex<Incoming>,
    ready: Mutex<Instant>,
    sequence: AtomicU64,
}

impl AsyncUdpSocket for Socket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Writable)
    }

    fn try_send(&self, transmit: &udp::Transmit) -> io::Result<()> {
        if self.blackhole.load(Ordering::SeqCst) {
            return Ok(());
        }
        let now = Instant::now();
        let mut due = now;
        if self.bytes_per_second != 0 {
            let mut ready = self.ready.lock().unwrap();
            // Bound the bottleneck queue independently of propagation delay.
            if ready.saturating_duration_since(now) > Duration::from_millis(50) {
                return Ok(());
            }
            *ready = (*ready).max(now)
                + Duration::from_secs_f64(
                    transmit.contents.len() as f64 / self.bytes_per_second as f64,
                );
            due = *ready;
        }
        let _ = self.outgoing.send((
            due + self.delay,
            self.sequence.fetch_add(1, Ordering::Relaxed),
            transmit.contents.to_vec(),
        ));
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut incoming = self.incoming.lock().unwrap();
        while let Poll::Ready(Some(packet)) = incoming.packets.poll_recv(cx) {
            incoming.pending.push(Reverse(packet));
        }
        if self.blackhole.load(Ordering::SeqCst) {
            incoming.pending.clear();
        }
        let Some(Reverse((due, _, _))) = incoming.pending.peek() else {
            return Poll::Pending;
        };
        let due = *due;
        incoming.timer.as_mut().reset(due);
        ready!(incoming.timer.as_mut().poll(cx));
        let Reverse((_, _, packet)) = incoming.pending.pop().unwrap();
        bufs[0][..packet.len()].copy_from_slice(&packet);
        meta[0] = udp::RecvMeta {
            addr: (
                [127, 0, 0, 1],
                if self.address.port() == 5000 {
                    5001
                } else {
                    5000
                },
            )
                .into(),
            len: packet.len(),
            stride: packet.len(),
            ecn: None,
            dst_ip: Some(self.address.ip()),
        };
        Poll::Ready(Ok(1))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.address)
    }
}

#[derive(Debug)]
struct Writable;

impl UdpPoller for Writable {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
