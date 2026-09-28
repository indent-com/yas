//! Non-PTY processes (the Process family).
//!
//! ```no_run
//! # async fn demo(client: yas_client::Client) -> yas_client::Result<()> {
//! use yas_client::process::{Command, Stdin};
//! use tokio::io::AsyncReadExt;
//!
//! let mut child = client
//!     .spawn(
//!         Command::new("bash")
//!             .arg("-c")
//!             .arg("echo hello; echo oops >&2; exit 3")
//!             .merge_stderr(true)
//!             .stdin(Stdin::Null)
//!             .current_dir("/tmp"),
//!     )
//!     .await?;
//! let mut output = String::new();
//! child.stdout().unwrap().read_to_string(&mut output).await.unwrap();
//! let status = child.wait().await?;
//! assert_eq!(status.code(), Some(3));
//! # Ok(()) }
//! ```
//!
//! # What happens to the processes a command leaves behind
//!
//! These are the server's rules (`crates/server/src/process.rs`), on Unix:
//!
//! - Every spawned child is the leader of a **new process group**
//!   (`setpgid(0, 0)`); it has no controlling terminal and inherits no file
//!   descriptor above 2.
//! - When the direct child exits, the server sends `SIGTERM` to its whole
//!   process group at once. Group members still holding the stdout/stderr
//!   pipes (`cmd &` in a shell script) keep the Transfers open; once every
//!   pipe closes, or after the kill grace (2 s by default,
//!   `YAS_PROCESS_KILL_GRACE`), whichever is first, the server sends
//!   `SIGKILL` to the group. So **background jobs do not outlive the command**
//!   unless they leave the process group (`setsid`, `setpgid`, a daemonizing
//!   double fork into a new session). Those escape and are never tracked.
//! - When the session that spawned an **ordinary** process closes (the
//!   [`Client`] is dropped, the connection breaks, the server shuts down),
//!   the group gets `SIGTERM`, then `SIGKILL` after the grace; the exit record
//!   says [`ExitReason::OwnerLost`].
//! - A **detachable** process ([`Command::detachable`]) survives its session:
//!   other sessions find it with [`Client::processes`] and [`Client::attach`]
//!   to it (output produced while nobody was attached is lost, not replayed).
//!   Its final exit record is kept for at most 5 minutes after it exits.
//! - A server restart ends every tracked process; an unclean server death
//!   can leave processes running unless the deployment adds cgroups or a
//!   parent-death signal.
//!
//! On Windows the child runs in a kill-on-close job object that contains its
//! whole tree; `Terminate` sends `CTRL_BREAK`, `Kill` terminates the job.
//!
//! Operation IDs deduplicate `SPAWN` and `CONTROL` **within one session**: if
//! a call times out locally, resending the same [`Command`] (same
//! [`Command::operation_id`]) on the same [`Client`] returns the original
//! process instead of starting a second one. A new session does not see the
//! old session's operation IDs (and its ordinary processes are gone anyway).

use std::ffi::OsStr;
use std::time::Duration;

use yas_wire::{
    Encode, Extensions,
    core::ResultPrefix,
    family,
    process::{
        self as wire, Attach, Control, ControlAction, ControlResult, Cwd, EnvEntry,
        EnvironmentKind, ExitKind, ExitRecord, ProcessRecord, RemovedProcess, Spawn, StreamBundle,
        Wait, request_kind,
    },
    schema::process as schema,
    state::{Phase, RecordKind, Watch as StateWatch},
};

use crate::client::{Client, DEFAULT_REQUEST_TIMEOUT, Hook, Reply, Route};
use crate::error::{Error, Result};
use crate::state::{STATE_CREDIT, Subscription};
use crate::transfer::{ByteSink, ByteStream, DEFAULT_WINDOW};

/// What the child's stdin is connected to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stdin {
    /// End of file from the start (the Transfer is closed right after spawn).
    #[default]
    Null,
    /// A [`ByteSink`] the caller writes to ([`Process::stdin`]).
    Piped,
}

/// Where the child starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WorkingDirectory {
    /// The server's own working directory.
    #[default]
    ServerDefault,
    /// A platform path (absolute, or relative to the server's directory).
    Path(Vec<u8>),
    /// The current directory of a YAS terminal.
    Terminal(u64),
}

