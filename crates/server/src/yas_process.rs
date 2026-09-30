//! Typed YAS adapter for the existing non-PTY process manager.
//!
//! The process manager remains the single owner of children, admission,
//! catalog generations and stream backpressure. This module translates its
//! private endpoint packets into semantic values; no YAS packet is exposed to
//! a YAS peer. Wire request IDs, operation replay and Transfer descriptors stay
//! in `yas`.

use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

#[cfg(test)]
use tokio::sync::{Notify, Semaphore};
use tokio::sync::{mpsc, watch};
use yas_wire::core::RuntimeState;
use yas_wire::process as wire;
use yas_wire::schema;

use super::app_env::SessionEnv;
use super::process::{self, NativeRecord, Server};

const ROUTE_EVENTS: usize = 80;

#[derive(Clone)]
pub(crate) struct Runtime {
    server: Server,
    #[cfg(test)]
    operation_gate: Option<Arc<TestOperationGate>>,
}

#[cfg(test)]
pub(crate) struct TestOperationGate {
    entered: AtomicUsize,
    releases: Semaphore,
    changed: Notify,
}

#[cfg(test)]
impl Default for TestOperationGate {
    fn default() -> Self {
        Self {
            entered: AtomicUsize::new(0),
            releases: Semaphore::new(0),
            changed: Notify::new(),
        }
    }
}

#[cfg(test)]
impl TestOperationGate {
    async fn enter(&self) {
        self.entered.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_waiters();
        self.releases
            .acquire()
            .await
            .expect("Process test operation gate remains open")
            .forget();
    }

    #[cfg(unix)]
    pub(crate) async fn wait_for_entered(&self, expected: usize) {
        loop {
            let changed = self.changed.notified();
            if self.entered.load(Ordering::Acquire) >= expected {
                return;
            }
            changed.await;
        }
    }

