//! State subscriptions: a family's catalogue as a snapshot plus deltas.
//!
//! Families with state (Process, FS, KV, Terminal, Surface, …) answer `WATCH`
//! with a subscription ID, then send `STATE` events: a snapshot
//! (`SnapshotBegin`, `SnapshotRecords`…, `SnapshotEnd`) followed by `Delta`s.
//! [`Subscription`] acknowledges each event as it is taken (granting the next
//! window of credit) and sends `UNWATCH` when dropped.

use yas_wire::{
    Class, Decode, Encode, Frame, FrameHeader,
    core::ResultPrefix,
    state::{Phase, Record, StateAck, StateEvent, Unwatch, WatchResult},
};

use crate::client::{Client, DEFAULT_REQUEST_TIMEOUT, FrameReceiver, Hook, Route};
use crate::error::{Error, Result};

/// Each State subscription's window, in event payload bytes: its initial
/// credit, kept constant by acknowledging every event as it is taken. One
/// wire frame, so any event fits, while a session's subscriptions share the
/// server's State budget (the peer's buffered bytes split across watches)
/// instead of the first one leasing all of it.
pub const STATE_CREDIT: u64 = yas_wire::schema::transport::RECOMMENDED_WIRE_FRAME as u64;

/// Event kind of `STATE_ACK` in every family with state.
const STATE_ACK_KIND: u16 = 1;

/// A live State subscription. Dropping it unsubscribes.
pub struct Subscription {
    client: Client,
    family: u16,
    unwatch_kind: u16,
    subscription_id: u32,
    current_revision: u64,
    frames: FrameReceiver,
    record_kinds: &'static [u16],
    cumulative_credit: u64,
    closed: bool,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("family", &self.family)
            .field("subscription_id", &self.subscription_id)
            .finish()
    }
}

impl Subscription {
    /// Send a family `WATCH` whose OK body is a `StateWatchResult`.
    pub(crate) async fn open(
        client: &Client,
        family: u16,
        watch_kind: u16,
        unwatch_kind: u16,
        payload: Vec<u8>,
    ) -> Result<Self> {
        let hook: Hook = Box::new(move |prefix: &ResultPrefix| {
            WatchResult::decode(&prefix.body)
                .map(|result| vec![Route::State(family, result.subscription_id)])
                .unwrap_or_default()
        });
        let mut reply = client
            .call_ok(
                family,
                watch_kind,
                payload,
                Some(DEFAULT_REQUEST_TIMEOUT),
                Some(hook),
            )
            .await?;
        let result = WatchResult::decode(&reply.prefix.body)?;
        let frames = reply
            .take(Route::State(family, result.subscription_id))
            .ok_or_else(|| Error::protocol("YAS WATCH route missing"))?;
        Ok(Self {
            client: client.clone(),
            family,
            unwatch_kind,
            subscription_id: result.subscription_id,
            current_revision: result.current_revision,
            frames,
            record_kinds: &[],
            cumulative_credit: STATE_CREDIT,
            closed: false,
        })
    }

    /// Accept these family-reserved record kinds (FS `RECORD_MOVE`).
    pub(crate) fn with_record_kinds(mut self, kinds: &'static [u16]) -> Self {
        self.record_kinds = kinds;
        self
    }

    /// The subscription ID the server assigned.
    pub fn subscription_id(&self) -> u32 {
        self.subscription_id
    }

    /// The catalogue revision when the subscription started, updated as
    /// events are taken.
    pub fn revision(&self) -> u64 {
        self.current_revision
    }

    /// The next State event, acknowledged before it is returned.
    pub async fn next(&mut self) -> Result<StateEvent> {
        if self.closed {
            return Err(Error::Closed);
        }
        let frame: Frame = self.frames.recv().await.ok_or_else(|| {
            self.client
                .closed_reason()
                .unwrap_or_else(|| Error::disconnected("YAS State subscription ended"))
        })?;
        let event = StateEvent::decode_with(&frame.payload, 0, self.record_kinds)?;
        self.current_revision = event.to_revision;
        self.cumulative_credit = self
            .cumulative_credit
            .saturating_add(frame.payload.len() as u64);
        self.client.send_event(
            self.family,
            STATE_ACK_KIND,
            &StateAck {
                subscription_id: self.subscription_id,
                applied_revision: event.to_revision,
                cumulative_byte_limit: self.cumulative_credit,
            },
            crate::client::default_sensitive(self.family, Class::Event, STATE_ACK_KIND),
        )?;
        Ok(event)
    }

    /// Take events until the initial snapshot is complete and return its
    /// records (deltas folded in by the caller's decoder, in order).
    pub async fn snapshot(&mut self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        loop {
            let event = tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, self.next())
                .await
                .map_err(|_| {
                    Error::Timeout(format!(
                        "timed out waiting for family {:#06x} snapshot",
                        self.family
                    ))
                })??;
            match event.phase {
                Phase::SnapshotBegin => {
                    records.clear();
                    records.extend(event.records);
                }
                Phase::SnapshotRecords | Phase::Delta => records.extend(event.records),
                Phase::SnapshotEnd => {
                    records.extend(event.records);
                    return Ok(records);
                }
                Phase::Reset => {
                    return Err(Error::protocol(format!(
                        "family {:#06x} reset its snapshot",
                        self.family
                    )));
                }
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.client
            .release(Route::State(self.family, self.subscription_id));
        if let Ok(payload) = (Unwatch {
            subscription_id: self.subscription_id,
        })
        .encode()
        {
            self.client
                .send_detached(self.family, self.unwatch_kind, payload);
        }
    }
}

impl Client {
    /// Send a Request whose `Result` nobody waits for.
    pub(crate) fn send_detached(&self, family_id: u16, kind: u16, payload: Vec<u8>) {
        if !self.supports(family_id, Class::Request, kind) {
            return;
        }
        let mut header = FrameHeader::request(family_id, kind, self.next_request_id());
        header.sensitive = crate::client::default_sensitive(family_id, Class::Request, kind);
        let _ = self.send_frame(Frame { header, payload });
    }
}