/// A process to spawn: argv executed directly (no implicit shell).
#[derive(Clone, Debug)]
pub struct Command {
    argv: Vec<Vec<u8>>,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    clear_env: bool,
    cwd: WorkingDirectory,
    merge_stderr: bool,
    detachable: bool,
    stdin: Stdin,
    window: u64,
    operation_id: [u8; 16],
}

impl Command {
    /// A command running `program` (looked up in the server's `PATH`).
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            argv: vec![os_bytes(program.as_ref())],
            env: Vec::new(),
            clear_env: false,
            cwd: WorkingDirectory::ServerDefault,
            merge_stderr: false,
            detachable: false,
            stdin: Stdin::Null,
            window: DEFAULT_WINDOW,
            operation_id: nonzero_id(),
        }
    }

    /// Append one argument (bytes preserved on Unix).
    pub fn arg(&mut self, argument: impl AsRef<OsStr>) -> &mut Self {
        self.argv.push(os_bytes(argument.as_ref()));
        self
    }

    /// Append arguments.
    pub fn args<I, S>(&mut self, arguments: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for argument in arguments {
            self.arg(argument);
        }
        self
    }

    /// Set one environment variable. Explicit entries replace inherited ones.
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.env
            .push((os_bytes(key.as_ref()), os_bytes(value.as_ref())));
        self
    }

    /// Set several environment variables.
    pub fn envs<I, K, V>(&mut self, pairs: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, value) in pairs {
            self.env(key, value);
        }
        self
    }

    /// Start from an empty environment instead of the server's session
    /// environment (the server's own environment plus the compositor's
    /// display variables when it runs one).
    pub fn env_clear(&mut self) -> &mut Self {
        self.clear_env = true;
        self
    }

    /// Start in `directory` on the server.
    pub fn current_dir(&mut self, directory: impl AsRef<OsStr>) -> &mut Self {
        self.cwd = WorkingDirectory::Path(os_bytes(directory.as_ref()));
        self
    }

    /// Choose the working directory source.
    pub fn working_directory(&mut self, directory: WorkingDirectory) -> &mut Self {
        self.cwd = directory;
        self
    }

    /// Send stderr into the stdout stream (one ordered stream, like `2>&1`).
    pub fn merge_stderr(&mut self, merge: bool) -> &mut Self {
        self.merge_stderr = merge;
        self
    }

    /// Let the process outlive this session (see the module docs).
    pub fn detachable(&mut self, detachable: bool) -> &mut Self {
        self.detachable = detachable;
        self
    }

    /// Connect stdin.
    pub fn stdin(&mut self, stdin: Stdin) -> &mut Self {
        self.stdin = stdin;
        self
    }

    /// Receive window per output stream in bytes (default 1 MiB): how much
    /// output the server may send ahead of the reader.
    pub fn window(&mut self, bytes: u64) -> &mut Self {
        self.window = bytes.max(1);
        self
    }

    /// Set the 128-bit operation ID (nonzero). Resending a `SPAWN` with the
    /// same ID on the same session returns the first process.
    pub fn operation_id(&mut self, id: [u8; 16]) -> &mut Self {
        if id.iter().any(|byte| *byte != 0) {
            self.operation_id = id;
        }
        self
    }

    /// The operation ID this command will spawn with.
    pub fn current_operation_id(&self) -> [u8; 16] {
        self.operation_id
    }

    fn to_wire(&self) -> Result<Spawn> {
        let mut flags = 0u16;
        if self.merge_stderr {
            flags |= schema::SPAWN_MERGE_STDERR as u16;
        }
        if self.detachable {
            flags |= schema::SPAWN_DETACHABLE as u16;
        }
        if self.argv.first().is_none_or(Vec::is_empty) {
            return Err(Error::invalid("process argv[0] must not be empty"));
        }
        Ok(Spawn {
            operation_id: self.operation_id,
            flags,
            environment_kind: if self.clear_env {
                EnvironmentKind::Empty
            } else {
                EnvironmentKind::Session
            },
            cwd: match &self.cwd {
                WorkingDirectory::ServerDefault => Cwd::ServerDefault,
                WorkingDirectory::Path(path) => Cwd::Path(path.clone()),
                WorkingDirectory::Terminal(id) => Cwd::Terminal(*id),
            },
            argv: self.argv.clone(),
            env: self
                .env
                .iter()
                .map(|(key, value)| EnvEntry {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
            stdout_receive_credit: self.window,
            stderr_receive_credit: if self.merge_stderr { 0 } else { self.window },
            extensions: Extensions::default(),
        })
    }
}

/// A signal [`Process::signal`] can deliver (portable subset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// `SIGINT`.
    Interrupt,
    /// `SIGTERM`.
    Terminate,
    /// `SIGKILL`.
    Kill,
    /// `SIGHUP`.
    Hangup,
}

