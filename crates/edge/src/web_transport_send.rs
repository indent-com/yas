//! Keep reliable writes close to QUIC transmission without limiting the pipe
//! to one small buffer per RTT. Quinn's send_window includes both queued bytes
//! and bytes already in flight; a fixed 64 KiB window caps a 200 ms path at
//! 2.6 Mbit/s even if the link can carry hundreds of Mbit/s.

use futures_util::task::AtomicWaker;
use quinn_proto::RttEstimator;
use quinn_proto::congestion::{Controller, ControllerFactory, ControllerMetrics, CubicConfig};
use std::any::Any;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::AsyncWrite;
use web_transport_quinn::{SendStream, Session, quinn};

pub(super) const QUEUE_BYTES: u64 = 64 * 1024;
// A memory ceiling, not the ordinary admission window. Covers a 1 Gbit/s
// connection with 500 ms RTT while retaining a per-connection upper bound.
const MAX_WINDOW_BYTES: u64 = 64 * 1024 * 1024;
const REFRESH_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct Flight {
    bytes: AtomicU64,
    writer: AtomicWaker,
}

impl Flight {
    fn set(&self, bytes: u64) {
        if self.bytes.swap(bytes, Ordering::AcqRel) != bytes {
            self.writer.wake();
        }
    }
}

pub(super) struct Factory;

impl ControllerFactory for Factory {
    fn build(self: Arc<Self>, now: Instant, mtu: u16) -> Box<dyn Controller> {
        Box::new(Tracked {
            inner: Arc::new(CubicConfig::default()).build(now, mtu),
            flight: Arc::new(Flight::default()),
        })
    }
}

// Preserve Quinn's congestion controller; observe its transmission callbacks
// to size application admission. The exact in-flight count is refreshed after
// each ACK batch. Between batches on_sent includes packet headers/ACKs, so this
// is a conservative allowance, also capped at the congestion window.
struct Tracked {
    inner: Box<dyn Controller>,
    flight: Arc<Flight>,
}

impl Controller for Tracked {
    fn on_sent(&mut self, now: Instant, bytes: u64, packet: u64) {
        self.inner.on_sent(now, bytes, packet);
        self.flight.set(
            self.flight
                .bytes
                .load(Ordering::Acquire)
                .saturating_add(bytes)
                .min(self.inner.window()),
        );
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest: Option<u64>,
    ) {
        self.inner.on_end_acks(now, in_flight, app_limited, largest);
        self.flight.set(in_flight.min(self.inner.window()));
    }

    fn on_congestion_event(&mut self, now: Instant, sent: Instant, persistent: bool, lost: u64) {
        self.inner.on_congestion_event(now, sent, persistent, lost);
        self.flight.set(
            self.flight
                .bytes
                .load(Ordering::Acquire)
                .saturating_sub(lost)
                .min(self.inner.window()),
        );
    }

    fn on_mtu_update(&mut self, mtu: u16) {
        self.inner.on_mtu_update(mtu);
    }

    fn window(&self) -> u64 {
        self.inner.window()
    }

    fn metrics(&self) -> ControllerMetrics {
        self.inner.metrics()
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Self {
            inner: self.inner.clone_box(),
            flight: self.flight.clone(),
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

pub(super) struct Writer {
    stream: SendStream,
    connection: quinn::Connection,
    flight: Arc<Flight>,
    window: u64,
    refresh: Pin<Box<tokio::time::Sleep>>,
    last_write_poll: Instant,
}

impl Writer {
    pub(super) fn new(session: &Session, stream: SendStream) -> Self {
        let connection: quinn::Connection = (**session).clone();
        let flight = Self::flight(&connection);
        Self {
            stream,
            connection,
            flight,
            window: QUEUE_BYTES,
            refresh: Box::pin(tokio::time::sleep(REFRESH_INTERVAL)),
            last_write_poll: Instant::now(),
        }
    }

    fn flight(connection: &quinn::Connection) -> Arc<Flight> {
        connection
            .congestion_state()
            .into_any()
            .downcast::<Tracked>()
            .expect("WebTransport uses the tracked congestion controller")
            .flight
    }
}

impl AsyncWrite for Writer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Migration/rebinding can replace the controller. Refresh even while
        // blocked so a writer never waits on an obsolete controller's waker.
        if self.refresh.as_mut().poll(cx).is_ready() {
            self.flight = Self::flight(&self.connection);
            self.refresh
                .as_mut()
                .reset(tokio::time::Instant::now() + REFRESH_INTERVAL);
        }
        let now = Instant::now();
        let idle = now.duration_since(self.last_write_poll);
        if idle >= Duration::from_secs(2) && idle >= self.connection.rtt().saturating_mul(3) {
            // ACK-only packets can accumulate in on_sent without another ACK
            // batch correcting the estimate. After an application-idle period,
            // start conservatively; the next real flight rebuilds its allowance.
            self.flight.set(0);
        }
        self.last_write_poll = now;
        self.flight.writer.register(cx.waker());
        let window = self
            .flight
            .bytes
            .load(Ordering::Acquire)
            .saturating_add(QUEUE_BYTES)
            .min(MAX_WINDOW_BYTES);
        if self.window != window {
            self.connection.set_send_window(window);
            self.window = window;
        }
        Pin::new(&mut self.stream).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
