//! Terminals: PTYs on the server that outlive the session that started them.
//!
//! [`Client::start_terminal`] starts one (a program, a shell command line, or
//! the server's default shell) and answers its ID. With an ID a client can
//! type into it ([`Client::write_terminal`]), read what it shows
//! ([`Client::terminal_screen`]), resize, signal, restart and close it, and
//! wait for it to exit. When the shell reports its commands (OSC 133, see
//! docs/shell-integration.md), [`Client::wait_terminal_command`] waits for a
//! command to finish and [`Client::terminal_output`] reads what it printed.
//!
//! The catalogue ([`Client::terminals`]) is every terminal on the server, not
//! only this session's: terminals belong to the server, and IDs are shared by
//! every client (`yas terminal list` shows the same ones).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::time::Duration;

use yas_wire::{
    Class, Decode, Encode, Extension, Extensions,
    core::ResultPrefix,
    family,
    schema::terminal as schema,
    state::{Phase, Record, RecordKind, Watch as StateWatch},
    terminal::{self as wire, request_kind},
};

pub use yas_wire::terminal::{ExitReason, ExitRecord, JournalRecord, SignalKind};

use crate::client::{Client, DEFAULT_REQUEST_TIMEOUT, Hook, Route};
use crate::error::{Error, Result};
use crate::state::{STATE_CREDIT, Subscription};
use crate::transfer::ByteStream;

/// Receive credit offered for a query's Transfer, and its window.
const QUERY_CREDIT: u64 = 1024 * 1024;
/// The most a query answer may hold.
pub const MAX_QUERY_BYTES: u64 = 64 * 1024 * 1024;
/// The most one screen read asks for.
const READ_PAGE_BYTES: u32 = 8 * 1024 * 1024;

/// What a new terminal runs, where, and how big it is.
#[derive(Clone, Debug)]
pub struct TerminalCommand {
    command: wire::Command,
    cwd: Option<Vec<u8>>,
    env: BTreeMap<Vec<u8>, Vec<u8>>,
    rows: u16,
    cols: u16,
    tag: Option<String>,
    deadline: Option<Duration>,
}

impl TerminalCommand {
    fn with(command: wire::Command) -> Self {
        Self {
            command,
            cwd: None,
            env: BTreeMap::new(),
            rows: 24,
            cols: 80,
            tag: None,
            deadline: None,
        }
    }

    /// The server's default shell, interactive.
    pub fn shell() -> Self {
        Self::with(wire::Command::DefaultShell)
    }