    #[cfg(unix)]
    pub(crate) fn release(&self, count: usize) {
        self.releases.add_permits(count);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub(crate) revision: u64,
    pub(crate) records: Vec<wire::ProcessRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Unavailable,
    NotFound,
    Conflict,
    Permission,
    ResourceExhausted,
    Invalid(String),
    Io(String),
    Closed(String),
    Timeout,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("Process family is unavailable"),
            Self::NotFound => formatter.write_str("process not found"),
            Self::Conflict => formatter.write_str("process state conflicts with the request"),
            Self::Permission => formatter.write_str("process operation is not permitted"),
            Self::ResourceExhausted => formatter.write_str("process resource limit reached"),
            Self::Invalid(detail) => formatter.write_str(detail),
            Self::Io(detail) | Self::Closed(detail) => formatter.write_str(detail),
            Self::Timeout => formatter.write_str("process wait timed out"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Output {
        stream: Stream,
        lifetime_offset: u64,
        data: Vec<u8>,
    },
    StdinProgress {
        consumed: u64,
        open: bool,
    },
    Exit(ExitInfo),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExitInfo {
    pub(crate) kind: wire::ExitKind,
    pub(crate) reason: u8,
    pub(crate) code: i32,
    pub(crate) detail: Vec<u8>,
}

impl ExitInfo {
    pub(crate) fn into_record(self, exited_server_ns: u64) -> wire::ExitRecord {
        wire::ExitRecord {
            kind: self.kind,
            reason: self.reason,
            code: self.code,
            exited_server_ns: exited_server_ns.max(1),
            detail: self.detail,
        }
    }
}

pub(crate) struct Attachment {
    session: Session,
    route: Arc<Route>,
    events: mpsc::Receiver<Event>,
    pub(crate) process_handle: u64,
    pub(crate) stdin_lifetime_offset: u64,
    pub(crate) stdout_lifetime_offset: u64,
    pub(crate) stderr_lifetime_offset: u64,
    pub(crate) stdin_window: u64,
    pub(crate) merged_stderr: bool,
}

/// Cloneable command half of one Process attachment.
///
/// The native YAS boundary owns the event receiver in a dedicated pump while
/// Transfer input, output acknowledgement, and half-close handling continue
/// independently on the session task. Keeping those halves separate avoids
/// holding a lock across `AttachmentEvents::next` and therefore avoids
/// blocking stdin behind an uncredited stdout stream.
#[derive(Clone)]
pub(crate) struct AttachmentControl {
    session: Session,
    route: Arc<Route>,
    pub(crate) process_handle: u64,
    pub(crate) stdin_lifetime_offset: u64,
    pub(crate) stdout_lifetime_offset: u64,
    pub(crate) stderr_lifetime_offset: u64,
    pub(crate) stdin_window: u64,
    pub(crate) merged_stderr: bool,
}

pub(crate) struct AttachmentEvents {
    events: mpsc::Receiver<Event>,
    failed: watch::Receiver<Option<String>>,
}

#[derive(Clone)]
pub(crate) struct Session {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    server: Server,
    manager: process::Manager,
    session_env: StdMutex<Option<SessionEnv>>,
    next_process_id: AtomicU32,
    routes: StdMutex<HashMap<u32, Arc<Route>>>,
    /// Ordinary children leave the global catalogue as soon as EXIT is
    /// delivered. Retain their terminal value for the owning YAS session so a
    /// later WAIT remains authoritative.
    exits: StdMutex<ExitReplays>,
    closed: watch::Sender<Option<Error>>,
    shutting_down: AtomicBool,
    #[cfg(test)]
    operation_gate: Option<Arc<TestOperationGate>>,
}

struct ExitReplays {
    values: HashMap<u64, ExitInfo>,
    order: VecDeque<u64>,
    capacity: usize,
}

impl ExitReplays {
    fn new(capacity: usize) -> Self {
        Self {
            values: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    fn get(&self, process_handle: u64) -> Option<&ExitInfo> {
        self.values.get(&process_handle)
    }

    fn insert(&mut self, process_handle: u64, exit: ExitInfo) {
        if self.values.insert(process_handle, exit).is_none() {
            self.order.push_back(process_handle);
        }
        while self.order.len() > self.capacity {
            if let Some(retired) = self.order.pop_front() {
                self.values.remove(&retired);
            }
        }
    }

    fn clear(&mut self) {
        self.values.clear();
        self.order.clear();
    }
}

struct Route {
    process_id: u32,
    process_handle: AtomicU64,
    auto_ack_output: bool,
    events: mpsc::Sender<Event>,
    exit: watch::Sender<Option<ExitInfo>>,
    /// Why the process's output left this route: its native binding fell a window behind or
    /// its queue filled. Only this attachment fails; the session and its other processes go on.
    failed: watch::Sender<Option<String>>,
}

/// What a route dropped for falling behind tells its attachment and its WAITs.
const ROUTE_EVICTED: &str = "Process output fell a window behind its reader and was dropped";

enum WatchOutcome {
    Running(Attachment),
    Exited(ExitInfo),
}

impl Runtime {
    pub(crate) fn new(server: Server) -> Self {
        Self {
            server,
            #[cfg(test)]
            operation_gate: None,
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_operation_gate(mut self, gate: Arc<TestOperationGate>) -> Self {
        self.operation_gate = Some(gate);
        self
    }

    pub(crate) fn enabled(&self) -> bool {
        self.server.enabled()
    }

    pub(crate) fn runtime_state(&self) -> RuntimeState {
        if self.enabled() {
            RuntimeState::Available
        } else {
            RuntimeState::Unavailable
        }
    }

    pub(crate) fn limits(&self) -> wire::Limits {
        // LEAVE_RESIDUE works with Unix process groups and Windows jobs alike.
        wire::Limits {
            launcher_flags: schema::process::SPAWN_LAUNCHER_FLAGS as u32,
            ..self.server.maxima().limits()
        }
    }

    pub(crate) fn maxima(&self) -> process::ProcessMaxima {
        self.server.maxima()
    }

    pub(crate) fn session(
        &self,
        owner_session: [u8; 16],
        session_env: Option<SessionEnv>,
    ) -> Result<Session, Error> {
        if !self.enabled() {
            return Err(Error::Unavailable);
        }
        if owner_session.iter().all(|byte| *byte == 0) {
            return Err(Error::Invalid("zero Process owner session".to_owned()));
        }
        let (manager, events, evictions) = self
            .server
            .native_endpoint_with_session(owner_session, self.server.maxima().endpoint_events());
        let (closed, _) = watch::channel(None);
        let inner = Arc::new(SessionInner {
            server: self.server.clone(),
            manager,
            session_env: StdMutex::new(session_env),
            next_process_id: AtomicU32::new(1),
            routes: StdMutex::new(HashMap::new()),
            exits: StdMutex::new(ExitReplays::new(self.server.maxima().exit_replays())),
            closed,
            shutting_down: AtomicBool::new(false),
            #[cfg(test)]
            operation_gate: self.operation_gate.clone(),
        });
        tokio::spawn(route_outbound(Arc::downgrade(&inner), events, evictions));
        Ok(Session { inner })
    }

    pub(crate) fn snapshot(&self, now_server_ns: u64) -> Result<Snapshot, Error> {
        if !self.enabled() {
            return Err(Error::Unavailable);
        }
        let snapshot = self.server.native_snapshot();
        let records = snapshot
            .records
            .into_iter()
            .map(|record| process_record(record, now_server_ns))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Snapshot {
            revision: snapshot.revision,
            records,
        })
    }

    pub(crate) async fn changed(
        &self,
        revision: u64,
        now_server_ns: impl FnOnce() -> u64,
    ) -> Result<Snapshot, Error> {
        if !self.enabled() {
            return Err(Error::Unavailable);
        }
        self.server.wait_native_catalogue_change(revision).await;
        self.snapshot(now_server_ns())
    }
}

impl Session {
    /// Install the compositor-scoped environment lazily when the first
    /// `ENV_SESSION` spawn is actually requested. Negotiating or merely
    /// watching Process must not create a compositor as a side effect.
    pub(crate) fn set_session_env(&self, session_env: SessionEnv) {
        let mut current = self.inner.session_env.lock().unwrap();
        if current.is_none() {
            *current = Some(session_env);
        }
    }

    pub(crate) async fn spawn(
        &self,
        request: &wire::Spawn,
        resolved_cwd: Option<Vec<u8>>,
    ) -> Result<Attachment, Error> {
        let cwd = resolve_cwd(&request.cwd, resolved_cwd)?;
        let flags = u8::try_from(request.flags)
            .map_err(|_| Error::Invalid("Process SPAWN flags do not fit v1".to_owned()))?;
        let process_id = self.allocate_process_id()?;
        let (route, events) = self.install_route(process_id, false)?;
        let session_env = (request.environment_kind == wire::EnvironmentKind::Session)
            .then(|| self.inner.session_env.lock().unwrap().clone())
            .flatten();
        let clear_environment = request.environment_kind == wire::EnvironmentKind::Empty;
        let preserve_residual = request
            .surface_app_handle()
            .map_err(|error| Error::Invalid(error.to_string()))?
            .is_some();
        let residue_grace = request
            .residue_grace_ns()
            .map_err(|error| Error::Invalid(error.to_string()))?
            .map(std::time::Duration::from_nanos);
        let started = self
            .inner
            .manager
            .spawn_native(
                process::NativeSpawnRequest {
                    process_id,
                    flags,
                    preserve_residual,
                    residue_grace,
                    cwd,
                    argv: request.argv.clone(),
                    env: request
                        .env
                        .iter()
                        .map(|entry| (entry.key.clone(), entry.value.clone()))
                        .collect(),
                    clear_environment,
                },
                session_env,
            )
            .await
            .map_err(backend_error);
        let started = match started {
            Ok(started) => started,
            Err(error) => {
                self.remove_route(process_id);
                return Err(error);
            }
        };
        if started.process_id != process_id {
            self.remove_route(process_id);
            return Err(Error::Closed("mismatched Process SPAWN reply".to_owned()));
        }
        {
            // Under the routes lock, like the Exit handler: either it sees
            // this handle and records the exit replay, or this sees the
            // exit it already published (a process that exits before SPAWN
            // returns) and records it here. A WAIT never misses both.
            let _routes = self.inner.routes.lock().unwrap();
            route
                .process_handle
                .store(started.process_handle, Ordering::Release);
            if let Some(exit) = route.exit.borrow().clone() {
                self.inner
                    .exits
                    .lock()
                    .unwrap()
                    .insert(started.process_handle, exit);
            }
        }
        Ok(Attachment {
            session: self.clone(),
            route,
            events,
            process_handle: started.process_handle,
            stdin_lifetime_offset: 0,
            stdout_lifetime_offset: 0,
            stderr_lifetime_offset: 0,
            stdin_window: started.stdin_window,
            merged_stderr: started.stderr_window == 0,
        })
    }

    pub(crate) async fn attach(&self, request: &wire::Attach) -> Result<Attachment, Error> {
        #[cfg(test)]
        if let Some(gate) = &self.inner.operation_gate {
            gate.enter().await;
        }
        match self
            .watch_process(
                request.process_handle,
                request.flags & schema::process::ATTACH_STDIN as u16 != 0,
                false,
            )
            .await?
        {
            WatchOutcome::Running(attachment) => Ok(attachment),
            WatchOutcome::Exited(_) => Err(Error::Conflict),
        }
    }

    pub(crate) async fn control(
        &self,
        request: &wire::Control,
    ) -> Result<wire::ControlResult, Error> {
        #[cfg(test)]
        if let Some(gate) = &self.inner.operation_gate {
            gate.enter().await;
        }
        let (process_id, temporary) = match self.route_for_handle(request.process_handle) {
            Some(process_id) => (process_id, None),
            None => match self
                .watch_process(request.process_handle, false, true)
                .await?
            {
                WatchOutcome::Running(attachment) => {
                    let process_id = attachment.route.process_id;
                    (process_id, Some(attachment))
                }
                WatchOutcome::Exited(_) => return Err(Error::Conflict),
            },
        };
        let action = native_control(request.action, request.value)?;
        self.send_control(process_id, action).await?;
        if request.action == wire::ControlAction::Detach {
            self.remove_route(process_id);
        } else if let Some(attachment) = temporary {
            let _ = self
                .send_control(process_id, process::NativeControl::Detach)
                .await;
            self.remove_route(attachment.route.process_id);
        }
        Ok(wire::ControlResult {
            state_revision: self.inner.server.native_snapshot().revision,
        })
    }

    pub(crate) async fn wait(&self, request: &wire::Wait) -> Result<ExitInfo, Error> {
        let deadline = (request.timeout_ns != 0)
            .then(|| tokio::time::Instant::now() + Duration::from_nanos(request.timeout_ns));
        // The route waited on can leave first (its stream was dropped, or it fell behind and
        // was failed): the process goes on, so the wait looks again.
        loop {
            if let Some(exit) = self.wait_route(request, deadline).await? {
                return Ok(exit);
            }
            if let Some(error) = self.inner.closed.borrow().clone() {
                return Err(error);
            }
        }
    }

    /// Waits on the process's route of now; None when the route left before the exit.
    async fn wait_route(
        &self,
        request: &wire::Wait,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<ExitInfo>, Error> {
        if let Some(exit) = self
            .inner
            .exits
            .lock()
            .unwrap()
            .get(request.process_handle)
            .cloned()
        {
            return Ok(Some(exit));
        }
        let (mut exit, mut failed, temporary) =
            if let Some(route) = self.route_by_handle(request.process_handle) {
                (route.exit.subscribe(), route.failed.subscribe(), None)
            } else if let Some(exit) = self
                .inner
                .exits
                .lock()
                .unwrap()
                .get(request.process_handle)
                .cloned()
            {
                // It exited between the first look and the route lookup; the
                // replay is recorded before the route is removed.
                return Ok(Some(exit));
            } else {
                match self
                    .watch_process(request.process_handle, false, true)
                    .await?
                {
                    WatchOutcome::Exited(exit) => return Ok(Some(exit)),
                    WatchOutcome::Running(attachment) => {
                        let exit = attachment.route.exit.subscribe();
                        let failed = attachment.route.failed.subscribe();
                        (exit, failed, Some(attachment))
                    }
                }
            };
        let wait = async {
            loop {
                if let Some(exit) = exit.borrow().clone() {
                    return Some(exit);
                }
                if failed.borrow().is_some() {
                    return None;
                }
                tokio::select! {
                    changed = exit.changed() => if changed.is_err() {
                        // The route is gone: with its exit, or before it.
                        return exit.borrow().clone();
                    },
                    _ = failed.changed() => {}
                }
            }
        };
        let result = match deadline {
            None => Ok(wait.await),
            Some(deadline) => tokio::time::timeout_at(deadline, wait)
                .await
                .map_err(|_| Error::Timeout),
        };
        if let Some(attachment) = temporary {
            let _ = self
                .send_control(attachment.route.process_id, process::NativeControl::Detach)
                .await;
            self.remove_route(attachment.route.process_id);
        }
        result
    }

    pub(crate) async fn shutdown(&self) {
        if self.inner.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        self.inner.manager.shutdown().await;
        close_session(
            &self.inner,
            Error::Closed("Process session closed".to_owned()),
        );
    }

    async fn watch_process(
        &self,
        process_handle: u64,
        stdin: bool,
        auto_ack_output: bool,
    ) -> Result<WatchOutcome, Error> {
        let process_id = self.allocate_process_id()?;
        let (route, events) = self.install_route(process_id, auto_ack_output)?;
        route
            .process_handle
            .store(process_handle, Ordering::Release);
        let watched = self
            .inner
            .manager
            .watch_native(process_id, process_handle, stdin)
            .map_err(backend_error);
        let watched = match watched {
            Ok(watched) => watched,
            Err(error) => {
                self.remove_route(process_id);
                return Err(error);
            }
        };
        if watched.process_id != process_id || watched.process_handle != process_handle {
            self.remove_route(process_id);
            return Err(Error::Closed("mismatched Process WATCH reply".to_owned()));
        }
        if !watched.running {
            self.remove_route(process_id);
            return Ok(WatchOutcome::Exited(
                watched
                    .exit
                    .map(native_exit_info)
                    .ok_or_else(|| Error::Closed("missing Process exit".to_owned()))?,
            ));
        }
        Ok(WatchOutcome::Running(Attachment {
            session: self.clone(),
            route,
            events,
            process_handle,
            stdin_lifetime_offset: watched.stdin_received,
            stdout_lifetime_offset: watched.stdout_next,
            stderr_lifetime_offset: watched.stderr_next,
            stdin_window: watched.stdin_window,
            merged_stderr: watched.stream_state & process::NATIVE_STREAM_MERGED_STDERR != 0,
        }))
    }

    async fn send_control(
        &self,
        process_id: u32,
        action: process::NativeControl,
    ) -> Result<(), Error> {
        self.inner
            .manager
            .control_native(process_id, action)
            .map_err(backend_error)
    }

    fn acknowledge_output(
        &self,
        route: &Route,
        stream: Stream,
        consumed_lifetime_offset: u64,
    ) -> Result<(), Error> {
        let stream = match stream {
            Stream::Stdout => process::NATIVE_STREAM_STDOUT,
            Stream::Stderr => process::NATIVE_STREAM_STDERR,
        };
        match self.inner.manager.acknowledge_output_native(
            route.process_id,
            stream,
            consumed_lifetime_offset,
        ) {
            Ok(()) => Ok(()),
            // Terminal dispatch retires the native binding. Output already
            // delivered ahead of EXIT can still be consumed afterwards, and
            // its final acknowledgement is then an idempotent no-op.
            Err(process::NativeError::NotFound) if route.exit.borrow().is_some() => Ok(()),
            Err(error) => Err(backend_error(error)),
        }
    }

    fn allocate_process_id(&self) -> Result<u32, Error> {
        for _ in 0..u32::MAX {
            let id = self
                .inner
                .next_process_id
                .fetch_add(1, Ordering::Relaxed)
                .max(1);
            if !self.inner.routes.lock().unwrap().contains_key(&id) {
                return Ok(id);
            }
        }
        Err(Error::ResourceExhausted)
    }

    fn install_route(
        &self,
        process_id: u32,
        auto_ack_output: bool,
    ) -> Result<(Arc<Route>, mpsc::Receiver<Event>), Error> {
        let (events, receiver) = mpsc::channel(ROUTE_EVENTS);
        let (exit, _) = watch::channel(None);
        let (failed, _) = watch::channel(None);
        let route = Arc::new(Route {
            process_id,
            process_handle: AtomicU64::new(0),
            auto_ack_output,
            events,
            exit,
            failed,
        });
        if self
            .inner
            .routes
            .lock()
            .unwrap()
            .insert(process_id, route.clone())
            .is_some()
        {
            return Err(Error::Conflict);
        }
        Ok((route, receiver))
    }

    fn remove_route(&self, process_id: u32) {
        self.inner.routes.lock().unwrap().remove(&process_id);
    }

    fn route_for_handle(&self, process_handle: u64) -> Option<u32> {
        self.route_by_handle(process_handle)
            .map(|route| route.process_id)
    }

    fn route_by_handle(&self, process_handle: u64) -> Option<Arc<Route>> {
        self.inner
            .routes
            .lock()
            .unwrap()
            .values()
            .find(|route| route.process_handle.load(Ordering::Acquire) == process_handle)
            .cloned()
    }
}

impl Attachment {
    pub(crate) fn split(self) -> (AttachmentControl, AttachmentEvents) {
        let control = AttachmentControl {
            session: self.session,
            route: self.route,
            process_handle: self.process_handle,
            stdin_lifetime_offset: self.stdin_lifetime_offset,
            stdout_lifetime_offset: self.stdout_lifetime_offset,
            stderr_lifetime_offset: self.stderr_lifetime_offset,
            stdin_window: self.stdin_window,
            merged_stderr: self.merged_stderr,
        };
        let failed = control.route.failed.subscribe();
        (
            control,
            AttachmentEvents {
                events: self.events,
                failed,
            },
        )
    }

    #[cfg(test)]
    pub(crate) async fn next(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    #[cfg(test)]
    pub(crate) fn acknowledge_output(
        &self,
        stream: Stream,
        consumed_lifetime_offset: u64,
    ) -> Result<(), Error> {
        self.session
            .acknowledge_output(&self.route, stream, consumed_lifetime_offset)
    }
}

impl AttachmentEvents {
    /// The next event; None once the route is closed or failed ([`AttachmentEvents::failure`]).
    pub(crate) async fn next(&mut self) -> Option<Event> {
        if self.failed.borrow().is_some() {
            return None;
        }
        tokio::select! {
            biased;
            _ = self.failed.changed() => None,
            event = self.events.recv() => event,
        }
    }

    /// Why the route failed, when it did.
    pub(crate) fn failure(&self) -> Option<String> {
        self.failed.borrow().clone()
    }
}

impl AttachmentControl {
    pub(crate) fn local_id(&self) -> u32 {
        self.route.process_id
    }

    pub(crate) fn write_stdin(&self, lifetime_offset: u64, data: &[u8]) -> Result<(), Error> {
        self.session
            .inner
            .manager
            .write_stdin_native(self.route.process_id, lifetime_offset, data)
            .map_err(backend_error)
    }

    pub(crate) fn acknowledge_output(
        &self,
        stream: Stream,
        consumed_lifetime_offset: u64,
    ) -> Result<(), Error> {
        self.session
            .acknowledge_output(&self.route, stream, consumed_lifetime_offset)
    }

    pub(crate) async fn close_stdin(&self) -> Result<(), Error> {
        self.session
            .send_control(self.route.process_id, process::NativeControl::CloseStdin)
            .await
    }

    pub(crate) async fn detach(self) -> Result<(), Error> {
        let result = self
            .session
            .send_control(self.route.process_id, process::NativeControl::Detach)
            .await;
        self.session.remove_route(self.route.process_id);
        result
    }
}

async fn route_outbound(
    inner: std::sync::Weak<SessionInner>,
    mut events: mpsc::Receiver<process::NativeEventEnvelope>,
    evictions: Arc<process::Evictions>,
) {
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            () = evictions.notified() => {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                for process_id in evictions.take() {
                    fail_route(&inner, process_id, ROUTE_EVICTED);
                }
                continue;
            }
        };
        let Some(event) = event else {
            break;
        };
        let Some(inner) = inner.upgrade() else {
            return;
        };
        if let Err(error) = event.dispatch(|event| dispatch_outbound(&inner, event)) {
            close_session(&inner, error);
            return;
        }
    }
    if let Some(inner) = inner.upgrade() {
        close_session(
            &inner,
            Error::Closed("Process endpoint writer closed".to_owned()),
        );
    }
}

fn dispatch_outbound(inner: &Arc<SessionInner>, event: process::NativeEvent) -> Result<(), Error> {
    match event {
        process::NativeEvent::Output {
            process_id,
            stream,
            offset,
            data,
        } => {
            // Queued before its route failed or detached: nobody takes it now.
            let Some(route) = inner.routes.lock().unwrap().get(&process_id).cloned() else {
                return Ok(());
            };
            let semantic_stream = match stream {
                process::NATIVE_STREAM_STDOUT => Stream::Stdout,
                process::NATIVE_STREAM_STDERR => Stream::Stderr,
                _ => return Err(Error::Closed("invalid Process output stream".to_owned())),
            };
            let end = offset
                .checked_add(data.len() as u64)
                .ok_or_else(|| Error::Closed("Process output offset overflow".to_owned()))?;
            if route.auto_ack_output {
                inner
                    .manager
                    .acknowledge_output_native(process_id, stream, end)
                    .map_err(backend_error)?;
                return Ok(());
            }
            if route
                .events
                .try_send(Event::Output {
                    stream: semantic_stream,
                    lifetime_offset: offset,
                    data,
                })
                .is_err()
            {
                // This attachment's reader fell behind: it alone fails.
                fail_route(inner, process_id, ROUTE_EVICTED);
                let _ = inner
                    .manager
                    .control_native(process_id, process::NativeControl::Detach);
            }
            Ok(())
        }
        process::NativeEvent::StdinProgress {
            process_id,
            consumed,
            open,
        } => {
            let Some(route) = inner.routes.lock().unwrap().get(&process_id).cloned() else {
                return Ok(());
            };
            if route
                .events
                .try_send(Event::StdinProgress { consumed, open })
                .is_err()
            {
                fail_route(inner, process_id, ROUTE_EVICTED);
                let _ = inner
                    .manager
                    .control_native(process_id, process::NativeControl::Detach);
            }
            Ok(())
        }
        process::NativeEvent::Exit { process_id, exit } => {
            let exit = native_exit_info(exit);
            let route = {
                let mut routes = inner.routes.lock().unwrap();
                // Its route failed as the exit was queued: that attachment is gone already. No
                // replay is recorded (the handle left with the route), so a WAIT in this session
                // answers CONFLICT while the exit is in flight, then NOT_FOUND.
                let Some(route) = routes.get(&process_id).cloned() else {
                    return Ok(());
                };
                let process_handle = route.process_handle.load(Ordering::Acquire);
                if process_handle != 0 {
                    // Record the replay before the route disappears: a WAIT
                    // that no longer finds the route must find the exit.
                    inner
                        .exits
                        .lock()
                        .unwrap()
                        .insert(process_handle, exit.clone());
                }
                routes.remove(&process_id);
                route.exit.send_replace(Some(exit.clone()));
                route
            };
            let _ = route.events.try_send(Event::Exit(exit));
            Ok(())
        }
    }
}

/// The route of `process_id` leaves the session and its attachment and WAITs fail with
/// `reason`; the session and its other processes go on.
fn fail_route(inner: &SessionInner, process_id: u32, reason: &str) {
    let route = inner.routes.lock().unwrap().remove(&process_id);
    if let Some(route) = route {
        route.failed.send_replace(Some(reason.to_owned()));
    }
}

fn close_session(inner: &SessionInner, error: Error) {
    if inner.closed.borrow().is_some() {
        return;
    }
    // send_replace: nobody subscribes, and send would drop the value.
    inner.closed.send_replace(Some(error));
    inner.routes.lock().unwrap().clear();
    inner.exits.lock().unwrap().clear();
}

fn resolve_cwd(cwd: &wire::Cwd, resolved: Option<Vec<u8>>) -> Result<Option<Vec<u8>>, Error> {
    match cwd {
        wire::Cwd::ServerDefault => Ok(None),
        wire::Cwd::Path(path) => Ok(Some(path.clone())),
        wire::Cwd::Terminal(_) | wire::Cwd::Fs { .. } => resolved
            .map(Some)
            .ok_or_else(|| Error::Invalid("Process cwd handle was not resolved".to_owned())),
    }
}

fn native_control(
    action: wire::ControlAction,
    value: u16,
) -> Result<process::NativeControl, Error> {
    match action {
        wire::ControlAction::Signal => Ok(process::NativeControl::Signal(native_signal(value)?)),
        wire::ControlAction::Terminate => Ok(process::NativeControl::Terminate),
        wire::ControlAction::Kill => Ok(process::NativeControl::Kill),
        wire::ControlAction::Detach => Ok(process::NativeControl::Detach),
    }
}

#[cfg(unix)]
fn native_signal(value: u16) -> Result<u32, Error> {
    let signal = match value {
        value if value == schema::process::SIGNAL_INTERRUPT as u16 => libc::SIGINT,
        value if value == schema::process::SIGNAL_TERMINATE as u16 => libc::SIGTERM,
        value if value == schema::process::SIGNAL_KILL as u16 => libc::SIGKILL,
        value if value == schema::process::SIGNAL_HANGUP as u16 => libc::SIGHUP,
        _ => return Err(Error::Invalid("unknown portable Process signal".to_owned())),
    };
    Ok(signal as u32)
}

#[cfg(windows)]
fn native_signal(value: u16) -> Result<u32, Error> {
    use process::windows_signal;
    match value {
        value if value == schema::process::SIGNAL_INTERRUPT as u16 => Ok(windows_signal::INTERRUPT),
        value if value == schema::process::SIGNAL_TERMINATE as u16 => Ok(windows_signal::TERMINATE),
        value if value == schema::process::SIGNAL_KILL as u16 => Ok(windows_signal::KILL),
        value if value == schema::process::SIGNAL_HANGUP as u16 => Ok(windows_signal::HANGUP),
        _ => Err(Error::Invalid("unknown portable Process signal".to_owned())),
    }
}

fn process_record(record: NativeRecord, now_server_ns: u64) -> Result<wire::ProcessRecord, Error> {
    let flags = u16::from(record.flags & process::NATIVE_CATALOG_FLAGS);
    let stream_state = if record.running {
        let mut state = 0u8;
        if record.stream_state
            & (process::NATIVE_STREAM_STDIN_ACCEPTING | process::NATIVE_STREAM_STDIN_CLOSING)
            != 0
        {
            state |= schema::process::STREAM_STDIN_OPEN as u8;
        }
        if record.stream_state & process::NATIVE_STREAM_STDOUT_OPEN != 0 {
            state |= schema::process::STREAM_STDOUT_OPEN as u8;
        }
        if record.stream_state & process::NATIVE_STREAM_STDERR_OPEN != 0 {
            state |= schema::process::STREAM_STDERR_OPEN as u8;
        }
        state
    } else {
        0
    };
    let exit = record
        .exit
        .map(native_exit_info)
        .map(|exit| exit.into_record(now_server_ns));
    Ok(wire::ProcessRecord {
        process_handle: record.process_handle,
        lifecycle: if record.running {
            schema::process::LIFECYCLE_RUNNING as u8
        } else {
            schema::process::LIFECYCLE_EXITED as u8
        },
        stream_state,
        flags,
        native_pid: u64::from(record.native_pid),
        owner_session: record.owner_session,
        argv0: record.argv0,
        stdin_received: record.stdin_received,
        stdout_produced: record.stdout_produced,
        stderr_produced: record.stderr_produced,
        retention_deadline_server_ns: if !record.running
            && flags & schema::process::SPAWN_DETACHABLE as u16 != 0
        {
            now_server_ns.saturating_add(schema::process::MAX_DETACHED_RETENTION_NS)
        } else {
            0
        },
        exit,
        extensions: Default::default(),
    })
}

fn native_exit_info(exit: process::NativeExit) -> ExitInfo {
    ExitInfo {
        kind: exit.kind,
        reason: exit.reason,
        code: exit.code,
        detail: exit.detail,
    }
}

fn backend_error(error: process::NativeError) -> Error {
    match error {
        process::NativeError::NotFound => Error::NotFound,
        process::NativeError::Conflict => Error::Conflict,
        process::NativeError::Permission => Error::Permission,
        process::NativeError::ResourceExhausted => Error::ResourceExhausted,
        process::NativeError::Invalid(detail) => Error::Invalid(detail),
        process::NativeError::Io(detail) => Error::Io(detail),
        process::NativeError::Closed(detail) => Error::Closed(detail),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use yas_wire::{Extension, Extensions, process::EnvEntry};

    #[test]
    fn exit_replays_are_bounded_retryable_and_fifo_evicted() {
        const MAX_EXIT_REPLAYS_PER_SESSION: usize = schema::process::MAX_PROCESSES as usize;
        assert_eq!(
            process::ProcessMaxima::DEFAULT.exit_replays(),
            MAX_EXIT_REPLAYS_PER_SESSION
        );
        let mut exits = ExitReplays::new(MAX_EXIT_REPLAYS_PER_SESSION);
        for process_handle in 1..=MAX_EXIT_REPLAYS_PER_SESSION as u64 + 1 {
            exits.insert(
                process_handle,
                ExitInfo {
                    kind: wire::ExitKind::Code,
                    reason: 0,
                    code: process_handle as i32,
                    detail: Vec::new(),
                },
            );
        }
        assert_eq!(exits.values.len(), MAX_EXIT_REPLAYS_PER_SESSION);
        assert_eq!(exits.order.len(), MAX_EXIT_REPLAYS_PER_SESSION);
        assert!(exits.get(1).is_none(), "oldest terminal replay is evicted");
        let newest = MAX_EXIT_REPLAYS_PER_SESSION as u64 + 1;
        assert_eq!(exits.get(newest).unwrap().code, newest as i32);
        // WAIT has no operation ID, so successful delivery remains retryable
        // until ordinary FIFO churn evicts the replay.
        assert_eq!(exits.get(newest).unwrap().code, newest as i32);
    }

    fn spawn_request(argv: Vec<Vec<u8>>, env: Vec<EnvEntry>) -> wire::Spawn {
        wire::Spawn {
            operation_id: [7; 16],
            flags: 0,
            environment_kind: wire::EnvironmentKind::Empty,
            cwd: wire::Cwd::ServerDefault,
            argv,
            env,
            stdout_receive_credit: 1024 * 1024,
            stderr_receive_credit: 1024 * 1024,
            extensions: Extensions::default(),
        }
    }

    fn executable(name: &str) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt;
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("{name} is not on PATH"))
            .as_os_str()
            .as_bytes()
            .to_vec()
    }

    #[tokio::test]
    async fn surface_app_launcher_keeps_its_process_group_alive() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([8; 16], None).unwrap();
        let sleep = String::from_utf8(executable("sleep")).unwrap();
        let mut request = spawn_request(
            vec![
                executable("sh"),
                b"-c".to_vec(),
                format!("({sleep} 0.15; printf survived) &").into_bytes(),
            ],
            Vec::new(),
        );
        request.flags =
            (schema::process::SPAWN_DETACHABLE | schema::process::SPAWN_MERGE_STDERR) as u16;
        request.stderr_receive_credit = 0;
        request.extensions = Extensions(vec![Extension {
            tag: schema::process::SPAWN_SURFACE_APP_EXTENSION as u16,
            required: true,
            value: 1u64.to_le_bytes().to_vec(),
        }]);

        let mut attachment = session.spawn(&request, None).await.unwrap();
        let mut output = Vec::new();
        let exit = loop {
            match tokio::time::timeout(Duration::from_secs(2), attachment.next())
                .await
                .unwrap()
                .unwrap()
            {
                Event::Output { data, .. } => output.extend_from_slice(&data),
                Event::Exit(exit) => break exit,
                Event::StdinProgress { .. } => {}
            }
        };
        assert_eq!(output, b"survived");
        assert_eq!(exit.kind, wire::ExitKind::Code);
        assert_eq!(exit.code, 0);
        session.shutdown().await;
        server.shutdown().await;
    }

    /// Output and the exit of a spawned attachment, crediting output as it comes.
    async fn output_and_exit(
        attachment: &mut Attachment,
        within: Duration,
        mut acked: u64,
    ) -> (Vec<u8>, ExitInfo) {
        let mut output = Vec::new();
        loop {
            match tokio::time::timeout(within, attachment.next())
                .await
                .expect("an event in time")
                .expect("the attachment is open")
            {
                Event::Output { stream, data, .. } => {
                    output.extend_from_slice(&data);
                    acked += data.len() as u64;
                    attachment.acknowledge_output(stream, acked).unwrap();
                }
                Event::Exit(exit) => return (output, exit),
                Event::StdinProgress { .. } => {}
            }
        }
    }

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn residue_request(script: String, grace: Option<Duration>) -> wire::Spawn {
        let mut request = spawn_request(
            vec![executable("sh"), b"-c".to_vec(), script.into_bytes()],
            Vec::new(),
        );
        request.flags = (schema::process::SPAWN_LEAVE_RESIDUE
            | schema::process::SPAWN_MERGE_STDERR
            | schema::process::SPAWN_STDIN_NULL) as u16;
        request.stderr_receive_credit = 0;
        if let Some(grace) = grace {
            request.extensions = Extensions(vec![Extension {
                tag: schema::process::SPAWN_RESIDUE_GRACE_EXTENSION as u16,
                required: true,
                value: (grace.as_nanos() as u64).to_le_bytes().to_vec(),
            }]);
        }
        request
    }

    #[tokio::test]
    async fn leave_residue_reports_the_exit_after_its_grace_and_leaves_the_group_running() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([11; 16], None).unwrap();
        let sleep = String::from_utf8(executable("sleep")).unwrap();
        let started = std::time::Instant::now();
        let mut attachment = session
            .spawn(
                &residue_request(
                    format!("{sleep} 30 & echo $!; echo started; exit 3"),
                    Some(Duration::from_millis(300)),
                ),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            attachment.stdin_window, 0,
            "the null device has no stdin Transfer"
        );
        let (output, exit) = output_and_exit(&mut attachment, Duration::from_secs(5), 0).await;
        let elapsed = started.elapsed();
        let text = String::from_utf8(output).unwrap();
        let mut lines = text.lines();
        let pid: i32 = lines.next().unwrap().parse().unwrap();
        assert_eq!(lines.next(), Some("started"));
        assert_eq!(exit.kind, wire::ExitKind::Code);
        assert_eq!(exit.code, 3);
        assert_eq!(exit.detail, b"residual process group left running");
        assert!(
            elapsed >= Duration::from_millis(300),
            "waited for the grace: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "did not wait for the residue: {elapsed:?}"
        );
        // Neither the exit, nor the session's end, nor the server's stops what was left running.
        assert!(alive(pid), "the residue runs after the exit");
        session.shutdown().await;
        server.shutdown().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(alive(pid), "the residue outlives its session and server");
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    #[tokio::test]
    async fn leave_residue_without_a_grace_forwards_output_until_the_streams_close() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([12; 16], None).unwrap();
        let sleep = String::from_utf8(executable("sleep")).unwrap();
        let mut attachment = session
            .spawn(
                &residue_request(format!("({sleep} 0.4; printf later) & printf now"), None),
                None,
            )
            .await
            .unwrap();
        let (output, exit) = output_and_exit(&mut attachment, Duration::from_secs(5), 0).await;
        assert_eq!(output, b"nowlater");
        assert_eq!(exit.kind, wire::ExitKind::Code);
        assert_eq!(exit.code, 0);
        assert!(
            exit.detail.is_empty(),
            "{:?}",
            String::from_utf8_lossy(&exit.detail)
        );
        session.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn leave_residue_terminate_kills_the_whole_group_after_the_kill_grace() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([13; 16], None).unwrap();
        let sleep = String::from_utf8(executable("sleep")).unwrap();
        let sh = String::from_utf8(executable("sh")).unwrap();
        // A member that ignores SIGTERM (it reports its pid once the trap is set), and a leader
        // waiting on a foreground child.
        let mut attachment = session
            .spawn(
                &residue_request(
                    format!("{sh} -c \"trap \\\"\\\" TERM; echo \\$\\$; {sleep} 30\" & {sleep} 30"),
                    Some(Duration::from_millis(100)),
                ),
                None,
            )
            .await
            .unwrap();
        let (pid, acked) = loop {
            match tokio::time::timeout(Duration::from_secs(5), attachment.next())
                .await
                .unwrap()
                .unwrap()
            {
                Event::Output { stream, data, .. } => {
                    let acked = data.len() as u64;
                    attachment.acknowledge_output(stream, acked).unwrap();
                    let pid = String::from_utf8(data)
                        .unwrap()
                        .trim()
                        .parse::<i32>()
                        .unwrap();
                    break (pid, acked);
                }
                Event::Exit(exit) => panic!("exited early: {exit:?}"),
                Event::StdinProgress { .. } => {}
            }
        };
        let started = std::time::Instant::now();
        session
            .control(&wire::Control {
                process_handle: attachment.process_handle,
                operation_id: [14; 16],
                action: wire::ControlAction::Terminate,
                value: 0,
                extensions: Extensions::default(),
            })
            .await
            .unwrap();
        let (_, exit) = output_and_exit(&mut attachment, Duration::from_secs(5), acked).await;
        assert_eq!(exit.kind, wire::ExitKind::Signal);
        assert_eq!(exit.code, libc::SIGTERM);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the exit waits for no residue"
        );
        assert!(alive(pid), "the member ignoring SIGTERM outlives it");
        tokio::time::sleep(Duration::from_millis(2_500).saturating_sub(started.elapsed())).await;
        assert!(!alive(pid), "the escalation killed the group");
        session.shutdown().await;
        server.shutdown().await;
    }

    fn watch_request(handle: u64) -> wire::Attach {
        wire::Attach {
            process_handle: handle,
            flags: 0,
            stdout_receive_credit: 1 << 20,
            stderr_receive_credit: 1 << 20,
            extensions: Extensions::default(),
        }
    }

    /// `sh -c SCRIPT`, SCRIPT's programs named by absolute path (spawn_request's environment is
    /// empty).
    fn sh(script: &str) -> Vec<Vec<u8>> {
        let mut script = script.to_owned();
        for name in ["head", "sleep", "yes"] {
            let path = String::from_utf8(executable(name)).unwrap();
            script = script.replace(&format!("{{{name}}}"), &path);
        }
        vec![executable("sh"), b"-c".to_vec(), script.into_bytes()]
    }

    /// What a command a session starts then says: its output and exit.
    async fn runs_a_command(session: &Session) -> (Vec<u8>, i32) {
        let mut after = session
            .spawn(
                &spawn_request(vec![executable("echo"), b"after".to_vec()], Vec::new()),
                None,
            )
            .await
            .unwrap();
        let (output, exit) = output_and_exit(&mut after, Duration::from_secs(5), 0).await;
        (output, exit.code)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_evicted_watcher_gives_back_its_process_slot() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let owner = runtime.session([4; 16], None).unwrap();
        let watcher = runtime.session([5; 16], None).unwrap();
        // More rounds than a session has process slots (16).
        for round in 0..20 {
            let mut attachment = owner
                .spawn(
                    &spawn_request(sh("{sleep} 0.2; {head} -c 3145728 /dev/zero"), Vec::new()),
                    None,
                )
                .await
                .unwrap();
            let watched = watcher
                .attach(&watch_request(attachment.process_handle))
                .await
                .unwrap_or_else(|error| panic!("round {round}: {error:?}"));
            let (control, mut events) = watched.split();
            let (output, exit) = output_and_exit(&mut attachment, Duration::from_secs(10), 0).await;
            assert_eq!((output.len(), exit.code), (3145728, 0), "round {round}");
            // The watcher never acknowledges: it falls a window behind and is dropped.
            while tokio::time::timeout(Duration::from_secs(5), events.next())
                .await
                .unwrap()
                .is_some()
            {}
            assert!(events.failure().is_some(), "round {round}");
            // What the adapter does next.
            let _ = control.detach().await;
        }
        assert_eq!(runs_a_command(&watcher).await, (b"after\n".to_vec(), 0));
        owner.shutdown().await;
        watcher.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_watcher_whose_queue_fills_after_the_exit_fails_alone() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let owner = runtime.session([6; 16], None).unwrap();
        let watcher = runtime.session([7; 16], None).unwrap();
        let mut attachment = owner
            .spawn(
                &spawn_request(
                    sh(
                        "{sleep} 0.3; i=0; while [ $i -lt 80 ]; do echo x; {sleep} 0.02; \
                        i=$((i+1)); done; printf z; exit 0",
                    ),
                    Vec::new(),
                ),
                None,
            )
            .await
            .unwrap();
        // Never read: its route queue (80 events) fills with the 80 lines, and the last
        // frame comes after the child exits.
        let _watched = watcher
            .attach(&watch_request(attachment.process_handle))
            .await
            .unwrap();
        // The owner acknowledges its first 48 frames only: with 32 unacknowledged, the reader
        // waits for it, and `z` stays in the pipe as the child exits.
        let (mut frames, mut end) = (0, 0u64);
        while frames < 80 {
            match tokio::time::timeout(Duration::from_secs(5), attachment.next())
                .await
                .unwrap()
                .unwrap()
            {
                Event::Output {
                    stream,
                    lifetime_offset,
                    data,
                } => {
                    frames += 1;
                    end = lifetime_offset + data.len() as u64;
                    if frames <= 48 {
                        attachment.acknowledge_output(stream, end).unwrap();
                    }
                }
                other => panic!("{other:?}"),
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        attachment.acknowledge_output(Stream::Stdout, end).unwrap();
        let (rest, exit) = output_and_exit(&mut attachment, Duration::from_secs(5), end).await;
        assert_eq!((rest, exit.code), (b"z".to_vec(), 0));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(watcher.inner.closed.borrow().is_none());
        assert_eq!(runs_a_command(&watcher).await, (b"after\n".to_vec(), 0));
        owner.shutdown().await;
        watcher.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_owner_that_drops_its_stream_after_the_exit_still_waits_for_it() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let owner = runtime.session([10; 16], None).unwrap();
        // The reader takes a window (1 MiB) and waits for the owner; the last 32 KiB stay in
        // the pipe as the child exits.
        let attachment = owner
            .spawn(
                &spawn_request(sh("{head} -c 1081344 /dev/zero; exit 4"), Vec::new()),
                None,
            )
            .await
            .unwrap();
        let handle = attachment.process_handle;
        let (control, _events) = attachment.split();
        tokio::time::sleep(Duration::from_millis(800)).await;
        // What the adapter does when its client drops the stream.
        control.detach().await.unwrap();
        let wait = wire::Wait {
            process_handle: handle,
            timeout_ns: 5_000_000_000,
            extensions: Extensions::default(),
        };
        let exit = owner.wait(&wait).await.unwrap();
        assert_eq!((exit.code, exit.detail.as_slice()), (4, &b""[..]));
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_owner_slow_to_take_its_window_after_the_exit_gets_all_its_output() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let owner = runtime.session([12; 16], None).unwrap();
        let mut attachment = owner
            .spawn(
                &spawn_request(sh("{head} -c 1081344 /dev/zero; exit 4"), Vec::new()),
                None,
            )
            .await
            .unwrap();
        // Longer than the kill grace (2 s): a slow link, or a client that waits for the exit
        // before it reads.
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let (output, exit) = output_and_exit(&mut attachment, Duration::from_secs(5), 0).await;
        assert_eq!(output.len(), 1081344);
        assert_eq!((exit.code, exit.detail.as_slice()), (4, &b""[..]));
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_residue_owner_slow_to_take_its_window_after_the_exit_gets_all_its_output() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let owner = runtime.session([13; 16], None).unwrap();
        let mut request = residue_request(String::new(), Some(Duration::from_millis(300)));
        request.argv = sh("{head} -c 1081344 /dev/zero; exit 4");
        let mut attachment = owner.spawn(&request, None).await.unwrap();
        // Longer than its grace: nothing of the group is left, so the grace does not cut it.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let (output, exit) = output_and_exit(&mut attachment, Duration::from_secs(5), 0).await;
        assert_eq!(output.len(), 1081344);
        assert_eq!((exit.code, exit.detail.as_slice()), (4, &b""[..]));
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_residue_writing_on_ends_its_grace_though_the_owner_reads_nothing() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let owner = runtime.session([14; 16], None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut request = residue_request(String::new(), Some(Duration::from_millis(300)));
        request.argv = sh(&format!(
            "{{yes}} & echo $! > '{}'; exit 0",
            pid_file.display()
        ));
        let started = std::time::Instant::now();
        let mut attachment = owner.spawn(&request, None).await.unwrap();
        // The owner takes nothing until the exit: the residue fills its window and blocks.
        let exit = loop {
            if let Event::Exit(exit) =
                tokio::time::timeout(Duration::from_secs(5), attachment.next())
                    .await
                    .unwrap()
                    .unwrap()
            {
                break exit;
            }
        };
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(exit.code, 0);
        assert_eq!(exit.detail, b"residual process group left running");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(alive(pid), "the residue is left running");
        unsafe { libc::kill(pid, libc::SIGKILL) };
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn stdin_null_gives_the_child_the_null_device() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([15; 16], None).unwrap();
        // A character device (the null device) rather than a pipe; /dev/stdin
        // names fd 0 on Linux and macOS alike, where /proc does not exist.
        let mut request = spawn_request(
            vec![
                executable("sh"),
                b"-c".to_vec(),
                b"if [ -c /dev/stdin ]; then echo character-device; else echo other; fi".to_vec(),
            ],
            Vec::new(),
        );
        request.flags = schema::process::SPAWN_STDIN_NULL as u16;
        let mut attachment = session.spawn(&request, None).await.unwrap();
        assert_eq!(attachment.stdin_window, 0);
        let (output, exit) = output_and_exit(&mut attachment, Duration::from_secs(5), 0).await;
        assert_eq!(output, b"character-device\n");
        assert_eq!(exit.code, 0);
        session.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn typed_spawn_stream_credit_exit_and_catalogue() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let before = runtime.snapshot(1).unwrap();
        assert_eq!(before.revision, 1);
        assert!(before.records.is_empty());

        let session = runtime.session([9; 16], None).unwrap();
        let attachment = session
            .spawn(&spawn_request(vec![executable("cat")], Vec::new()), None)
            .await
            .unwrap();
        assert_ne!(attachment.process_handle, 0);
        assert_ne!(attachment.stdin_window, 0);
        let (control, mut events) = attachment.split();

        let changed = runtime.changed(before.revision, || 2).await.unwrap();
        assert_eq!(changed.records.len(), 1);
        assert_eq!(changed.records[0].owner_session, [9; 16]);
        assert!(changed.records[0].argv0.ends_with(b"/cat"));

        control.write_stdin(0, b"hello\n").unwrap();
        let mut saw_ack = false;
        let mut saw_output = false;
        while !saw_ack || !saw_output {
            match tokio::time::timeout(Duration::from_secs(2), events.next())
                .await
                .unwrap()
                .unwrap()
            {
                Event::StdinProgress { consumed, .. } => {
                    assert_eq!(consumed, 6);
                    saw_ack = true;
                }
                Event::Output {
                    stream,
                    lifetime_offset,
                    data,
                } => {
                    assert_eq!(stream, Stream::Stdout);
                    assert_eq!(lifetime_offset, 0);
                    assert_eq!(data, b"hello\n");
                    control
                        .acknowledge_output(stream, data.len() as u64)
                        .unwrap();
                    saw_output = true;
                }
                Event::Exit(exit) => panic!("cat exited early: {exit:?}"),
            }
        }
        control.close_stdin().await.unwrap();
        let exit = loop {
            if let Event::Exit(exit) = tokio::time::timeout(Duration::from_secs(2), events.next())
                .await
                .unwrap()
                .unwrap()
            {
                break exit;
            }
        };
        assert_eq!(exit.kind, wire::ExitKind::Code);
        assert_eq!(exit.code, 0);
        session.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn wait_after_a_fast_exit_returns_the_exit_record() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([4; 16], None).unwrap();
        for _ in 0..50 {
            let mut attachment = session
                .spawn(
                    &spawn_request(
                        vec![executable("sh"), b"-c".to_vec(), b"exit 7".to_vec()],
                        Vec::new(),
                    ),
                    None,
                )
                .await
                .unwrap();
            let handle = attachment.process_handle;
            while !matches!(
                tokio::time::timeout(Duration::from_secs(5), attachment.next())
                    .await
                    .unwrap()
                    .unwrap(),
                Event::Exit(_)
            ) {}
            let exit = session
                .wait(&wire::Wait {
                    process_handle: handle,
                    timeout_ns: 5_000_000_000,
                    extensions: Extensions::default(),
                })
                .await
                .unwrap();
            assert_eq!(exit.code, 7);
        }
        session.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn empty_environment_is_exact_and_wait_observes_exit() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([3; 16], None).unwrap();
        let mut attachment = session
            .spawn(
                &spawn_request(
                    vec![b"/usr/bin/env".to_vec()],
                    vec![EnvEntry {
                        key: b"YAS_PROCESS_TEST".to_vec(),
                        value: b"exact".to_vec(),
                    }],
                ),
                None,
            )
            .await
            .unwrap();
        let handle = attachment.process_handle;
        let mut output = Vec::new();
        let mut acknowledgements = Vec::new();
        let exit = loop {
            match tokio::time::timeout(Duration::from_secs(2), attachment.next())
                .await
                .unwrap()
                .unwrap()
            {
                Event::Output {
                    stream,
                    lifetime_offset,
                    data,
                } => {
                    output.extend_from_slice(&data);
                    acknowledgements
                        .push((stream, lifetime_offset.saturating_add(data.len() as u64)));
                }
                Event::Exit(exit) => break exit,
                Event::StdinProgress { .. } => {}
            }
        };
        assert_eq!(output, b"YAS_PROCESS_TEST=exact\n");
        assert_eq!(exit.code, 0);

        // Output delivery and terminal publication run independently. Final
        // output acknowledgements remain valid even after EXIT retires the
        // native binding.
        for (stream, consumed) in acknowledgements {
            attachment.acknowledge_output(stream, consumed).unwrap();
        }

        // The route's exit watch remains usable after terminal delivery.
        let waited = session
            .wait(&wire::Wait {
                process_handle: handle,
                timeout_ns: Duration::from_secs(1).as_nanos() as u64,
                extensions: Extensions::default(),
            })
            .await
            .unwrap();
        assert_eq!(waited, exit);
        session.shutdown().await;
        server.shutdown().await;
    }
}

/// Windows process semantics against real children: console control, jobs, LEAVE_RESIDUE.
/// `ping` answers CTRL_BREAK with statistics and keeps going, so it stands for a process that
/// ignores it; `waitfor` has no handler, so CTRL_BREAK ends it with STATUS_CONTROL_C_EXIT.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        TerminateProcess,
    };
    use yas_wire::{Extension, Extensions};

    /// STATUS_CONTROL_C_EXIT, as the exit code's bits.
    const CONTROL_C_EXIT: i32 = 0xC000_013A_u32 as i32;

    fn system32(program: &str) -> Vec<u8> {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        format!(r"{root}\System32\{program}").into_bytes()
    }

    fn request(argv: &[&[u8]], flags: u64, operation: u8) -> wire::Spawn {
        wire::Spawn {
            operation_id: [operation; 16],
            flags: (flags | schema::process::SPAWN_MERGE_STDERR | schema::process::SPAWN_STDIN_NULL)
                as u16,
            environment_kind: wire::EnvironmentKind::Session,
            cwd: wire::Cwd::ServerDefault,
            argv: argv.iter().map(|value| value.to_vec()).collect(),
            env: Vec::new(),
            stdout_receive_credit: 1024 * 1024,
            stderr_receive_credit: 0,
            extensions: Extensions::default(),
        }
    }

    fn with_residue_grace(mut request: wire::Spawn, grace: Duration) -> wire::Spawn {
        request.extensions = Extensions(vec![Extension {
            tag: schema::process::SPAWN_RESIDUE_GRACE_EXTENSION as u16,
            required: true,
            value: (grace.as_nanos() as u64).to_le_bytes().to_vec(),
        }]);
        request
    }

    fn powershell(script: &str) -> Vec<Vec<u8>> {
        vec![
            system32(r"WindowsPowerShell\v1.0\powershell.exe"),
            b"-NoProfile".to_vec(),
            b"-NonInteractive".to_vec(),
            b"-Command".to_vec(),
            script.as_bytes().to_vec(),
        ]
    }

    /// Starts a `ping` that shares the script's console and pipes, prints its PID on a line of
    /// its own, then runs `rest`.
    fn residue_script(rest: &str) -> Vec<Vec<u8>> {
        powershell(&format!(
            "$p = Start-Process -FilePath ping -ArgumentList '-n','120','127.0.0.1' \
             -NoNewWindow -PassThru; [Console]::Out.WriteLine('PID ' + $p.Id); {rest}"
        ))
    }

    async fn control(session: &Session, handle: u64, action: wire::ControlAction, value: u16) {
        session
            .control(&wire::Control {
                process_handle: handle,
                operation_id: rand_operation(),
                action,
                value,
                extensions: Extensions::default(),
            })
            .await
            .unwrap();
    }

    fn rand_operation() -> [u8; 16] {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let mut id = [0x5a; 16];
        id[..8].copy_from_slice(&NEXT.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        id
    }

    /// Reads output until `until` finds what it wants in it, crediting as it goes.
    async fn read_until<T>(
        attachment: &mut Attachment,
        acked: &mut u64,
        mut until: impl FnMut(&str) -> Option<T>,
    ) -> T {
        let mut output = String::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(20), attachment.next())
                .await
                .expect("output in time")
                .expect("the attachment is open")
            {
                Event::Output { stream, data, .. } => {
                    *acked += data.len() as u64;
                    attachment.acknowledge_output(stream, *acked).unwrap();
                    output.push_str(&String::from_utf8_lossy(&data));
                    if let Some(found) = until(&output) {
                        return found;
                    }
                }
                Event::Exit(exit) => panic!("exited early: {exit:?}; output {output:?}"),
                Event::StdinProgress { .. } => {}
            }
        }
    }

    async fn exit_of(attachment: &mut Attachment, acked: &mut u64, within: Duration) -> ExitInfo {
        loop {
            match tokio::time::timeout(within, attachment.next())
                .await
                .expect("the exit in time")
                .expect("the attachment is open")
            {
                Event::Output { stream, data, .. } => {
                    *acked += data.len() as u64;
                    attachment.acknowledge_output(stream, *acked).unwrap();
                }
                Event::Exit(exit) => return exit,
                Event::StdinProgress { .. } => {}
            }
        }
    }

    fn residue_pid(output: &str) -> Option<u32> {
        output
            .lines()
            .find_map(|line| line.trim().strip_prefix("PID "))
            .and_then(|pid| pid.trim().parse().ok())
    }

    fn alive(pid: u32) -> bool {
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return false;
            }
            let mut code = 0u32;
            let running =
                GetExitCodeProcess(process, &mut code) != 0 && code == STILL_ACTIVE as u32;
            CloseHandle(process);
            running
        }
    }