impl Signal {
    fn wire(self) -> u16 {
        (match self {
            Self::Interrupt => schema::SIGNAL_INTERRUPT,
            Self::Terminate => schema::SIGNAL_TERMINATE,
            Self::Kill => schema::SIGNAL_KILL,
            Self::Hangup => schema::SIGNAL_HANGUP,
        }) as u16
    }
}

/// Why a process ended, beyond its code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// It returned or was signalled by something else.
    Unknown,
    /// `SIGINT`.
    Interrupt,
    /// `SIGTERM`.
    Terminate,
    /// `SIGKILL` from outside YAS.
    Kill,
    /// `SIGHUP`.
    Hangup,
    /// A client asked (`Kill`, or `Terminate` escalated).
    Client,
    /// Its owning session went away (ordinary processes only).
    OwnerLost,
    /// `Terminate` did not finish within the kill grace, so it was killed.
    TerminateTimeout,
    /// The server shut down.
    ServerShutdown,
    /// A reason this client does not know.
    Other(u8),
}

impl ExitReason {
    fn from_wire(value: u8) -> Self {
        match u64::from(value) {
            schema::EXIT_REASON_UNKNOWN => Self::Unknown,
            schema::EXIT_REASON_INTERRUPT => Self::Interrupt,
            schema::EXIT_REASON_TERMINATE => Self::Terminate,
            schema::EXIT_REASON_KILL => Self::Kill,
            schema::EXIT_REASON_HANGUP => Self::Hangup,
            schema::EXIT_REASON_CLIENT => Self::Client,
            schema::EXIT_REASON_OWNER_LOST => Self::OwnerLost,
            schema::EXIT_REASON_TERMINATE_TIMEOUT => Self::TerminateTimeout,
            schema::EXIT_REASON_SERVER_SHUTDOWN => Self::ServerShutdown,
            _ => Self::Other(value),
        }
    }
}

/// How a process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitStatus {
    /// Returned a code, died of a signal, was killed by YAS, or other.
    pub kind: ExitKind,
    /// Why.
    pub reason: ExitReason,
    /// Exit code for [`ExitKind::Code`], signal number for
    /// [`ExitKind::Signal`], else 0.
    pub raw_code: i32,
    /// Server monotonic time of the exit, in nanoseconds.
    pub exited_server_ns: u64,
    /// Human-readable detail (for [`ExitKind::Other`]: what went wrong).
    pub detail: String,
}

impl ExitStatus {
    fn from_wire(record: ExitRecord) -> Self {
        Self {
            kind: record.kind,
            reason: ExitReason::from_wire(record.reason),
            raw_code: record.code,
            exited_server_ns: record.exited_server_ns,
            detail: String::from_utf8_lossy(&record.detail).into_owned(),
        }
    }

    /// Whether it returned 0.
    pub fn success(&self) -> bool {
        self.code() == Some(0)
    }

    /// The exit code, if it returned one.
    pub fn code(&self) -> Option<i32> {
        (self.kind == ExitKind::Code).then_some(self.raw_code)
    }

    /// The signal number, if a signal ended it.
    pub fn signal(&self) -> Option<i32> {
        (self.kind == ExitKind::Signal).then_some(self.raw_code)
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            ExitKind::Code => write!(f, "exit code {}", self.raw_code),
            ExitKind::Signal => write!(f, "signal {} ({:?})", self.raw_code, self.reason),
            ExitKind::Killed => write!(f, "killed ({:?})", self.reason),
            ExitKind::Other => write!(f, "ended: {}", self.detail),
        }
    }
}

/// Collected output of [`Process::output`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// How it ended.
    pub status: ExitStatus,
    /// Everything it wrote to stdout (and stderr, when merged).
    pub stdout: Vec<u8>,
    /// Everything it wrote to stderr (empty when merged).
    pub stderr: Vec<u8>,
}