    /// `program` with no shell in between; add arguments with
    /// [`arg`](Self::arg).
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self::with(wire::Command::Argv(vec![os_bytes(program.as_ref())]))
    }

    /// A command line for the server's shell to run (`$SHELL -c LINE`).
    pub fn shell_command(line: impl Into<String>) -> Self {
        Self::with(wire::Command::ShellCommand(line.into()))
    }

    /// Append an argument (only after [`new`](Self::new)).
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        if let wire::Command::Argv(argv) = &mut self.command {
            argv.push(os_bytes(arg.as_ref()));
        }
        self
    }

    /// Append arguments (only after [`new`](Self::new)).
    pub fn args<A: AsRef<OsStr>>(mut self, args: impl IntoIterator<Item = A>) -> Self {
        for arg in args {
            self = self.arg(arg);
        }
        self
    }

    /// Start in this directory on the server (else the server's default).
    pub fn current_dir(mut self, dir: impl AsRef<OsStr>) -> Self {
        self.cwd = Some(os_bytes(dir.as_ref()));
        self
    }

    /// Set a variable on top of the server's environment; the last value set
    /// for a name wins.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .insert(os_bytes(key.as_ref()), os_bytes(value.as_ref()));
        self
    }

    /// The terminal's size in character cells (24 × 80 by default).
    pub fn size(mut self, rows: u16, cols: u16) -> Self {
        self.rows = rows;
        self.cols = cols;
        self
    }

    /// A label shown in the catalogue (`TAG` in `yas terminal list`).
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Have the server end the terminal's program after this long.
    pub fn deadline(mut self, after: Duration) -> Self {
        self.deadline = Some(after);
        self
    }

    fn to_create(&self) -> Result<wire::Create> {
        if self.rows == 0 || self.cols == 0 {
            return Err(Error::Invalid(
                "a terminal needs at least one row and column".into(),
            ));
        }
        let mut launch_extensions = Vec::new();
        if let Some(after) = self.deadline {
            let nanos = u64::try_from(after.as_nanos()).unwrap_or(u64::MAX);
            if nanos == 0 {
                return Err(Error::Invalid(
                    "a terminal deadline must be positive".into(),
                ));
            }
            launch_extensions.push(Extension {
                tag: schema::LAUNCH_DEADLINE_AFTER_NS_EXTENSION as u16,
                required: false,
                value: nanos.to_le_bytes().to_vec(),
            });
        }
        let mut create_extensions = Vec::new();
        if let Some(tag) = &self.tag {
            create_extensions.push(Extension {
                tag: schema::CREATE_RESOURCE_TAG_EXTENSION as u16,
                required: false,
                value: tag.as_bytes().to_vec(),
            });
        }
        Ok(wire::Create {
            rows: self.rows,
            cols: self.cols,
            operation_id: operation_id(),
            launch: wire::Launch {
                command: self.command.clone(),
                cwd: self
                    .cwd
                    .clone()
                    .map_or(wire::Cwd::ServerDefault, wire::Cwd::Path),
                environment_base: wire::EnvironmentBase::Server,
                environment: self
                    .env
                    .iter()
                    .map(|(key, value)| wire::EnvironmentEntry {
                        key: key.clone(),
                        value: wire::EnvironmentValue::Set(value.clone()),
                    })
                    .collect(),
                extensions: Extensions(launch_extensions),
            },
            extensions: Extensions(create_extensions),
        })
    }
}

/// A terminal in the server's catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalInfo {
    /// Its ID, the same for every client of the server.
    pub id: u64,
    /// Bumped when the terminal restarts.
    pub generation: u32,
    pub rows: u16,
    pub cols: u16,
    pub tag: Option<String>,
    /// The title its program set.
    pub title: Option<String>,
    /// What it runs, for display.
    pub command: Option<String>,
    /// Its working directory, as the shell last reported it.
    pub cwd: Option<Vec<u8>>,
    pub status: TerminalStatus,
}

/// Whether a terminal's program still runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalStatus {
    Running,
    /// It ended; the server says how when it knows.
    Exited(Option<ExitRecord>),
}

impl TerminalInfo {
    fn from_record(record: &wire::TerminalRecord) -> Result<Self> {
        let text = |tag: u64| {
            extension(&record.extensions, tag).map(|value| String::from_utf8_lossy(value).into())
        };
        let status = match record.lifecycle {
            wire::Lifecycle::Running => TerminalStatus::Running,
            wire::Lifecycle::Exited => TerminalStatus::Exited(
                extension(&record.extensions, schema::STATE_EXIT_EXTENSION)
                    .map(ExitRecord::decode)
                    .transpose()?,
            ),
        };
        Ok(Self {
            id: record.terminal_handle,
            generation: record.generation,
            rows: record.rows,
            cols: record.cols,
            tag: text(schema::STATE_RESOURCE_TAG_EXTENSION),
            title: text(schema::STATE_TITLE_EXTENSION),
            command: text(schema::STATE_COMMAND_DISPLAY_EXTENSION),
            cwd: extension(&record.extensions, schema::STATE_CWD_EXTENSION).map(<[u8]>::to_vec),
            status,
        })
    }

    /// Whether its program still runs.
    pub fn is_running(&self) -> bool {
        self.status == TerminalStatus::Running
    }
}

/// What a command printed ([`Client::terminal_output`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalOutput {
    /// Its output as the terminal rendered it: text, one line per row.
    pub text: Vec<u8>,
    /// More followed than `max_bytes` allowed.
    pub truncated: bool,
    /// Its start had already scrolled out of the server's backlog.
    pub evicted: bool,
}

struct Query {
    content_kind: u8,
    bytes: Vec<u8>,
}

