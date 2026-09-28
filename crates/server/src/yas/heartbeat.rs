//! Core liveness, independent of application handlers and reliable writes.

use super::*;

// Selection GET is the only other server-originated Request; its allocator
// skips this ID. Only one Ping is outstanding, so reuse follows its Result.
pub(super) const REQUEST_ID: u32 = u32::MAX;

#[derive(Default)]
pub(super) struct Heartbeat {
    pending: AtomicBool,
    replied: Notify,
    replied_at: StdMutex<Option<tokio::time::Instant>>,
    pub(super) rtt_us: Arc<AtomicU64>,
}

// An isolated browser pause is not distance. Require two successive exchanges
// to raise the estimate, accept a faster sample immediately, and never turn
// seconds of queueing into seconds of Surface credit.
const MAX_RTT: Duration = Duration::from_secs(1);

#[derive(Default)]
struct RttSamples {
    previous: Option<Duration>,
}

impl RttSamples {
    fn record(&mut self, sample: Duration, estimate_us: &AtomicU64) {
        if sample > MAX_RTT {
            self.previous = None;
            return;
        }
        let sample = sample.max(Duration::from_micros(500));
        let current = estimate_us.load(Ordering::Relaxed);
        let confirmed = self.previous.map(|previous| previous.min(sample));
        self.previous = Some(sample);
        if current > 0 && sample.as_micros() < u128::from(current) {
            estimate_us.store(sample.as_micros() as u64, Ordering::Relaxed);
        } else if let Some(confirmed) = confirmed {
            estimate_us.store(confirmed.as_micros() as u64, Ordering::Relaxed);
        }
    }
}

impl Heartbeat {
    /// Consume validated Ping Results in the reader, before application queue
    /// backpressure. Unrelated traffic never acknowledges our outstanding Ping.
    pub(super) fn receive(&self, frame: &Frame) -> Result<bool, ()> {
        if frame.header.class != Class::Result
            || frame.header.family != family::CORE
            || frame.header.kind != yas_wire::core::request_kind::PING
        {
            return Ok(false);
        }
        if frame.header.request_id != Some(REQUEST_ID) {
            return Err(());
        }
        let result = ResultPrefix::decode(&frame.payload).map_err(|_| ())?;
        if result.status != Status::Ok {
            return Err(());
        }
        PingResult::decode(&result.body).map_err(|_| ())?;
        if !self.pending.swap(false, Ordering::AcqRel) {
            return Err(());
        }
        *self.replied_at.lock().expect("Ping receive time") = Some(tokio::time::Instant::now());
        self.replied.notify_one();
        Ok(true)
    }

    /// Returns only on failure. The deadline includes queueing/writing the Ping,
    /// so a non-reading peer cannot keep a session alive by blocking its writer.
    pub(super) async fn run(&self, out: &FrameSender, interval: Duration) {
        if interval.is_zero() {
            std::future::pending::<()>().await;
        }
        let mut samples = RttSamples::default();
        loop {
            tokio::time::sleep(interval).await;
            self.pending.store(true, Ordering::Release);
            let frame = Frame {
                header: FrameHeader::request(
                    family::CORE,
                    yas_wire::core::request_kind::PING,
                    REQUEST_ID,
                ),
                payload: Ping {
                    sender_monotonic_ns: monotonic_ns(),
                }
                .encode()
                .expect("fixed Ping payload"),
            };
            let exchange = async {
                let written = out.send_with_receipt(frame).await.map_err(|_| ())?;
                let sent_at = written.await.map_err(|_| ())?;
                self.replied.notified().await;
                let received_at = self.replied_at.lock().expect("Ping receive time").take();
                // Writer completion excludes local queue delay; reader time
                // excludes scheduling delay before this future resumes. A reply
                // can race write completion, in which case it is liveness only.
                if let Some(sample) = received_at.and_then(|at| at.checked_duration_since(sent_at))
                {
                    samples.record(sample, &self.rtt_us);
                } else {
                    samples.previous = None;
                }
                Ok::<(), ()>(())
            };
            if !matches!(
                tokio::time::timeout(interval.saturating_mul(2), exchange).await,
                Ok(Ok(()))
            ) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtt_requires_confirmation_and_rejects_stalls() {
        let estimate = AtomicU64::new(0);
        let mut samples = RttSamples::default();
        samples.record(Duration::from_millis(800), &estimate);
        assert_eq!(estimate.load(Ordering::Relaxed), 0);
        samples.record(Duration::from_secs(2), &estimate);
        samples.record(Duration::from_millis(300), &estimate);
        assert_eq!(estimate.load(Ordering::Relaxed), 0);
        samples.record(Duration::from_millis(500), &estimate);
        assert_eq!(estimate.load(Ordering::Relaxed), 300_000);
        samples.record(Duration::from_millis(500), &estimate);
        assert_eq!(estimate.load(Ordering::Relaxed), 500_000);
        samples.record(Duration::from_secs(1), &estimate);
        assert_eq!(estimate.load(Ordering::Relaxed), 500_000);
        samples.record(Duration::from_secs(1), &estimate);
        assert_eq!(estimate.load(Ordering::Relaxed), 1_000_000);
        samples.record(Duration::ZERO, &estimate);
        assert_eq!(estimate.load(Ordering::Relaxed), 500);
    }
}