/// A running (or finished) process and the streams this session holds.
#[derive(Debug)]
pub struct Process {
    client: Client,
    handle: u64,
    stdin: Option<ByteSink>,
    stdout: Option<ByteStream>,
    stderr: Option<ByteStream>,
    stdout_offset: u64,
    stderr_offset: u64,
    merged_stderr: bool,
}

impl Process {
    /// The boot-scoped process handle (valid on any session of this server
    /// boot, for [`Client::attach`], [`Client::wait_process`], …).
    pub fn handle(&self) -> u64 {
        self.handle
    }

    /// Whether stderr is merged into stdout.
    pub fn merged_stderr(&self) -> bool {
        self.merged_stderr
    }

    /// Lifetime byte offsets at which this attachment's stdout and stderr
    /// begin (0 for a fresh spawn; later for [`Client::attach`]: earlier
    /// output is a gap).
    pub fn offsets(&self) -> (u64, u64) {
        (self.stdout_offset, self.stderr_offset)
    }

    /// Stdin, if piped and not taken.
    pub fn stdin(&mut self) -> Option<&mut ByteSink> {
        self.stdin.as_mut()
    }

    /// Stdout, if not taken.
    pub fn stdout(&mut self) -> Option<&mut ByteStream> {
        self.stdout.as_mut()
    }

    /// Stderr, if separate and not taken.
    pub fn stderr(&mut self) -> Option<&mut ByteStream> {
        self.stderr.as_mut()
    }

    /// Take ownership of stdin.
    pub fn take_stdin(&mut self) -> Option<ByteSink> {
        self.stdin.take()
    }

    /// Take ownership of stdout.
    pub fn take_stdout(&mut self) -> Option<ByteStream> {
        self.stdout.take()
    }

    /// Take ownership of stderr.
    pub fn take_stderr(&mut self) -> Option<ByteStream> {
        self.stderr.take()
    }

    /// Wait for the process to exit.
    pub async fn wait(&self) -> Result<ExitStatus> {
        self.client
            .wait_process(self.handle, None)
            .await?
            .ok_or_else(|| Error::protocol("Process WAIT without timeout timed out"))
    }

    /// Wait at most `timeout`; `None` if it is still running.
    pub async fn wait_timeout(&self, timeout: Duration) -> Result<Option<ExitStatus>> {
        self.client.wait_process(self.handle, Some(timeout)).await
    }

    /// Deliver a signal to the process group.
    pub async fn signal(&self, signal: Signal) -> Result<()> {
        self.client
            .control_process(self.handle, ControlAction::Signal, signal.wire())
            .await
    }

    /// `SIGTERM` the group, escalating to `SIGKILL` after the server's kill
    /// grace.
    pub async fn terminate(&self) -> Result<()> {
        self.client
            .control_process(self.handle, ControlAction::Terminate, 0)
            .await
    }

    /// `SIGKILL` the group now.
    pub async fn kill(&self) -> Result<()> {
        self.client
            .control_process(self.handle, ControlAction::Kill, 0)
            .await
    }

    /// Make a detachable process independent of this session's attachment.
    pub async fn detach(&self) -> Result<()> {
        self.client
            .control_process(self.handle, ControlAction::Detach, 0)
            .await
    }

