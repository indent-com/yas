//! Host a private YAS server as a child process (Unix).
//!
//! [`HostedServer::start`] runs `yas server --fd-channel FD` as a child and
//! keeps the other end of that channel. Every [`HostedServer::connect`]
//! creates a fresh socketpair, passes one end to the server over the channel
//! (`SCM_RIGHTS`), and speaks YAS on the other: no listening socket has to be
//! found, raced, or protected for those sessions. On macOS sessions use the
//! server's private socket instead (see [macOS](#macos)).
//!
//! ```no_run
//! # async fn demo() -> yas_client::Result<()> {
//! use yas_client::host::{HostOptions, HostedServer};
//! use yas_client::process::Command;
//!
//! let server = HostedServer::start(HostOptions::new("yas").name("demo")).await?;
//! let client = server.connect().await?;
//! let output = client.spawn(&Command::new("uname").arg("-a")).await?.output().await?;
//! print!("{}", String::from_utf8_lossy(&output.stdout));
//! server.shutdown().await?;
//! # Ok(()) }
//! ```
//!
//! # Lifetime
//!
//! The server lives exactly as long as the channel. Dropping the
//! [`HostedServer`] (or [`HostedServer::shutdown`]) closes it, and the
//! server takes its ordinary shutdown path: sessions end, their ordinary
//! processes are killed (see [`crate::process`]), and it exits. If this
//! process dies, the kernel closes the channel and the same happens, so a
//! hosted server never outlives its host by more than its shutdown grace.
//! The child runs in its own process group, so a terminal's Ctrl-C reaches
//! the host, not the server.
//!
//! # Isolation
//!
//! Each hosted server has a private directory (mode 0700): a fresh temporary
//! directory removed when the server is gone, or [`HostOptions::root`].
//! It holds the server's Unix socket (`yas.sock`, or
//! [`HostOptions::socket`]), its log (`server.log`) and, with
//! [`Isolation::Private`] (the default), its state, cache and runtime
//! directories (`XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR`), so
//! it shares nothing with the user's own YAS servers. The server name
//! (`--name`) is unique unless set.
//!
//! The server also listens on that socket, because YAS servers always do;
//! [`HostedServer::socket_path`] exposes it for tools that need a `YAS_SOCK`
//! (`yas uplink`, a `yas` CLI run inside a hosted process). Only this user
//! can reach it.
//!
//! # macOS
//!
//! On macOS every session connects to that private socket rather than being
//! passed over the channel, which still bounds the server's lifetime. XNU's
//! unix-socket garbage collector only scans the queues of sockets that are
//! themselves in flight, so a passed socket whose other descriptors are
//! closed looks unreachable while it waits in the server's end of the
//! channel: whenever any unix socket on the machine closes before the server
//! takes it, the collector flushes it, and the session reads EOF (its peer
//! gets `EPIPE`). A server that is still starting, or is busy, loses
//! sessions that way.
//!
//! # Environment
//!
//! The server starts with this process's environment, minus the variables
//! that would point it at another server or credential (`YAS_SOCK`,
//! `YAS_SERVER_NAME`, `YAS_FD_CHANNEL`, `YAS_TARGET`, `YAS_PASSPHRASE`),
//! plus the isolation variables above, `YAS_SKIP_COMPOSITOR=1` and
//! `YAS_RELAY=0` unless [`HostOptions::compositor`] / [`HostOptions::relay`]
//! turn them back on. [`HostOptions::env_clear`], [`HostOptions::env`] and
//! [`HostOptions::env_remove`] make it explicit. Processes the server spawns
//! inherit its environment (unless their [`Command`](crate::process::Command)
//! clears it), including the private `XDG_*` directories under
//! [`Isolation::Private`]; use [`Isolation::Shared`] when commands must see
//! the user's own.

use std::ffi::OsString;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::native::NativeClient;
use crate::transport::Transport;
use crate::{Client, HelloOptions};

/// How long [`HostedServer::start`] waits for the first HELLO by default.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(20);

/// How long [`HostedServer::shutdown`] waits for the server to exit after the
/// channel closes before killing it, by default.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