    fn kill(pid: u32) {
        unsafe {
            let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !process.is_null() {
                TerminateProcess(process, 1);
                CloseHandle(process);
            }
        }
    }

    async fn gone_within(pid: u32, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        !alive(pid)
    }

    #[test]
    fn leave_residue_is_offered() {
        let runtime = Runtime::new(Server::new(false, true));
        let flags = runtime.limits().launcher_flags;
        assert_ne!(flags & schema::process::SPAWN_LEAVE_RESIDUE as u32, 0);
        assert_ne!(flags & schema::process::SPAWN_STDIN_NULL as u32, 0);
    }

    #[tokio::test]
    async fn terminate_sends_ctrl_break_to_the_group() {
        ctrl_break_ends_waitfor().await;
    }

    async fn ctrl_break_ends_waitfor() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([31; 16], None).unwrap();
        let mut attachment = session
            .spawn(
                &request(
                    &[
                        &system32("waitfor.exe"),
                        b"/t",
                        b"60",
                        b"yastestneversignalled",
                    ],
                    0,
                    1,
                ),
                None,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let started = std::time::Instant::now();
        control(
            &session,
            attachment.process_handle,
            wire::ControlAction::Terminate,
            0,
        )
        .await;
        let mut acked = 0;
        let exit = exit_of(&mut attachment, &mut acked, Duration::from_secs(10)).await;
        assert_eq!(
            (exit.kind, exit.code),
            (wire::ExitKind::Code, CONTROL_C_EXIT),
            "CTRL_BREAK ended it: {exit:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1_500),
            "before the kill grace: {:?}",
            started.elapsed()
        );
        session.shutdown().await;
        server.shutdown().await;
    }