    /// Close stdin (if piped), collect stdout and stderr (each at most
    /// `limit` bytes), and wait for the exit.
    pub async fn output_limited(mut self, limit: u64) -> Result<Output> {
        if let Some(stdin) = self.stdin.take() {
            stdin.finish().await?;
        }
        let stdout = self.stdout.take();
        let stderr = self.stderr.take();
        let (stdout, stderr) = tokio::try_join!(
            async {
                match stdout {
                    Some(stream) => stream.read_to_end(limit).await,
                    None => Ok(Vec::new()),
                }
            },
            async {
                match stderr {
                    Some(stream) => stream.read_to_end(limit).await,
                    None => Ok(Vec::new()),
                }
            }
        )?;
        let status = self.wait().await?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// [`Process::output_limited`] with a 256 MiB limit per stream.
    pub async fn output(self) -> Result<Output> {
        self.output_limited(256 * 1024 * 1024).await
    }
}

/// One entry of the server-wide process catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Boot-scoped handle.
    pub handle: u64,
    /// Whether it is still running.
    pub running: bool,
    /// Survives its owner session.
    pub detachable: bool,
    /// Stderr merged into stdout.
    pub merged_stderr: bool,
    /// Stdin still open / stdout open / stderr open / stdin claimed.
    pub stdin_open: bool,
    /// See `stdin_open`.
    pub stdout_open: bool,
    /// See `stdin_open`.
    pub stderr_open: bool,
    /// Some session holds stdin.
    pub stdin_claimed: bool,
    /// OS process ID (diagnostics only).
    pub native_pid: u64,
    /// Session that spawned it.
    pub owner_session: [u8; 16],
    /// `argv[0]` (later arguments and the environment are not published).
    pub argv0: Vec<u8>,
    /// Lifetime stream counters.
    pub stdin_received: u64,
    /// See `stdin_received`.
    pub stdout_produced: u64,
    /// See `stdin_received`.
    pub stderr_produced: u64,
    /// For exited detachable processes: when the record is dropped (server
    /// monotonic ns).
    pub retention_deadline_server_ns: u64,
    /// How it ended, once it has.
    pub exit: Option<ExitStatus>,
}

impl ProcessInfo {
    fn from_wire(record: ProcessRecord) -> Self {
        let state = u64::from(record.stream_state);
        let flags = u64::from(record.flags);
        Self {
            handle: record.process_handle,
            running: u64::from(record.lifecycle) == schema::LIFECYCLE_RUNNING,
            detachable: flags & schema::SPAWN_DETACHABLE != 0,
            merged_stderr: flags & schema::SPAWN_MERGE_STDERR != 0,
            stdin_open: state & schema::STREAM_STDIN_OPEN != 0,
            stdout_open: state & schema::STREAM_STDOUT_OPEN != 0,
            stderr_open: state & schema::STREAM_STDERR_OPEN != 0,
            stdin_claimed: state & schema::STREAM_STDIN_CLAIMED != 0,
            native_pid: record.native_pid,
            owner_session: record.owner_session,
            argv0: record.argv0,
            stdin_received: record.stdin_received,
            stdout_produced: record.stdout_produced,
            stderr_produced: record.stderr_produced,
            retention_deadline_server_ns: record.retention_deadline_server_ns,
            exit: record.exit.map(ExitStatus::from_wire),
        }
    }
}

/// A change in the process catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessChange {
    /// The complete catalogue (first event, and after a server-side reset).
    Snapshot(Vec<ProcessInfo>),
    /// A process appeared or changed.
    Updated(ProcessInfo),
    /// A process record was dropped.
    Removed(u64),
}

/// A live view of the process catalogue.
#[derive(Debug)]
pub struct ProcessWatch {
    subscription: Subscription,
    snapshot: Vec<ProcessInfo>,
    in_snapshot: bool,
    pending: std::collections::VecDeque<ProcessChange>,
}

impl ProcessWatch {
    /// The next change.
    pub async fn next(&mut self) -> Result<ProcessChange> {
        loop {
            if let Some(change) = self.pending.pop_front() {
                return Ok(change);
            }
            let event = self.subscription.next().await?;
            match event.phase {
                Phase::SnapshotBegin => {
                    self.snapshot.clear();
                    self.in_snapshot = true;
                    self.snapshot.extend(decode_upserts(&event.records)?);
                }
                Phase::SnapshotRecords if self.in_snapshot => {
                    self.snapshot.extend(decode_upserts(&event.records)?);
                }
                Phase::SnapshotEnd => {
                    self.snapshot.extend(decode_upserts(&event.records)?);
                    self.in_snapshot = false;
                    return Ok(ProcessChange::Snapshot(std::mem::take(&mut self.snapshot)));
                }
                Phase::Reset => {
                    self.snapshot.clear();
                    self.in_snapshot = true;
                }
                Phase::Delta | Phase::SnapshotRecords => {
                    for record in &event.records {
                        if let Some(change) = change_from_record(record)? {
                            self.pending.push_back(change);
                        }
                    }
                }
            }
        }
    }
}