impl Client {
    /// Every terminal on the server.
    pub async fn terminals(&self) -> Result<Vec<TerminalInfo>> {
        let mut subscription = self.watch_terminal_catalogue().await?;
        let mut catalogue = BTreeMap::new();
        for record in &subscription.snapshot().await? {
            fold(&mut catalogue, record)?;
        }
        catalogue.values().map(TerminalInfo::from_record).collect()
    }

    /// The terminal with this ID (a `NotFound` status error when there is
    /// none).
    pub async fn terminal(&self, id: u64) -> Result<TerminalInfo> {
        self.terminals()
            .await?
            .into_iter()
            .find(|terminal| terminal.id == id)
            .ok_or_else(|| not_found(id))
    }

    /// Start a terminal; answers its ID.
    pub async fn start_terminal(&self, command: &TerminalCommand) -> Result<u64> {
        let result: wire::CreateResult = self
            .request(
                family::TERMINAL,
                request_kind::CREATE,
                &command.to_create()?,
            )
            .await?;
        Ok(result.terminal_handle)
    }

    /// Type `bytes` into the terminal, as keystrokes (`"ls\r"` runs `ls`).
    pub async fn write_terminal(&self, id: u64, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let kind = wire::event_kind::WRITE;
        if !self.supports(family::TERMINAL, Class::Event, kind) {
            return Err(Error::Unsupported(
                "this YAS session cannot type into terminals (read-only, or no Terminal family)"
                    .into(),
            ));
        }
        self.send_event(
            family::TERMINAL,
            kind,
            &wire::Write {
                terminal_handle: id,
                data: bytes.to_vec(),
            },
            crate::client::default_sensitive(family::TERMINAL, Class::Event, kind),
        )
    }

    /// Resize the terminal (its program sees `SIGWINCH`).
    pub async fn resize_terminal(&self, id: u64, rows: u16, cols: u16) -> Result<()> {
        let _: wire::ResizeResult = self
            .request(
                family::TERMINAL,
                request_kind::RESIZE,
                &wire::Resize {
                    terminal_handle: id,
                    rows,
                    cols,
                },
            )
            .await?;
        Ok(())
    }