/// Environment variables removed from an inherited environment.
const REDIRECTING_VARIABLES: &[&str] = &[
    "YAS_SOCK",
    "YAS_SERVER_NAME",
    "YAS_FD_CHANNEL",
    "YAS_TARGET",
    "YAS_PASSPHRASE",
];

/// Which directories a hosted server keeps its state in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Isolation {
    /// State, cache and runtime directories inside the private directory:
    /// nothing is shared with other YAS servers, and nothing survives the
    /// private directory.
    #[default]
    Private,
    /// The user's own XDG directories, with state keyed by the server name
    /// (`--name`), so a stable name keeps KV and extension state across
    /// restarts. Processes see the user's real `XDG_*` variables.
    Shared,
}

/// Where the server's stderr goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerLog {
    /// Append to `server.log` in the private directory.
    #[default]
    File,
    /// Inherit this process's stderr.
    Inherit,
    /// Discard.
    Null,
}

/// How to start a [`HostedServer`].
#[derive(Clone, Debug)]
pub struct HostOptions {
    executable: PathBuf,
    name: Option<String>,
    root: Option<PathBuf>,
    socket: Option<PathBuf>,
    isolation: Isolation,
    compositor: bool,
    relay: bool,
    env_clear: bool,
    env: Vec<(OsString, Option<OsString>)>,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
    log: ServerLog,
    hello: HelloOptions,
    start_timeout: Duration,
}

impl HostOptions {
    /// Start `executable` (a `yas` binary; a bare name is looked up in
    /// `PATH`) with the defaults described in the [module docs](self).
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            name: None,
            root: None,
            socket: None,
            isolation: Isolation::default(),
            compositor: false,
            relay: false,
            env_clear: false,
            env: Vec::new(),
            args: Vec::new(),
            current_dir: None,
            log: ServerLog::default(),
            hello: HelloOptions::named("yas-client-host"),
            start_timeout: DEFAULT_START_TIMEOUT,
        }
    }

    /// The server name (`--name`: letters, digits, `-`, `_`, `.`). Defaults
    /// to a unique `hosted-<random>`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Use `root` as the private directory (created with mode 0700 if
    /// missing, kept afterwards) instead of a temporary one.
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Bind the server's socket at `path` instead of `yas.sock` in the
    /// private directory. Its directory should be private to this user.
    pub fn socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket = Some(path.into());
        self
    }

    /// Where state lives; see [`Isolation`].
    pub fn isolation(mut self, isolation: Isolation) -> Self {
        self.isolation = isolation;
        self
    }

    /// Bring up the surface compositor (off by default: `YAS_SKIP_COMPOSITOR=1`).
    pub fn compositor(mut self, enabled: bool) -> Self {
        self.compositor = enabled;
        self
    }

    /// Let the server join the WebRTC relay/signalling hub (off by default:
    /// `YAS_RELAY=0`).
    pub fn relay(mut self, enabled: bool) -> Self {
        self.relay = enabled;
        self
    }

    /// Start from an empty environment instead of this process's.
    pub fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self.env.clear();
        self
    }

    /// Set one environment variable for the server.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), Some(value.into())));
        self
    }

    /// Set several environment variables for the server.
    pub fn envs<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        for (key, value) in vars {
            self.env.push((key.into(), Some(value.into())));
        }
        self
    }

    /// Remove one environment variable.
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env.push((key.into(), None));
        self
    }

    /// Append an argument to `yas server …` (for example
    /// `--no-persistent-extensions`, `--export-sock`, `--max-ptys N`).
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append arguments to `yas server …`.
    pub fn args<A: Into<OsString>>(mut self, args: impl IntoIterator<Item = A>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// The server's working directory (also the default for what it spawns).
    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    /// Where the server's stderr goes; see [`ServerLog`].
    pub fn log(mut self, log: ServerLog) -> Self {
        self.log = log;
        self
    }

    /// The HELLO [`HostedServer::connect`] sends.
    pub fn hello(mut self, hello: HelloOptions) -> Self {
        self.hello = hello;
        self
    }

    /// How long [`HostedServer::start`] waits for the server to answer.
    pub fn start_timeout(mut self, timeout: Duration) -> Self {
        self.start_timeout = timeout;
        self
    }
}