fn decode_upserts(records: &[yas_wire::state::Record]) -> Result<Vec<ProcessInfo>> {
    records
        .iter()
        .filter(|record| matches!(record.kind, RecordKind::Add | RecordKind::Replace))
        .map(|record| {
            Ok(ProcessInfo::from_wire(ProcessRecord::from_state_record(
                record,
            )?))
        })
        .collect()
}

fn change_from_record(record: &yas_wire::state::Record) -> Result<Option<ProcessChange>> {
    match record.kind {
        RecordKind::Remove => {
            use yas_wire::Decode;
            Ok(Some(ProcessChange::Removed(
                RemovedProcess::decode(&record.body)?.process_handle,
            )))
        }
        RecordKind::Add | RecordKind::Replace => Ok(Some(ProcessChange::Updated(
            ProcessInfo::from_wire(ProcessRecord::from_state_record(record)?),
        ))),
        _ if record.required => Err(Error::protocol(
            "Process sent a required State record this client does not know",
        )),
        _ => Ok(None),
    }
}

fn bundle_hook() -> Hook {
    Box::new(|prefix: &ResultPrefix| {
        use yas_wire::Decode;
        let Ok(bundle) = StreamBundle::decode(&prefix.body) else {
            return Vec::new();
        };
        let mut routes = vec![Route::Transfer(bundle.stdout.transfer_id)];
        if let Some(stdin) = &bundle.stdin {
            routes.push(Route::Transfer(stdin.transfer_id));
        }
        if let Some(stderr) = &bundle.stderr {
            routes.push(Route::Transfer(stderr.transfer_id));
        }
        routes
    })
}

impl Client {
    /// Spawn a process. See the [module docs](crate::process) for what
    /// happens to it and its children.
    pub async fn spawn(&self, command: &Command) -> Result<Process> {
        let spawn = command.to_wire()?;
        let reply = self
            .call_ok(
                family::PROCESS,
                request_kind::SPAWN,
                spawn.encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                Some(bundle_hook()),
            )
            .await?;
        let mut process = self.process_from_reply(reply, command.window)?;
        if command.stdin == Stdin::Null
            && let Some(stdin) = process.stdin.take()
        {
            stdin.finish().await?;
        }
        Ok(process)
    }

    /// Attach to an existing process (any session's), receiving output from
    /// now on. With `stdin`, claim its stdin (at most one attachment can).
    pub async fn attach(&self, handle: u64, stdin: bool) -> Result<Process> {
        let merged = self
            .processes()
            .await?
            .into_iter()
            .find(|info| info.handle == handle)
            .is_some_and(|info| info.merged_stderr);
        let attach = Attach {
            process_handle: handle,
            flags: if stdin {
                schema::ATTACH_STDIN as u16
            } else {
                0
            },
            stdout_receive_credit: DEFAULT_WINDOW,
            stderr_receive_credit: if merged { 0 } else { DEFAULT_WINDOW },
            extensions: Extensions::default(),
        };
        let reply = self
            .call_ok(
                family::PROCESS,
                request_kind::ATTACH,
                attach.encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                Some(bundle_hook()),
            )
            .await?;
        self.process_from_reply(reply, DEFAULT_WINDOW)
    }

    fn process_from_reply(&self, mut reply: Reply, window: u64) -> Result<Process> {
        use yas_wire::Decode;
        let bundle = StreamBundle::decode(&reply.prefix.body)?;
        let route = |reply: &mut Reply, id: u32| {
            reply
                .take(Route::Transfer(id))
                .ok_or_else(|| Error::protocol("Process stream route missing"))
        };
        let stdout_frames = route(&mut reply, bundle.stdout.transfer_id)?;
        let stdout = ByteStream::new(self.clone(), bundle.stdout, stdout_frames, window)?;
        let stderr = match bundle.stderr {
            Some(descriptor) => {
                let frames = route(&mut reply, descriptor.transfer_id)?;
                Some(ByteStream::new(self.clone(), descriptor, frames, window)?)
            }
            None => None,
        };
        let stdin = match bundle.stdin {
            Some(descriptor) => {
                let frames = route(&mut reply, descriptor.transfer_id)?;
                Some(ByteSink::new(self.clone(), descriptor, frames)?)
            }
            None => None,
        };
        Ok(Process {
            client: self.clone(),
            handle: bundle.process_handle,
            stdin,
            stdout: Some(stdout),
            stderr,
            stdout_offset: bundle.stdout_lifetime_offset,
            stderr_offset: bundle.stderr_lifetime_offset,
            merged_stderr: bundle.merged_stderr,
        })
    }