    /// A server without a console (as `yas connect` starts one) reaches the group through the
    /// child's own hidden console. Ignored: it gives up this process's console, which the other
    /// tests share; run it alone (`--ignored without_a_console`).
    #[tokio::test]
    #[ignore]
    async fn without_a_console_terminate_still_sends_ctrl_break() {
        unsafe { windows_sys::Win32::System::Console::FreeConsole() };
        ctrl_break_ends_waitfor().await;
    }

    #[tokio::test]
    async fn terminate_never_leaves_a_process_that_ignores_ctrl_break_running() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([32; 16], None).unwrap();
        let mut attachment = session
            .spawn(
                &request(&[&system32("PING.EXE"), b"-n", b"120", b"127.0.0.1"], 0, 2),
                None,
            )
            .await
            .unwrap();
        let mut acked = 0;
        read_until(&mut attachment, &mut acked, |output| {
            output.contains("127.0.0.1").then_some(())
        })
        .await;
        let started = std::time::Instant::now();
        control(
            &session,
            attachment.process_handle,
            wire::ControlAction::Terminate,
            0,
        )
        .await;
        let exit = exit_of(&mut attachment, &mut acked, Duration::from_secs(10)).await;
        assert_eq!(exit.kind, wire::ExitKind::Killed, "{exit:?}");
        assert_eq!(
            exit.reason,
            schema::process::EXIT_REASON_TERMINATE_TIMEOUT as u8,
            "{exit:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "within the kill grace: {:?}",
            started.elapsed()
        );
        session.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn signals_map_to_ctrl_break_and_the_job() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([33; 16], None).unwrap();
        // INTERRUPT is CTRL_BREAK: waitfor has no handler for it.
        let mut waiting = session
            .spawn(
                &request(
                    &[
                        &system32("waitfor.exe"),
                        b"/t",
                        b"60",
                        b"yastestneverinterrupted",
                    ],
                    0,
                    3,
                ),
                None,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        control(
            &session,
            waiting.process_handle,
            wire::ControlAction::Signal,
            schema::process::SIGNAL_INTERRUPT as u16,
        )
        .await;
        let mut acked = 0;
        let exit = exit_of(&mut waiting, &mut acked, Duration::from_secs(10)).await;
        assert_eq!(
            (exit.kind, exit.code),
            (wire::ExitKind::Code, CONTROL_C_EXIT)
        );
        // KILL ends the job, and says so.
        let mut pinging = session
            .spawn(
                &request(&[&system32("PING.EXE"), b"-n", b"120", b"127.0.0.1"], 0, 4),
                None,
            )
            .await
            .unwrap();
        control(
            &session,
            pinging.process_handle,
            wire::ControlAction::Signal,
            schema::process::SIGNAL_KILL as u16,
        )
        .await;
        let mut acked = 0;
        let exit = exit_of(&mut pinging, &mut acked, Duration::from_secs(5)).await;
        assert_eq!(exit.kind, wire::ExitKind::Killed, "{exit:?}");
        assert_eq!(exit.reason, schema::process::EXIT_REASON_CLIENT as u8);
        session.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn leave_residue_reports_the_exit_after_its_grace_and_leaves_the_job_running() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([34; 16], None).unwrap();
        let started = std::time::Instant::now();
        let mut attachment = session
            .spawn(
                &with_residue_grace(
                    request(
                        &residue_script("exit 3")
                            .iter()
                            .map(Vec::as_slice)
                            .collect::<Vec<_>>(),
                        schema::process::SPAWN_LEAVE_RESIDUE,
                        5,
                    ),
                    Duration::from_millis(500),
                ),
                None,
            )
            .await
            .unwrap();
        let mut acked = 0;
        let pid = read_until(&mut attachment, &mut acked, residue_pid).await;
        let exit = exit_of(&mut attachment, &mut acked, Duration::from_secs(20)).await;
        assert_eq!(
            (exit.kind, exit.code),
            (wire::ExitKind::Code, 3),
            "{exit:?}"
        );
        assert_eq!(exit.detail, b"residual process group left running");
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(alive(pid), "the residue runs after the exit");
        session.shutdown().await;
        server.shutdown().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(alive(pid), "the residue outlives its session and server");
        kill(pid);
    }

