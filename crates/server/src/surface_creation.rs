//! Deliver ready surface encoders without waiting for slower sibling jobs.

use std::{collections::HashMap, time::Duration};
use tokio::task::{Id, JoinSet};

type Subscriber = (u64, u16);

pub(super) struct SurfaceCreations<T> {
    tasks: JoinSet<T>,
    subscribers: HashMap<Id, Subscriber>,
    deadline: tokio::time::Instant,
}

impl<T: Send + 'static> SurfaceCreations<T> {
    pub(super) fn new(timeout: Duration) -> Self {
        Self {
            tasks: JoinSet::new(),
            subscribers: HashMap::new(),
            deadline: tokio::time::Instant::now() + timeout,
        }
    }

    pub(super) fn spawn(
        &mut self,
        subscriber: Subscriber,
        create: impl FnOnce() -> T + Send + 'static,
    ) {
        let task = self.tasks.spawn_blocking(create);
        self.subscribers.insert(task.id(), subscriber);
    }

    /// Each result can be installed immediately. Failures identify only the
    /// subscriptions whose creation flags need clearing. The deadline starts
    /// at dispatch, so fast completions cannot keep a hung sibling alive.
    pub(super) async fn next(&mut self) -> Option<Result<T, Vec<Subscriber>>> {
        if self.subscribers.is_empty() {
            return None;
        }
        match tokio::time::timeout_at(self.deadline, self.tasks.join_next_with_id()).await {
            Ok(Some(Ok((id, result)))) => {
                self.subscribers.remove(&id);
                Some(Ok(result))
            }
            Ok(Some(Err(error))) => {
                let subscriber = self
                    .subscribers
                    .remove(&error.id())
                    .expect("tracked creation");
                eprintln!(
                    "[surface-encoder] create task failed: cid={} sid={}: {error}",
                    subscriber.0, subscriber.1
                );
                Some(Err(vec![subscriber]))
            }
            Ok(None) => None,
            Err(_) => {
                // A running driver call cannot be interrupted. Its eventual
                // result is dropped by the task set, never installed into a
                // subscription that may already have retried.
                self.tasks.abort_all();
                let failed: Vec<_> = self.subscribers.drain().map(|(_, sub)| sub).collect();
                for &(cid, sid) in &failed {
                    eprintln!("[surface-encoder] create timed out: cid={cid} sid={sid}");
                }
                Some(Err(failed))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Virtual driver completion times make the latency and timeout contracts
    // deterministic; production uses blocking driver calls through spawn().
    fn create_after(jobs: &mut SurfaceCreations<u16>, sid: u16, millis: u64, panic: bool) {
        let task = jobs.tasks.spawn(async move {
            tokio::time::sleep(Duration::from_millis(millis)).await;
            assert!(!panic, "simulated driver panic");
            sid
        });
        jobs.subscribers.insert(task.id(), (7, sid));
    }

    #[tokio::test(start_paused = true)]
    async fn ready_surface_does_not_wait_for_slow_siblings() {
        let start = tokio::time::Instant::now();
        let mut jobs = SurfaceCreations::new(Duration::from_secs(10));
        create_after(&mut jobs, 1, 250, false);
        create_after(&mut jobs, 2, 20, false);

        assert_eq!(jobs.next().await, Some(Ok(2)));
        assert_eq!(start.elapsed(), Duration::from_millis(20));
        assert_eq!(jobs.next().await, Some(Ok(1)));
        assert_eq!(start.elapsed(), Duration::from_millis(250));
        assert_eq!(jobs.next().await, None);
    }

    #[tokio::test]
    async fn a_blocked_driver_does_not_hold_a_finished_creation() {
        let mut jobs = SurfaceCreations::new(Duration::from_secs(10));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        jobs.spawn((7, 1), move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
            1
        });
        entered_rx.await.unwrap();
        jobs.spawn((7, 2), || 2);

        let ready = tokio::time::timeout(Duration::from_secs(1), jobs.next()).await;
        // Release even when the assertion will fail, so the test cannot
        // leave a worker blocked during runtime shutdown.
        let _ = release_tx.send(());
        assert_eq!(ready.unwrap(), Some(Ok(2)));
        assert_eq!(jobs.next().await, Some(Ok(1)));
        assert_eq!(jobs.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicked_creation_does_not_discard_ready_siblings() {
        let mut jobs = SurfaceCreations::new(Duration::from_secs(10));
        create_after(&mut jobs, 1, 10, true);
        create_after(&mut jobs, 2, 20, false);
        assert_eq!(jobs.next().await, Some(Err(vec![(7, 1)])));
        assert_eq!(jobs.next().await, Some(Ok(2)));
        assert_eq!(jobs.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn creation_deadline_does_not_restart_after_each_completion() {
        let start = tokio::time::Instant::now();
        let mut jobs = SurfaceCreations::new(Duration::from_millis(100));
        for (sid, millis) in [(1, 20), (2, 80), (3, 200), (4, 300)] {
            create_after(&mut jobs, sid, millis, false);
        }
        assert_eq!(jobs.next().await, Some(Ok(1)));
        assert_eq!(jobs.next().await, Some(Ok(2)));
        let mut failed = jobs.next().await.unwrap().unwrap_err();
        failed.sort_unstable();
        assert_eq!(failed, [(7, 3), (7, 4)]);
        assert_eq!(start.elapsed(), Duration::from_millis(100));
        assert_eq!(jobs.next().await, None);
    }
}