enum Root {
    Temporary(tempfile::TempDir),
    Kept(PathBuf),
}

impl Root {
    fn path(&self) -> &Path {
        match self {
            Self::Temporary(dir) => dir.path(),
            Self::Kept(path) => path,
        }
    }
}

/// A private YAS server running as a child of this process. See the
/// [module docs](self).
pub struct HostedServer {
    channel: Mutex<Option<UnixStream>>,
    child: Option<std::process::Child>,
    root: Option<Root>,
    root_path: PathBuf,
    socket: PathBuf,
    name: String,
    hello: HelloOptions,
}

impl std::fmt::Debug for HostedServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostedServer")
            .field("name", &self.name)
            .field("pid", &self.child.as_ref().map(std::process::Child::id))
            .field("root", &self.root_path)
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl HostedServer {
    /// Start the server and wait until it answers a HELLO and its socket
    /// exists.
    ///
    /// Fails with [`Error::Connect`] (including the tail of its log) when
    /// the server cannot start, and [`Error::Invalid`] for a bad name.
    pub async fn start(options: HostOptions) -> Result<Self> {
        let name = match &options.name {
            Some(name) => name.clone(),
            None => format!("hosted-{:016x}", rand::random::<u64>()),
        };
        if !yas_webserver::config::valid_server_name(&name) {
            return Err(Error::invalid(format!("invalid YAS server name {name:?}")));
        }
        let root = match &options.root {
            Some(path) => {
                create_private_dir(path)
                    .map_err(|error| io_connect("cannot create the private directory", error))?;
                Root::Kept(path.clone())
            }
            None => {
                let dir = tempfile::Builder::new()
                    .prefix("yas-hosted-")
                    .tempdir()
                    .map_err(|error| io_connect("cannot create a private directory", error))?;
                create_private_dir(dir.path())
                    .map_err(|error| io_connect("cannot protect the private directory", error))?;
                Root::Temporary(dir)
            }
        };
        let root_path = root.path().to_path_buf();
        let socket = options
            .socket
            .clone()
            .unwrap_or_else(|| root_path.join("yas.sock"));
        if socket.exists() {
            // A leftover from a server that died: the new one must bind.
            let _ = std::fs::remove_file(&socket);
        }

        let (ours, theirs) = UnixStream::pair()
            .map_err(|error| io_connect("cannot create the fd channel", error))?;
        let channel_fd = theirs.as_raw_fd();

        let mut command = std::process::Command::new(&options.executable);
        command
            .arg("server")
            .arg("--name")
            .arg(&name)
            .arg("--socket")
            .arg(&socket)
            .arg("--fd-channel")
            .arg(channel_fd.to_string())
            .args(&options.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        if options.env_clear {
            command.env_clear();
        } else {
            for key in REDIRECTING_VARIABLES {
                command.env_remove(key);
            }
        }
        if options.isolation == Isolation::Private {
            for (key, dir) in [
                ("XDG_STATE_HOME", "state"),
                ("XDG_CACHE_HOME", "cache"),
                ("XDG_RUNTIME_DIR", "run"),
            ] {
                let path = root_path.join(dir);
                create_private_dir(&path)
                    .map_err(|error| io_connect("cannot create a private directory", error))?;
                command.env(key, path);
            }
        }
        if !options.compositor {
            command.env("YAS_SKIP_COMPOSITOR", "1");
        }
        if !options.relay {
            command.env("YAS_RELAY", "0");
        }
        for (key, value) in &options.env {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        if let Some(dir) = &options.current_dir {
            command.current_dir(dir);
        }
        let log_path = root_path.join("server.log");
        match options.log {
            ServerLog::File => {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .map_err(|error| io_connect("cannot open server.log", error))?;
                command.stderr(file);
            }
            ServerLog::Inherit => {
                command.stderr(Stdio::inherit());
            }
            ServerLog::Null => {
                command.stderr(Stdio::null());
            }
        }
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            // SAFETY: fcntl is async-signal-safe; it only clears
            // close-on-exec on the channel end the child must keep.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(channel_fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let child = command.spawn().map_err(|error| {
            io_connect(
                &format!("cannot start {}", options.executable.display()),
                error,
            )
        })?;
        drop(theirs);

        let mut server = Self {
            channel: Mutex::new(Some(ours)),
            child: Some(child),
            root: Some(root),
            root_path,
            socket,
            name,
            hello: options.hello.clone(),
        };
        match server.wait_ready(options.start_timeout).await {
            Ok(()) => Ok(server),
            Err(error) => {
                let status = server.stop_now();
                let log = if options.log == ServerLog::File {
                    log_tail(&log_path)
                } else {
                    String::new()
                };
                Err(Error::Connect(format!(
                    "hosted YAS server {} did not start: {error}{}{}",
                    server.name,
                    status.map(|s| format!(" ({s})")).unwrap_or_default(),
                    if log.is_empty() {
                        String::new()
                    } else {
                        format!("\n{log}")
                    }
                )))
            }
        }
    }

    async fn wait_ready(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let probe = if SESSIONS_OVER_SOCKET {
            let stream = self.wait_listening(deadline).await?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            tokio::time::timeout(remaining, self.native_on(stream)).await
        } else {
            tokio::time::timeout(timeout, self.connect_native()).await
        };
        match probe {
            Ok(Ok(_client)) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(Error::Timeout("no HELLO answer".into())),
        }
        while !self.socket.exists() {
            if Instant::now() >= deadline {
                return Err(Error::Timeout(format!(
                    "socket {} did not appear",
                    self.socket.display()
                )));
            }
            if let Some(status) = self.try_wait() {
                return Err(Error::Disconnected(format!("server exited ({status})")));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }

    /// A connection to the socket, once the server listens on it.
    async fn wait_listening(&mut self, deadline: Instant) -> Result<UnixStream> {
        loop {
            match UnixStream::connect(&self.socket) {
                Ok(stream) => return Ok(stream),
                // Not bound yet, or bound and not listening yet.
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) => {}
                Err(error) => {
                    return Err(io_connect(
                        &format!("cannot connect to {}", self.socket.display()),
                        error,
                    ));
                }
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(format!(
                    "socket {} did not appear",
                    self.socket.display()
                )));
            }
            if let Some(status) = self.try_wait() {
                return Err(Error::Disconnected(format!("server exited ({status})")));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A new [`Client`] session with the configured HELLO.
    pub async fn connect(&self) -> Result<Client> {
        self.connect_with(&self.hello).await
    }

    /// A new [`Client`] session with `hello` (for example
    /// [`HelloOptions::read_only`]).
    pub async fn connect_with(&self, hello: &HelloOptions) -> Result<Client> {
        let stream = self.stream()?;
        let transport = Transport::from_std_unix(stream)
            .map_err(|error| Error::Connect(format!("cannot register the stream: {error}")))?;
        Client::from_transport(transport, hello).await
    }

    /// A new sequential [`NativeClient`] session.
    pub async fn connect_native(&self) -> Result<NativeClient> {
        self.native_on(self.stream()?).await
    }

    async fn native_on(&self, stream: UnixStream) -> Result<NativeClient> {
        let transport = Transport::from_std_unix(stream)
            .map_err(|error| Error::Connect(format!("cannot register the stream: {error}")))?;
        NativeClient::connect_transport(transport, &self.hello).await
    }

    /// A raw byte stream the server treats as a new client connection: speak
    /// native YAS on it (starting with the preface and HELLO), or splice it
    /// to something that does (`yas connect --stdio` on the other side of a
    /// pipe, a relay).
    ///
    /// On macOS it is a connection to [`HostedServer::socket_path`] (see
    /// [macOS](self#macos)); elsewhere, one end of a socketpair whose other
    /// end went to the server over the channel.
    pub fn stream(&self) -> Result<UnixStream> {
        let channel = self.channel.lock().unwrap_or_else(|p| p.into_inner());
        let Some(channel) = channel.as_ref() else {
            return Err(Error::Closed);
        };
        if SESSIONS_OVER_SOCKET {
            return UnixStream::connect(&self.socket).map_err(|error| {
                Error::Disconnected(format!(
                    "cannot connect to the hosted YAS server socket {}: {error}",
                    self.socket.display()
                ))
            });
        }
        let (ours, theirs) = UnixStream::pair()
            .map_err(|error| Error::Connect(format!("cannot create a socketpair: {error}")))?;
        send_fd(channel.as_raw_fd(), theirs.as_raw_fd()).map_err(|error| {
            Error::Disconnected(format!("hosted YAS server channel failed: {error}"))
        })?;
        Ok(ours)
    }

    /// The server's Unix socket (for `YAS_SOCK`); private to this user.
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// The private directory.
    pub fn root(&self) -> &Path {
        &self.root_path
    }

    /// The server's log, when [`ServerLog::File`].
    pub fn log_path(&self) -> PathBuf {
        self.root_path.join("server.log")
    }

    /// The server name (`--name`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The server's process ID, while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(std::process::Child::id)
    }

    /// The exit status, if the server has exited.
    pub fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut()?.try_wait().ok().flatten()
    }

    /// Close the channel, wait for the server to exit (killing it after
    /// [`DEFAULT_SHUTDOWN_GRACE`]), and remove a temporary private directory.
    pub async fn shutdown(self) -> Result<std::process::ExitStatus> {
        self.shutdown_within(DEFAULT_SHUTDOWN_GRACE).await
    }

    /// [`HostedServer::shutdown`] with an explicit grace period.
    pub async fn shutdown_within(mut self, grace: Duration) -> Result<std::process::ExitStatus> {
        self.close_channel();
        let deadline = Instant::now() + grace;
        loop {
            if let Some(status) = self.try_wait() {
                self.child = None;
                return Ok(status);
            }
            if Instant::now() >= deadline {
                let status = self.stop_now();
                return status.ok_or_else(|| Error::Timeout("hosted server did not exit".into()));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn close_channel(&self) {
        let mut channel = self.channel.lock().unwrap_or_else(|p| p.into_inner());
        channel.take();
    }

    /// Kill the server's process group now and reap it.
    fn stop_now(&mut self) -> Option<std::process::ExitStatus> {
        self.close_channel();
        let mut child = self.child.take()?;
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if let Ok(pid) = libc::pid_t::try_from(child.id()) {
            // SAFETY: plain syscall on the group this child leads.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        let _ = child.kill();
        child.wait().ok()
    }
}

impl Drop for HostedServer {
    fn drop(&mut self) {
        self.close_channel();
        let Some(mut child) = self.child.take() else {
            return;
        };
        let root = self.root.take();
        // The server shuts down on channel EOF; reap it off-thread so drop
        // does not block, and kill it if it overstays the grace period.
        std::thread::spawn(move || {
            let deadline = Instant::now() + DEFAULT_SHUTDOWN_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if Instant::now() >= deadline => {
                        if let Ok(pid) = libc::pid_t::try_from(child.id()) {
                            // SAFETY: plain syscall on the group this child leads.
                            unsafe {
                                libc::kill(-pid, libc::SIGKILL);
                            }
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
            drop(root);
        });
    }
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Whether sessions connect to the private socket instead of being passed
/// over the channel (see [macOS](self#macos)).
const SESSIONS_OVER_SOCKET: bool = cfg!(target_vendor = "apple");

fn io_connect(what: &str, error: std::io::Error) -> Error {
    Error::Connect(format!("{what}: {error}"))
}

fn log_tail(path: &Path) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(4096);
    String::from_utf8_lossy(&bytes[start..]).trim().to_string()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SEND_FLAGS: libc::c_int = 0;

/// Send `fd` over the Unix stream `channel` with a one-byte message, the
/// framing `yas server --fd-channel` expects.
fn send_fd(channel: RawFd, fd: RawFd) -> std::io::Result<()> {
    let byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: msghdr is plain data; every pointer below outlives sendmsg.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = space as _;
    // SAFETY: the control buffer is CMSG_SPACE(sizeof fd) bytes, enough for
    // exactly one header carrying one descriptor.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
    }
    loop {
        // SAFETY: message points at live buffers for the duration of the call.
        let sent = unsafe { libc::sendmsg(channel, &message, SEND_FLAGS) };
        if sent == 1 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