    /// What the terminal shows now: its visible rows as plain text.
    pub async fn terminal_screen(&self, id: u64) -> Result<String> {
        let terminal = self.terminal(id).await?;
        let query = self
            .terminal_query(
                request_kind::READ,
                &wire::Read {
                    terminal_handle: id,
                    generation: terminal.generation,
                    cursor_kind: schema::READ_CURSOR_TAIL as u8,
                    representation: schema::QUERY_REPRESENTATION_PLAIN as u8,
                    flags: schema::READ_FLAGS as u16,
                    cursor_a: 0,
                    cursor_b: u32::from(terminal.rows),
                    max_bytes: READ_PAGE_BYTES,
                    initial_receive_credit: QUERY_CREDIT,
                    extensions: Extensions::default(),
                },
                u64::from(READ_PAGE_BYTES),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?;
        expect_content(&query, schema::CONTENT_TEXT)?;
        String::from_utf8(query.bytes)
            .map_err(|_| Error::protocol("YAS Terminal READ returned text that is not UTF-8"))
    }

    /// The terminal's working directory, as its shell reports it.
    pub async fn terminal_cwd(&self, id: u64) -> Result<Vec<u8>> {
        let terminal = self.terminal(id).await?;
        let query = self
            .terminal_query(
                request_kind::CWD,
                &wire::CwdQuery {
                    terminal_handle: id,
                    generation: terminal.generation,
                    initial_receive_credit: QUERY_CREDIT,
                    extensions: Extensions::default(),
                },
                MAX_QUERY_BYTES,
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?;
        expect_content(&query, schema::CONTENT_PATH)?;
        Ok(query.bytes)
    }

    /// Wait up to `timeout` for a command the shell reported to finish: the
    /// one with this journal `index`, or the one running now (else the next
    /// one to start). Answers its journal record (exit code, times, command
    /// line). When it still runs at the timeout: a `Timeout` error; when
    /// none started by then (or it left the backlog): a `NotFound` status.
    /// Needs shell integration.
    pub async fn wait_terminal_command(
        &self,
        id: u64,
        index: Option<u64>,
        timeout: Duration,
    ) -> Result<JournalRecord> {
        let terminal = self.terminal(id).await?;
        let timeout_ns = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX).max(1);
        let query = self
            .terminal_query(
                request_kind::WAIT,
                &wire::Wait {
                    terminal_handle: id,
                    generation: terminal.generation,
                    wait_kind: if index.is_some() {
                        schema::WAIT_COMMAND as u8
                    } else {
                        schema::WAIT_LATEST_COMMAND as u8
                    },
                    flags: schema::WAIT_FLAGS as u8,
                    cursor_a: index.unwrap_or(0),
                    cursor_b: 0,
                    max_bytes: READ_PAGE_BYTES,
                    timeout_ns,
                    needle: Vec::new(),
                    initial_receive_credit: QUERY_CREDIT,
                    extensions: Extensions::default(),
                },
                MAX_QUERY_BYTES,
                timeout.saturating_add(Duration::from_secs(2)),
            )
            .await?;
        expect_content(&query, schema::CONTENT_JOURNAL)?;
        let record = wire::JournalResult::decode(&query.bytes)?
            .records
            .into_iter()
            .next()
            .ok_or_else(|| not_found(id))?;
        if record.flags & schema::JOURNAL_RUNNING as u16 != 0 {
            return Err(Error::Timeout(format!(
                "terminal {id}: command {} still runs after {timeout:?}",
                record.index
            )));
        }
        Ok(record)
    }

    /// The commands the shell reported, oldest first, at most `limit` of the
    /// latest ones.
    pub async fn terminal_commands(&self, id: u64, limit: u16) -> Result<Vec<JournalRecord>> {
        let terminal = self.terminal(id).await?;
        let query = self
            .terminal_query(
                request_kind::JOURNAL,
                &wire::Journal {
                    terminal_handle: id,
                    generation: terminal.generation,
                    flags: schema::JOURNAL_TAIL as u16,
                    limit,
                    from_index: 0,
                    initial_receive_credit: QUERY_CREDIT,
                    extensions: Extensions::default(),
                },
                MAX_QUERY_BYTES,
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?;
        expect_content(&query, schema::CONTENT_JOURNAL)?;
        Ok(wire::JournalResult::decode(&query.bytes)?.records)
    }

    /// What a command printed: the one with this journal `index`, or the
    /// latest one, at most `max_bytes` of it. Needs shell integration.
    pub async fn terminal_output(
        &self,
        id: u64,
        index: Option<u64>,
        max_bytes: u32,
    ) -> Result<TerminalOutput> {
        let terminal = self.terminal(id).await?;
        let query = self
            .terminal_query(
                request_kind::OUTPUT,
                &wire::Output {
                    terminal_handle: id,
                    generation: terminal.generation,
                    cursor_kind: if index.is_some() {
                        schema::OUTPUT_CURSOR_COMMAND as u8
                    } else {
                        schema::OUTPUT_CURSOR_LATEST_COMMAND as u8
                    },
                    flags: schema::OUTPUT_REQUEST_FLAGS as u8,
                    cursor_a: index.unwrap_or(0),
                    cursor_b: 0,
                    max_bytes,
                    initial_receive_credit: QUERY_CREDIT,
                    extensions: Extensions::default(),
                },
                u64::from(max_bytes).clamp(1, MAX_QUERY_BYTES),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?;
        expect_content(&query, schema::CONTENT_OUTPUT)?;
        let output = wire::OutputResult::decode(&query.bytes)?;
        Ok(TerminalOutput {
            text: output.text,
            truncated: output.flags & schema::OUTPUT_TRUNCATED as u16 != 0,
            evicted: output.flags & schema::OUTPUT_EVICTED as u16 != 0,
        })
    }

    /// Send the terminal's program a signal.
    pub async fn signal_terminal(&self, id: u64, signal: SignalKind) -> Result<()> {
        self.terminal_empty(
            request_kind::SIGNAL,
            &wire::Signal {
                terminal_handle: id,
                operation_id: operation_id(),
                signal,
                extensions: Extensions::default(),
            },
        )
        .await
    }

    /// Start the terminal's program again (its generation goes up).
    pub async fn restart_terminal(&self, id: u64) -> Result<()> {
        let _: wire::RestartResult = self
            .request(
                family::TERMINAL,
                request_kind::RESTART,
                &wire::Restart {
                    terminal_handle: id,
                    operation_id: operation_id(),
                    launch_mode: wire::LaunchMode::Replay,
                    cutover_mode: wire::CutoverMode::StopThenStart,
                    launch: None,
                    extensions: Extensions::default(),
                },
            )
            .await?;
        Ok(())
    }

    /// Close the terminal: its program is hung up and it leaves the
    /// catalogue.
    pub async fn close_terminal(&self, id: u64) -> Result<()> {
        self.terminal_empty(
            request_kind::CLOSE,
            &wire::Close {
                terminal_handle: id,
                operation_id: operation_id(),
            },
        )
        .await
    }

    /// Wait until the terminal's program ends (or the terminal is closed) and
    /// answer the terminal as it was then. No timeout: wrap it in one.
    pub async fn wait_terminal_exit(&self, id: u64) -> Result<TerminalInfo> {
        let mut subscription = self.watch_terminal_catalogue().await?;
        let mut catalogue = BTreeMap::new();
        let mut last: Option<TerminalInfo> = None;
        loop {
            let event = subscription.next().await?;
            if matches!(event.phase, Phase::SnapshotBegin | Phase::Reset) {
                catalogue.clear();
            }
            for record in &event.records {
                fold(&mut catalogue, record)?;
            }
            // Mid-snapshot, a missing terminal may simply not have come yet.
            if !matches!(event.phase, Phase::SnapshotEnd | Phase::Delta) {
                continue;
            }
            match catalogue.get(&id) {
                Some(record) => {
                    let terminal = TerminalInfo::from_record(record)?;
                    if !terminal.is_running() {
                        return Ok(terminal);
                    }
                    last = Some(terminal);
                }
                // Closed while running: it left without an exit record.
                None => match last.take() {
                    Some(mut terminal) => {
                        terminal.status = TerminalStatus::Exited(None);
                        return Ok(terminal);
                    }
                    None => return Err(not_found(id)),
                },
            }
        }
    }

    async fn watch_terminal_catalogue(&self) -> Result<Subscription> {
        Subscription::open(
            self,
            family::TERMINAL,
            request_kind::WATCH,
            request_kind::UNWATCH,
            StateWatch {
                initial_credit: STATE_CREDIT,
                resume: None,
                extensions: Extensions::default(),
            }
            .encode()?,
        )
        .await
    }

    async fn terminal_empty<Q: Encode>(&self, kind: u16, request: &Q) -> Result<()> {
        let reply = self
            .call_ok(
                family::TERMINAL,
                kind,
                request.encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                None,
            )
            .await?;
        if reply.prefix.body.is_empty() {
            Ok(())
        } else {
            Err(Error::protocol(format!(
                "YAS Terminal Result {kind:#06x} had an unexpected body"
            )))
        }
    }

    /// A Terminal query (READ, CWD, JOURNAL, OUTPUT, WAIT): its answer comes
    /// inline or over a byte Transfer.
    async fn terminal_query<Q: Encode>(
        &self,
        kind: u16,
        request: &Q,
        maximum: u64,
        timeout: Duration,
    ) -> Result<Query> {
        let hook: Hook =
            Box::new(
                |prefix: &ResultPrefix| match wire::QueryBody::decode(&prefix.body) {
                    Ok(wire::QueryBody {
                        delivery: wire::QueryDelivery::Transfer(descriptor),
                        ..
                    }) => vec![Route::Transfer(descriptor.transfer_id)],
                    _ => Vec::new(),
                },
            );
        let mut reply = self
            .call_ok(
                family::TERMINAL,
                kind,
                request.encode()?,
                Some(timeout),
                Some(hook),
            )
            .await?;
        let body = wire::QueryBody::decode(&reply.prefix.body)?;
        body.validate_receive_credit(QUERY_CREDIT)?;
        let bytes = match body.delivery {
            wire::QueryDelivery::Inline(bytes) => bytes,
            wire::QueryDelivery::Transfer(descriptor) => {
                let frames = reply
                    .take(Route::Transfer(descriptor.transfer_id))
                    .ok_or_else(|| Error::protocol("YAS Terminal query route missing"))?;
                ByteStream::new(self.clone(), descriptor, frames, QUERY_CREDIT)?
                    .read_to_end(maximum)
                    .await?
            }
        };
        if bytes.len() as u64 > maximum {
            return Err(Error::protocol(format!(
                "YAS Terminal query answered {} bytes, more than the {maximum} asked for",
                bytes.len()
            )));
        }
        Ok(Query {
            content_kind: body.content_kind,
            bytes,
        })
    }
}

fn expect_content(query: &Query, expected: u64) -> Result<()> {
    if u64::from(query.content_kind) == expected {
        Ok(())
    } else {
        Err(Error::protocol(format!(
            "YAS Terminal query answered content kind {} instead of {expected}",
            query.content_kind
        )))
    }
}

fn not_found(id: u64) -> Error {
    Error::status_from(
        format!("terminal {id}"),
        yas_wire::core::Status::NotFound,
        Extensions::default(),
    )
}

/// Fold one catalogue state record into `catalogue`, by terminal ID.
fn fold(catalogue: &mut BTreeMap<u64, wire::TerminalRecord>, record: &Record) -> Result<()> {
    match record.kind {
        RecordKind::Add | RecordKind::Replace => {
            let terminal = wire::terminal_from_state_record(record)?;
            catalogue.insert(terminal.terminal_handle, terminal);
        }
        RecordKind::Patch => {
            let patch = wire::patch_from_state_record(record)?;
            if let Some(terminal) = catalogue.get_mut(&patch.terminal_handle) {
                for extension in patch.extensions.0 {
                    terminal
                        .extensions
                        .0
                        .retain(|existing| existing.tag != extension.tag);
                    terminal.extensions.0.push(extension);
                }
            }
        }
        RecordKind::Remove => {
            catalogue.remove(&wire::removal_from_state_record(record)?.terminal_handle);
        }
        RecordKind::Family(_) => {}
    }
    Ok(())
}

fn extension(extensions: &Extensions, tag: u64) -> Option<&[u8]> {
    extensions
        .0
        .iter()
        .find(|extension| u64::from(extension.tag) == tag)
        .map(|extension| extension.value.as_slice())
}

fn operation_id() -> [u8; 16] {
    let mut value: [u8; 16] = rand::random();
    if value == [0; 16] {
        value[15] = 1;
    }
    value
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_keeps_argv_and_sorts_the_environment() {
        let create = TerminalCommand::new("printf")
            .args(["%s", "hello world"])
            .current_dir("/src")
            .env("Z", "last")
            .env("A", "first")
            .env("Z", "later")
            .size(30, 100)
            .tag("build")
            .deadline(Duration::from_secs(3))
            .to_create()
            .unwrap();
        assert_eq!(
            create.launch.command,
            wire::Command::Argv(vec![
                b"printf".to_vec(),
                b"%s".to_vec(),
                b"hello world".to_vec()
            ])
        );
        assert_eq!(create.launch.cwd, wire::Cwd::Path(b"/src".to_vec()));
        let keys: Vec<_> = create
            .launch
            .environment
            .iter()
            .map(|entry| &entry.key[..])
            .collect();
        assert_eq!(keys, [&b"A"[..], &b"Z"[..]]);
        assert_eq!(
            create.launch.environment[1].value,
            wire::EnvironmentValue::Set(b"later".to_vec())
        );
        assert_eq!((create.rows, create.cols), (30, 100));
        assert_eq!(
            create.launch.extensions.0[0].value,
            3_000_000_000u64.to_le_bytes()
        );
        assert_eq!(create.extensions.0[0].value, b"build");
        assert!(TerminalCommand::shell().size(0, 80).to_create().is_err());
    }
}