    /// Wait for any process of this boot to exit; `None` on timeout.
    pub async fn wait_process(
        &self,
        handle: u64,
        timeout: Option<Duration>,
    ) -> Result<Option<ExitStatus>> {
        let timeout_ns = timeout.map_or(0, |timeout| {
            u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX).max(1)
        });
        let reply = self
            .call(
                family::PROCESS,
                request_kind::WAIT,
                Wait {
                    process_handle: handle,
                    timeout_ns,
                    extensions: Extensions::default(),
                }
                .encode()?,
                timeout.map(|timeout| timeout + DEFAULT_REQUEST_TIMEOUT),
                None,
            )
            .await?;
        use yas_wire::Decode;
        match reply.prefix.status {
            yas_wire::core::Status::Ok => Ok(Some(ExitStatus::from_wire(ExitRecord::decode(
                &reply.prefix.body,
            )?))),
            yas_wire::core::Status::Timeout => Ok(None),
            status => Err(Error::status_from(
                "Process WAIT",
                status,
                reply.prefix.detail,
            )),
        }
    }

    pub(crate) async fn control_process(
        &self,
        handle: u64,
        action: ControlAction,
        value: u16,
    ) -> Result<()> {
        let _: ControlResult = self
            .request(
                family::PROCESS,
                request_kind::CONTROL,
                &Control {
                    process_handle: handle,
                    operation_id: nonzero_id(),
                    action,
                    value,
                    extensions: Extensions::default(),
                },
            )
            .await?;
        Ok(())
    }

    /// Signal, terminate, kill or detach any process of this boot by handle.
    pub async fn signal_process(&self, handle: u64, signal: Signal) -> Result<()> {
        self.control_process(handle, ControlAction::Signal, signal.wire())
            .await
    }

    /// `SIGTERM`, escalating to `SIGKILL` after the kill grace.
    pub async fn terminate_process(&self, handle: u64) -> Result<()> {
        self.control_process(handle, ControlAction::Terminate, 0)
            .await
    }

    /// `SIGKILL` now.
    pub async fn kill_process(&self, handle: u64) -> Result<()> {
        self.control_process(handle, ControlAction::Kill, 0).await
    }

    /// The server-wide process catalogue right now.
    pub async fn processes(&self) -> Result<Vec<ProcessInfo>> {
        let mut watch = self.watch_processes().await?;
        match watch.next().await? {
            ProcessChange::Snapshot(processes) => Ok(processes),
            _ => Err(Error::protocol(
                "Process WATCH did not start with a snapshot",
            )),
        }
    }

    /// Watch the process catalogue: a [`ProcessChange::Snapshot`] first,
    /// then changes.
    pub async fn watch_processes(&self) -> Result<ProcessWatch> {
        let subscription = Subscription::open(
            self,
            family::PROCESS,
            request_kind::WATCH,
            request_kind::UNWATCH,
            StateWatch {
                initial_credit: STATE_CREDIT,
                resume: None,
                extensions: Extensions::default(),
            }
            .encode()?,
        )
        .await?;
        Ok(ProcessWatch {
            subscription,
            snapshot: Vec::new(),
            in_snapshot: false,
            pending: std::collections::VecDeque::new(),
        })
    }

    /// The Process family limits this session negotiated.
    pub fn process_limits(&self) -> Option<wire::Limits> {
        self.family_limits(family::PROCESS)
            .and_then(|limits| wire::Limits::from_extensions(&limits).ok())
    }

    pub(crate) fn family_limits(&self, family_id: u16) -> Option<Extensions> {
        self.hello()
            .families
            .into_iter()
            .find(|descriptor| descriptor.family_id == family_id)
            .map(|descriptor| descriptor.limits)
    }
}

pub(crate) fn nonzero_id() -> [u8; 16] {
    loop {
        let value: [u8; 16] = rand::random();
        if value.iter().any(|byte| *byte != 0) {
            return value;
        }
    }
}

pub(crate) fn os_bytes(value: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        value.to_string_lossy().into_owned().into_bytes()
    }
}