    #[tokio::test]
    async fn leave_residue_terminate_ends_the_job_after_the_kill_grace() {
        let server = Server::new(false, true);
        let runtime = Runtime::new(server.clone());
        let session = runtime.session([35; 16], None).unwrap();
        let mut attachment = session
            .spawn(
                &with_residue_grace(
                    request(
                        &residue_script("Start-Sleep -Seconds 120")
                            .iter()
                            .map(Vec::as_slice)
                            .collect::<Vec<_>>(),
                        schema::process::SPAWN_LEAVE_RESIDUE,
                        6,
                    ),
                    Duration::from_millis(100),
                ),
                None,
            )
            .await
            .unwrap();
        let mut acked = 0;
        let pid = read_until(&mut attachment, &mut acked, residue_pid).await;
        let started = std::time::Instant::now();
        control(
            &session,
            attachment.process_handle,
            wire::ControlAction::Terminate,
            0,
        )
        .await;
        let exit = exit_of(&mut attachment, &mut acked, Duration::from_secs(10)).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the exit waits for no residue: {:?} {exit:?}",
            started.elapsed()
        );
        assert!(
            gone_within(
                pid,
                Duration::from_secs(5).saturating_sub(started.elapsed())
            )
            .await,
            "TERMINATE ended the job, ping included"
        );
        session.shutdown().await;
        server.shutdown().await;
    }
}
