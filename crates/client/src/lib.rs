//! A Rust client for YAS servers.
//!
//! `yas-client` connects to a YAS server (the same way the `yas` CLI does)
//! and drives its families from async Rust: non-PTY [processes](process),
//! [files](fs), the [key-value store](kv) and environment,
//! [terminals](terminal), GUI [surfaces](surface), and TCP/UDP flows the
//! server opens for it ([net]). On Unix it can also
//! [host] a private server as a child process, reachable through the
//! connections it hands out.
//!
//! # Connecting
//!
//! ```no_run
//! # async fn demo() -> yas_client::Result<()> {
//! use yas_client::{Client, ConnectOptions};
//!
//! // Any target the CLI accepts: local[:NAME], socket:PATH, ssh:[USER@]HOST,
//! // tcp:, ws(s)://, wt://, uplink:, share:, or a remote name.
//! let client = Client::connect(Some("ssh:build@ci.example"), &ConnectOptions::default()).await?;
//! println!("connected to {} {}", client.server_name(), client.hello().server_release);
//! # Ok(()) }
//! ```
//!
//! Over an existing byte stream (an SSH channel you opened, a socketpair end,
//! `docker exec -i CONTAINER yas connect --stdio`'s stdio):
//! [`Client::from_stream`]. With in-memory SSH keys and a pinned host key:
//! build a [`ssh::SshPool`] with [`ssh::SshOptions`] and pass it in
//! [`ConnectOptions::ssh`].
//!
//! # Concurrency
//!
//! A [`Client`] is one YAS session. It is `Clone + Send + Sync`: clones share
//! the session, and any number of calls, process streams, file transfers and
//! subscriptions run on it concurrently, each flow-controlled on its own (a
//! stalled reader of one process's stdout never blocks another's). Everything
//! a session creates that is not detachable (ordinary processes, open roots,
//! staged writes) ends when the session ends; see [`process`] for exactly
//! what that means for processes and their children.
//!
//! [`native::NativeClient`] is the lower-level, sequential session the CLI
//! uses for one-shot commands; [`Client`] is built on it.
//!
//! # Errors
//!
//! Every call returns [`Error`], which separates connection failures,
//! lost sessions, server statuses (`NotFound`, `Conflict`, …), timeouts,
//! catalogue gaps, and protocol violations. `impl From<Error> for String`
//! keeps `?` working in code that reports errors as strings.
//!
//! # Depending on it
//!
//! It lives in the YAS repository (`crates/client`) and is not on crates.io
//! yet; use a path or git dependency:
//!
//! ```toml
//! yas-client = { path = "../yas/crates/client" }
//! ```

mod client;
mod error;
mod options;

pub mod fs;
#[cfg(unix)]
pub mod host;
pub mod kv;
pub mod native;
pub mod net;
pub mod process;
pub mod state;
pub mod surface;
pub mod terminal;
pub mod transfer;
pub mod transport;

pub use client::{Client, DEFAULT_REQUEST_TIMEOUT};
pub use error::{Error, Result, format_result_detail};
pub use options::{ConnectOptions, HelloOptions, read_only_extension};

/// The producer side of YAS uplinks (`yas uplink`): publish a YAS server
/// through a relay, over WebTransport or WebSockets.
pub use yas_proxy::uplink_producer;
/// The wire codecs this crate speaks (`yas-wire`), for the escape hatches
/// ([`Client::request`], [`Client::request_raw`]).
pub use yas_wire as wire;

/// The embedded SSH client used for `ssh:` targets.
pub use yas_ssh as ssh;
