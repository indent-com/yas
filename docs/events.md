# YAS event tracing

The bounded server journal is available through `yas events`. Lifecycle events
are enabled by default. Enable detailed events at runtime:

```sh
yas events set --events 'default,+frame.native.*,+datagram.native.*,+state.record.*' --size 8388608
yas events tail --from-now
```

This covers every negotiated native family through the shared transport layer.
Events-family frames are excluded so a live event stream does not generate its
own next event. Existing PTY, compositor, supervisor, and server events remain
available with selectors such as `pty.*`, `compositor.*`, and `supervisor.*`.

## Trace meanings

| Selector                    | Meaning                                                                                                                                                                                                                                    |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `client.native.*`           | Negotiated session start, read/write/codec errors, session end with exit reason, elapsed time and reliable-stream byte counters. Enabled by default.                                                                                       |
| `frame.native.*`            | Successfully read or written reliable frame; family, operation, class, session/request/watch/trace IDs and exact byte counts. Results include status; State includes phase, revisions and count; ACKs include applied revision and credit. |
| `datagram.native.*`         | Decoded inbound datagram, accepted outbound datagram, or outbound queue drop. Queue acceptance is not a delivery receipt.                                                                                                                  |
| `state.record.*`            | Each typed State record at the transport boundary, including optional unknown kinds and Git QUERY_STATE's nested records.                                                                                                                  |
| `payload.native.*`          | Complete decoded frame bodies, including sensitive content, in bounded chunks.                                                                                                                                                             |
| `git.watch.*`, `fs.watch.*` | Watch lifecycle, canonical resource path, flags, session/resource/watch IDs and resolved settle delays. Enabled by default.                                                                                                                |
| `git.record`, `fs.record`   | Individual canonical State records before enqueue.                                                                                                                                                                                         |
| `git.state`, `fs.state`     | Update publication to the outbound queue, with revision, record count and byte count.                                                                                                                                                      |
| `git.fs_event`, `fs.event`  | Individual native filesystem notifications, including access notifications, rename paths, rescan requests and backend errors, before read-event filtering/coalescing.                                                                      |

Raw filesystem notifications are hints. Reconciliation and identical-snapshot
suppression may produce no State update. Shared roots fan notifications out to
each observing subscription. A callback already in flight when a watch stops
can finish after its stop event.

Use session + request ID to correlate requests and results. Trace IDs distinguish
frames, including unsolicited events and repeated ACKs. State traces also carry
subscription ID, phase, revisions and record index. A write trace confirms that
the reliable transport write completed; a Git/FS publication trace only confirms
enqueue. Connection tracing begins after successful negotiation.

For an exact capture, use `yas events tail --binary -o trace.events` or a detached
server-side recording (`yas events record start /path/trace.events`). Human
rendering decodes complete small State records and previews long binary chunks.
The ring and live stream remain bounded; evictions and stream gaps are reported
by journal counters/gap records. `yas events set --events default` restores
lifecycle-only tracing.

## Client watch controls

Manage → Clients displays canonical Git/FS paths, effective watch flags and
configured delays after server-default resolution. Git flags distinguish state
datasets/status selection from watched query kinds and query flags. Timing is
published in optional Client extension 4, keyed by family and subscription ID;
resource/flag details continue to use optional extension 3.

Git WATCH and WATCH_QUERY accept State WATCH extensions 1 (`refs_settle_ms:u16`)
and 2 (`status_settle_ms:u16`). Zero or omission selects the server default.
WATCH_QUERY derives its ref/status invalidation selection from the query itself.
The TypeScript wire API exposes `refsSettleMs` and `statusSettleMs`; workspace Git
state/log APIs expose `refsLatencyMs` and `statusLatencyMs`. Log watches inherit
repository timing options unless overridden. FS WATCH already exposes
`settle_ms`/`settleMs`, with zero selecting its default.

Shared engines use the minimum delay requested by their subscribers. The
Clients values describe each subscription's resolved configuration, so another
subscriber can make the shared engine run sooner.

## Native trace payload layout

All integers are little-endian. Stable IDs are in
`protocol/yas/families/events.toml`; the common native trace identity is:

```text
session_id:16 bytes, family:u16, operation:u16, class:u8, flags:u8,
request_id:u32, subscription_id:u32, decoded_payload_bytes:u32,
wire_bytes:u64, trace_id:u64
```

Flags: bit 0 sensitive, bit 1 compressed, bit 2 datagram. Missing IDs are zero.
Reliable wire bytes include the four-byte stream length prefix.

Frame events append a bounded semantic header where applicable: the four-byte
Result status/reserved prefix, the 26-byte State header, the 20-byte State ACK,
or a four-byte Transfer ID. Git QUERY_STATE uses the inner State header.
Payload events append `total_bytes:u32,offset:u32,chunk:remaining_bytes`.
State-record events append the first 24 bytes of the State header, then
`record_index:u16,record_kind:u16,record_flags:u16,total_body_bytes:u32,offset:u32,chunk:remaining_bytes`.
Chunks contain at most 2048 bytes; empty bodies have one empty chunk. Reassemble
by trace ID (and record index for State records). Native record capture preserves
optional unknown kinds without allocating a second full frame.

Git/FS pre-enqueue record events use
`session_id:16 bytes,resource_handle:u64,subscription_id:u32,revision:u64,watch_flags:u32,record_id:u64,record_kind:u16,record_flags:u16,total_body_bytes:u32,offset:u32,chunk:remaining_bytes`.
Their complete typed bodies use the same 2048-byte chunk bound; reassemble by
record ID. This keeps large watched-query values within live journal limits.
