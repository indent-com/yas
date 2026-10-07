//! End-to-end tests of the `yas-client` library against real, private YAS
//! servers hosted with `yas_client::host` (fd-channel + private socket).

#![cfg(unix)]

#[path = "support/ssh_server.rs"]
mod ssh_server;

use std::time::{Duration, Instant};

use yas_client::fs::{CaseBehavior, EntryKind, PathModel, Precondition, WriteOptions};
use yas_client::host::{HostOptions, HostedServer};
use yas_client::kv::{KvChange, KvPrecondition};
use yas_client::process::{Command, Signal, Stdin};
use yas_client::{Client, Error, HelloOptions};

const TIMEOUT: Duration = Duration::from_secs(30);

fn options() -> HostOptions {
    options_for(env!("CARGO_BIN_EXE_yas"))
}

fn options_for(binary: impl Into<std::path::PathBuf>) -> HostOptions {
    HostOptions::new(binary)
        .arg("--no-persistent-extensions")
        .env("YAS_EXT", "0")
        .env("YAS_CHANNEL", "0")
        .env("YAS_FONTS", "0")
        .env("YAS_AUDIO", "0")
        .env("YAS_CLIENT_TEST_VARIABLE", "forty-two")
}

async fn start() -> HostedServer {
    tokio::time::timeout(TIMEOUT, HostedServer::start(options()))
        .await
        .expect("hosted server start timed out")
        .expect("hosted server starts")
}

fn server_log(server: &HostedServer) -> String {
    std::fs::read_to_string(server.log_path()).unwrap_or_default()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_server_is_private_and_stops_with_its_channel() {
    let server = start().await;
    let client = server.connect().await.unwrap();
    assert_eq!(client.server_name(), server.name());
    assert!(server.name().starts_with("hosted-"));

    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let socket = std::fs::metadata(server.socket_path()).unwrap();
    assert!(socket.file_type().is_socket());
    let root_mode = std::fs::metadata(server.root())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(root_mode & 0o777, 0o700, "private directory mode");

    // A second session is independent.
    let other = server.connect().await.unwrap();
    assert_ne!(other.session_id(), client.session_id());
    assert_eq!(other.boot_id(), client.boot_id());

    // The CLI (built on the same library) reaches the private socket.
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_yas"))
        .arg("--on")
        .arg(format!("socket:{}", server.socket_path().display()))
        .args(["run", "--", "echo", "via-cli"])
        .env("YAS_PROXY", "0")
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "via-cli\n");

    let root = server.root().to_path_buf();
    let pid = server.pid().unwrap() as i32;
    let status = server.shutdown().await.unwrap();
    assert!(status.success(), "clean exit on channel EOF: {status}");
    assert!(!alive(pid));
    assert!(!root.exists(), "temporary private directory removed");
    let error = client.closed().await;
    assert!(error.is_disconnected(), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_host_stops_the_server() {
    let server = start().await;
    let pid = server.pid().unwrap() as i32;
    drop(server);
    let deadline = Instant::now() + TIMEOUT;
    while alive(pid) {
        // The reaper thread collects the zombie; until then kill(0) succeeds.
        assert!(Instant::now() < deadline, "server outlived its channel");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A session survives a server that takes its time to pick it up. On macOS
/// a socketpair end passed over the fd channel and closed here was flushed
/// by XNU's unix-socket garbage collector whenever a unix socket closed
/// before the server took it, so the session read EOF (see
/// `yas_client::host`, macOS).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_survives_a_server_slow_to_take_it() {
    let server = start().await;
    let pid = server.pid().unwrap() as i32;
    // SAFETY: stops the hosted server only; it is resumed below.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let (client, ()) = tokio::join!(tokio::time::timeout(TIMEOUT, server.connect()), async {
        // Closing unix sockets runs XNU's collector; keep closing some
        // while the server cannot take the session.
        for _ in 0..50 {
            drop(std::os::unix::net::UnixStream::pair().unwrap());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // SAFETY: resumes the server stopped above.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
    });
    let client = client
        .expect("connect timed out")
        .expect("the session survives while the server is stopped");
    let output = client
        .spawn(Command::new("echo").arg("taken"))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(output.stdout, b"taken\n");
    assert!(server.shutdown().await.unwrap().success());
}

/// The server binds its desktop's Unix sockets in its runtime directory, the
/// longest a window's `yas-app-<32 hex>-<16 hex>`, and a socket path holds 103
/// bytes on macOS (107 on Linux). A private root deep enough that `<root>/run`
/// leaves no room for them still gets a runtime directory that does, the
/// server's own: private, and gone with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deep_private_root_gets_a_runtime_directory_short_enough_for_sockets() {
    use std::os::unix::fs::PermissionsExt;
    let base = tempfile::tempdir().unwrap();
    // `<root>/run` about 60 bytes, while `<root>/yas.sock` stays well inside
    // a socket path (macOS's own temporary directory is ~50 bytes already).
    let depth = 56usize.saturating_sub(base.path().as_os_str().len()).max(1);
    let root = base.path().join("r".repeat(depth));
    assert!(root.join("run").as_os_str().len() > 103 - "/yas-app-".len() - 49);
    let server = tokio::time::timeout(TIMEOUT, HostedServer::start(options().root(&root)))
        .await
        .expect("hosted server start timed out")
        .expect("hosted server starts");
    let client = server.connect().await.unwrap();
    let output = client
        .spawn(Command::new("sh").args(["-c", r#"printf %s "$XDG_RUNTIME_DIR""#]))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    let run = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap());
    let app_socket = run.join(format!("yas-app-{}-{}", "0".repeat(32), "0".repeat(16)));
    assert!(
        app_socket.as_os_str().len() <= 103,
        "{} is {} bytes",
        app_socket.display(),
        app_socket.as_os_str().len()
    );
    let mode = std::fs::metadata(&run).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "{} is private", run.display());
    drop(client);
    server.shutdown().await.unwrap();
    assert!(!run.exists(), "{} outlived the server", run.display());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_server_arguments_fail_start_with_the_log() {
    let error = HostedServer::start(options().arg("--definitely-not-a-flag"))
        .await
        .unwrap_err();
    let Error::Connect(message) = &error else {
        panic!("expected Connect, got {error:?}");
    };
    assert!(message.contains("definitely-not-a-flag"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processes_run_with_streams_status_and_env() {
    let server = start().await;
    let client = server.connect().await.unwrap();

    let output = client
        .spawn(Command::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(output.stdout, b"out\n");
    assert_eq!(output.stderr, b"err\n");
    assert_eq!(output.status.code(), Some(3));
    assert!(!output.status.success());

    let merged = client
        .spawn(
            Command::new("sh")
                .args(["-c", "echo one; echo two >&2"])
                .merge_stderr(true),
        )
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(merged.stdout, b"one\ntwo\n");
    assert!(merged.stderr.is_empty());

    let env = client
        .spawn(
            Command::new("sh")
                .args([
                    "-c",
                    "printf '%s %s %s' \"$A\" \"$YAS_CLIENT_TEST_VARIABLE\" \"$PWD\"",
                ])
                .env("A", "b")
                .current_dir("/"),
        )
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&env.stdout), "b forty-two /");

    let mut cat = client
        .spawn(Command::new("cat").stdin(Stdin::Piped))
        .await
        .unwrap();
    let mut stdin = cat.take_stdin().unwrap();
    stdin.write_all(b"hello through stdin").await.unwrap();
    stdin.finish().await.unwrap();
    let output = cat.output().await.unwrap();
    assert_eq!(output.stdout, b"hello through stdin");

    let sleeper = client.spawn(Command::new("sleep").arg("30")).await.unwrap();
    assert_eq!(
        sleeper
            .wait_timeout(Duration::from_millis(100))
            .await
            .unwrap(),
        None
    );
    assert!(
        client
            .processes()
            .await
            .unwrap()
            .iter()
            .any(|info| info.handle == sleeper.handle())
    );
    sleeper.signal(Signal::Terminate).await.unwrap();
    let status = sleeper.wait().await.unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{status}");

    let missing = client
        .spawn(&Command::new("/definitely/not/a/program"))
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(missing, Error::Status { .. }), "{missing:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processes_run_concurrently_on_one_session() {
    let server = start().await;
    let client = server.connect().await.unwrap();
    let started = Instant::now();
    let runs = (0..8).map(|index| {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .spawn(Command::new("sh").args(["-c", &format!("sleep 1; echo {index}")]))
                .await
                .unwrap()
                .output()
                .await
                .unwrap()
        })
    });
    for (index, run) in runs.collect::<Vec<_>>().into_iter().enumerate() {
        let output = run.await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{index}\n")
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "8 one-second commands ran concurrently: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_configured_for_more_processes_runs_them_on_one_session() {
    const PROCESSES: usize = 200;
    let server = tokio::time::timeout(
        TIMEOUT,
        HostedServer::start(
            options()
                .args(["--process-max-per-session", "256"])
                .args(["--process-max-pending-spawns", "64"])
                .args(["--process-max-waits", "512"])
                .env("YAS_PROCESS_MAX", "1024")
                .env("YAS_PROCESS_MAX_ENV", "1024"),
        ),
    )
    .await
    .expect("hosted server start timed out")
    .expect("hosted server starts");
    let client = server.connect().await.unwrap();
    let limits = client.process_limits().expect("Process limits");
    assert_eq!(limits.max_processes_per_session, 256);
    assert_eq!(limits.max_processes, 1024);
    assert_eq!(limits.max_pending_spawns, 64);
    assert_eq!(limits.max_pending_waits, 512);
    assert_eq!(limits.max_envc, 1024);
    // 12 MiB of the 16 MiB receive budget over 256 processes' two streams.
    assert_eq!(client.default_process_window(), 24 * 1024);

    // All of them alive at once: each blocks until its stdin closes.
    let mut processes = Vec::new();
    for index in 0..PROCESSES {
        let process = client
            .spawn(
                Command::new("sh")
                    .args([
                        "-c",
                        &format!("read _; echo {index}; head -c 65536 /dev/zero"),
                    ])
                    .stdin(Stdin::Piped),
            )
            .await
            .unwrap_or_else(|error| panic!("spawn {index}: {error}\n{}", server_log(&server)));
        processes.push(process);
    }
    assert!(client.processes().await.unwrap().len() >= PROCESSES);
    let runs = processes
        .into_iter()
        .enumerate()
        .map(|(index, mut process)| {
            tokio::spawn(async move {
                let mut stdin = process.take_stdin().unwrap();
                stdin.write_all(b"go\n").await.unwrap();
                stdin.finish().await.unwrap();
                let output = process.output().await.unwrap();
                assert!(output.status.success(), "{index}: {:?}", output.status);
                let expected = format!("{index}\n");
                assert!(output.stdout.starts_with(expected.as_bytes()), "{index}");
                assert_eq!(output.stdout.len(), expected.len() + 65536, "{index}");
            })
        });
    for run in runs.collect::<Vec<_>>() {
        tokio::time::timeout(TIMEOUT, run).await.unwrap().unwrap();
    }

    // More environment entries than the original 256.
    let mut command = Command::new("sh");
    command.args(["-c", "echo $V999"]);
    for index in 0..1000 {
        command.env(format!("V{index}"), index.to_string());
    }
    let output = client
        .spawn(&command)
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(output.stdout, b"999\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_default_server_refuses_a_seventeenth_process_and_extra_environment() {
    let server = start().await;
    let client = server.connect().await.unwrap();
    let limits = client.process_limits().expect("Process limits");
    assert_eq!(
        limits,
        yas_client::wire::process::Limits {
            max_mutation_replays: limits.max_mutation_replays,
            launcher_flags: limits.launcher_flags,
            ..yas_client::wire::process::Limits::DEFAULT
        }
    );
    let mut processes = Vec::new();
    for _ in 0..16 {
        processes.push(client.spawn(Command::new("sleep").arg("30")).await.unwrap());
    }
    let error = client
        .spawn(Command::new("sleep").arg("30"))
        .await
        .unwrap_err();
    assert_eq!(
        error.status(),
        Some(yas_client::wire::core::Status::ResourceExhausted),
        "{error}\n{}",
        server_log(&server)
    );
    for process in &processes {
        process.kill().await.unwrap();
    }
    let mut command = Command::new("true");
    for index in 0..300 {
        command.env(format!("V{index}"), "x");
    }
    let error = client.spawn(&command).await.unwrap_err();
    assert!(matches!(error, Error::Invalid(_)), "{error}");
}

/// Waits at most [`TIMEOUT`] for `future`, naming what it waited for.
async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("{what} took over {TIMEOUT:?}"))
}

/// The session still runs commands: the Process family was not taken down.
async fn still_runs_commands(client: &Client) {
    let output = within(
        "a command after it",
        client
            .spawn(Command::new("echo").arg("after"))
            .await
            .unwrap()
            .output(),
    )
    .await
    .unwrap();
    assert_eq!(output.stdout, b"after\n");
    assert!(output.status.success(), "{}", output.status);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_writing_faster_than_its_output_is_read_loses_none_of_it() {
    use tokio::io::AsyncReadExt;
    let server = start().await;
    let client = server.connect().await.unwrap();
    // Three times the largest stream buffer a default server keeps, written as fast as the
    // pipe takes it: the server reads it no faster than this client does (it used to drop the
    // stream NOT_FOUND about a window in, and the exit never came).
    const BYTES: u64 = 3 * 8 * 1024 * 1024;
    let mut process = client
        .spawn(Command::new("sh").args([
            "-c".to_owned(),
            format!("head -c {BYTES} /dev/zero; printf end; echo tail >&2; exit 9"),
        ]))
        .await
        .unwrap();
    let stdout = process.take_stdout().unwrap();
    let stderr = process.take_stderr().unwrap();
    // Read as Ultimator's bash does: the exit is awaited while the output streams in.
    let reader = tokio::spawn(async move {
        tokio::join!(stdout.read_to_end(BYTES + 1024), stderr.read_to_end(1024))
    });
    let status = within("the exit", process.wait()).await.unwrap();
    assert_eq!(status.code(), Some(9), "{status}\n{}", server_log(&server));
    let (stdout, stderr) = within("the output", reader).await.unwrap();
    let stdout = stdout.unwrap_or_else(|error| panic!("{error}\n{}", server_log(&server)));
    assert_eq!(stdout.len() as u64, BYTES + 3);
    assert!(stdout.ends_with(b"end"));
    assert!(stdout[..BYTES as usize].iter().all(|byte| *byte == 0));
    assert_eq!(stderr.unwrap(), b"tail\n");

    // A reader slower than the writer, a few KiB at a time: the writer waits for it.
    const SLOW: usize = 4 * 1024 * 1024;
    let mut process = client
        .spawn(
            Command::new("head")
                .args(["-c".to_owned(), SLOW.to_string()])
                .arg("/dev/zero"),
        )
        .await
        .unwrap();
    let mut stdout = process.take_stdout().unwrap();
    let mut total = 0;
    let mut buffer = vec![0u8; 64 * 1024];
    within("the slow reads", async {
        loop {
            let n = stdout.read(&mut buffer).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert_eq!(total, SLOW);
    assert!(within("the exit", process.wait()).await.unwrap().success());

    still_runs_commands(&client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_output_stream_leaves_its_process_and_session_working() {
    use tokio::io::AsyncReadExt;
    let server = start().await;
    let client = server.connect().await.unwrap();
    let mut process = client
        .spawn(Command::new("sh").args(["-c", "head -c 25165824 /dev/zero; exit 4"]))
        .await
        .unwrap();
    let mut stdout = process.take_stdout().unwrap();
    let mut some = vec![0u8; 1024 * 1024];
    within("the first MiB", stdout.read_exact(&mut some))
        .await
        .unwrap();
    // Dropped half-way: what the command writes next goes nowhere, and it still finishes.
    drop(stdout);
    let status = within("the exit", process.wait()).await.unwrap();
    assert_eq!(status.code(), Some(4), "{status}\n{}", server_log(&server));
    still_runs_commands(&client).await;
}

/// A spawned process whose attachment goes before its exit (it was detached, or its streams
/// were dropped) is asked for its exit with WAIT, which its attachment would have reported.
/// Nothing holds it to the session meanwhile: a detached one gives back its slot, and either
/// can be attached to again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_whose_attachment_went_is_waited_for_and_held_by_nothing() {
    use yas_client::wire::schema::process as schema;
    let server = start().await;
    let client = server.connect().await.unwrap();
    assert_ne!(
        client.launcher_flags() & schema::SPAWN_REPORT_EXIT as u32,
        0
    );
    let per_session = client.process_limits().unwrap().max_processes_per_session as usize;
    // Detached, each gives back its slot: one more than the session holds still spawns.
    let mut detached = Vec::new();
    for _ in 0..per_session {
        let process = client
            .spawn(Command::new("sleep").arg("30").detachable(true))
            .await
            .unwrap();
        within("a detach", process.detach()).await.unwrap();
        detached.push(process);
    }
    let one_more = within(
        "a spawn beside the detached",
        client.spawn(Command::new("sh").args(["-c", "exit 6"])),
    )
    .await
    .unwrap();
    assert_eq!(
        within("its exit", one_more.wait()).await.unwrap().code(),
        Some(6)
    );
    // A detached one can be attached to again, and its exit comes (by WAIT).
    let again = within("an attach", client.attach(detached[0].handle(), false))
        .await
        .unwrap();
    detached[0].kill().await.unwrap();
    let status = within("a detached one's exit", detached[0].wait())
        .await
        .unwrap();
    // (Each answer stamps the time it is given.)
    let attached = within("its exit, attached", again.wait()).await.unwrap();
    assert_eq!(
        (&status.kind, &status.reason, status.raw_code),
        (&attached.kind, &attached.reason, attached.raw_code)
    );
    for process in &detached[1..] {
        process.kill().await.unwrap();
        within("a detached one's exit", process.wait())
            .await
            .unwrap();
    }
    // An ordinary one whose streams were dropped: the same.
    let mut sleeper = client.spawn(Command::new("sleep").arg("30")).await.unwrap();
    drop(sleeper.take_stdout());
    drop(sleeper.take_stderr());
    let again = within("an attach", client.attach(sleeper.handle(), false))
        .await
        .unwrap_or_else(|error| panic!("{error}\n{}", server_log(&server)));
    assert_eq!(
        within(
            "a short wait",
            sleeper.wait_timeout(Duration::from_millis(200))
        )
        .await
        .unwrap(),
        None
    );
    sleeper.kill().await.unwrap();
    let status = within("its exit", sleeper.wait()).await.unwrap();
    // (Each answer stamps the time it is given.)
    let attached = within("its exit, attached", again.wait()).await.unwrap();
    assert_eq!(
        (&status.kind, &status.reason, status.raw_code),
        (&attached.kind, &attached.reason, attached.raw_code)
    );
    // One whose stdin was aborted: its attachment goes with it.
    let mut reader = client
        .spawn(
            Command::new("sh")
                .args(["-c", "sleep 0.3; exit 3"])
                .stdin(Stdin::Piped),
        )
        .await
        .unwrap();
    reader.take_stdin().unwrap().abort();
    assert_eq!(
        within("its exit, stdin aborted", reader.wait())
            .await
            .unwrap()
            .code(),
        Some(3)
    );
    still_runs_commands(&client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watcher_that_falls_behind_is_dropped_alone() {
    let server = start().await;
    let owner = server.connect().await.unwrap();
    let watcher = server.connect().await.unwrap();
    const BYTES: u64 = 3 * 8 * 1024 * 1024;
    let mut process = owner
        .spawn(Command::new("sh").args([
            "-c".to_owned(),
            format!("sleep 1; head -c {BYTES} /dev/zero; exit 3"),
        ]))
        .await
        .unwrap();
    // Another session watches, and reads nothing until the command is done.
    let mut watched = within("the attach", watcher.attach(process.handle(), false))
        .await
        .unwrap();
    let watched_stdout = watched.take_stdout().unwrap();
    let stdout = process.take_stdout().unwrap();
    let stdout = within("the owner's output", stdout.read_to_end(BYTES + 1024))
        .await
        .unwrap_or_else(|error| panic!("{error}\n{}", server_log(&server)));
    assert_eq!(stdout.len() as u64, BYTES);
    assert_eq!(
        within("the exit", process.wait()).await.unwrap().code(),
        Some(3)
    );
    // The watcher fell a window behind: its stream was dropped, and says why.
    let dropped = within(
        "the watcher's output",
        watched_stdout.read_to_end(BYTES + 1024),
    )
    .await
    .unwrap_err();
    assert_eq!(
        dropped.status(),
        Some(yas_client::wire::core::Status::ResourceExhausted),
        "{dropped}"
    );
    // Both sessions still run commands (the watcher's used to lose its Process family).
    still_runs_commands(&watcher).await;
    still_runs_commands(&owner).await;
}

/// A server started with `--process-max-waits 1`, with that one WAIT held on a
/// `sleep` and shown to be held: a second WAIT is refused.
async fn hold_the_only_wait(
    client: &Client,
) -> (yas_client::process::Process, tokio::task::JoinHandle<()>) {
    let sleeper = client.spawn(Command::new("sleep").arg("30")).await.unwrap();
    let handle = sleeper.handle();
    let holder = {
        let client = client.clone();
        tokio::spawn(async move {
            let _ = client.wait_process(handle, None).await;
        })
    };
    // The holder's WAIT goes first on the one connection.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let refused = client
        .wait_process(handle, Some(Duration::from_millis(1)))
        .await
        .unwrap_err();
    assert_eq!(
        refused.status(),
        Some(yas_client::wire::core::Status::ResourceExhausted),
        "{refused}"
    );
    (sleeper, holder)
}

/// Commands whose exits must come whole: a non-zero exit with stderr, output
/// beyond the stream buffer read while the exit is awaited, a signal.
async fn exits_come_with_all_their_output(server: &HostedServer, client: &Client) {
    let output = client
        .spawn(Command::new("sh").args(["-c", "echo out; echo 'went wrong' >&2; exit 42"]))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(42), "{}", output.status);
    assert_eq!(output.stdout, b"out\n");
    assert_eq!(output.stderr, b"went wrong\n");

    // Three times the largest stream buffer a default server keeps: the
    // command blocks on its pipe until this reads, and its exit arrives while
    // the tail is still on its way, as Ultimator's bash reads it.
    const BYTES: u64 = 3 * 8 * 1024 * 1024;
    let mut process = client
        .spawn(Command::new("sh").args([
            "-c".to_owned(),
            format!("head -c {BYTES} /dev/zero; printf end; echo tail >&2; exit 9"),
        ]))
        .await
        .unwrap();
    let stdout = process.take_stdout().unwrap();
    let stderr = process.take_stderr().unwrap();
    let reader = tokio::spawn(async move {
        (
            stdout.read_to_end(BYTES + 1024).await.unwrap(),
            stderr.read_to_end(1024).await,
        )
    });
    let status = tokio::time::timeout(TIMEOUT, process.wait())
        .await
        .unwrap_or_else(|_| panic!("no exit\n{}", server_log(server)))
        .unwrap();
    assert_eq!(status.code(), Some(9), "{status}");
    let (stdout, stderr) = tokio::time::timeout(TIMEOUT, reader)
        .await
        .expect("output after the exit")
        .unwrap();
    let stderr = stderr.unwrap_or_else(|error| panic!("{error}\n{}", server_log(server)));
    assert_eq!(stdout.len() as u64, BYTES + 3);
    assert!(stdout.ends_with(b"end"));
    assert!(stdout[..BYTES as usize].iter().all(|byte| *byte == 0));
    assert_eq!(stderr, b"tail\n");

    let sleeper = client.spawn(Command::new("sleep").arg("30")).await.unwrap();
    assert_eq!(
        sleeper
            .wait_timeout(Duration::from_millis(100))
            .await
            .unwrap(),
        None
    );
    sleeper.signal(Signal::Terminate).await.unwrap();
    let status = sleeper.wait().await.unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{status}");
    // Asked again, the same exit (an older server's WAITs may each give it another time).
    let same = |other: &yas_client::process::ExitStatus| {
        (&other.kind, &other.reason, other.raw_code)
            == (&status.kind, &status.reason, status.raw_code)
    };
    let again = sleeper.wait().await.unwrap();
    assert!(same(&again), "{again:?} after {status:?}");
    let again = sleeper
        .wait_timeout(Duration::from_millis(1))
        .await
        .unwrap()
        .expect("exited");
    assert!(same(&again), "{again:?} after {status:?}");

    for _ in 0..20 {
        let output = client
            .spawn(&Command::new("true"))
            .await
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spawned_process_reports_its_exit_without_a_wait() {
    use yas_client::wire::schema::process as schema;
    let server = tokio::time::timeout(
        TIMEOUT,
        HostedServer::start(
            options()
                .arg("--verbose")
                .args(["--process-max-waits", "1"]),
        ),
    )
    .await
    .expect("hosted server start timed out")
    .expect("hosted server starts");
    let client = server.connect().await.unwrap();
    assert_ne!(
        client.launcher_flags() & schema::SPAWN_REPORT_EXIT as u32,
        0
    );
    // With the only WAIT held, every exit below comes unasked.
    let (sleeper, holder) = hold_the_only_wait(&client).await;
    exits_come_with_all_their_output(&server, &client).await;
    // A process attached to (not spawned) is still waited for: another session, its only
    // WAIT held too, attaches to this one's sleeper and is refused a second WAIT.
    let other = server.connect().await.unwrap();
    let (other_sleeper, other_holder) = hold_the_only_wait(&other).await;
    let attached = other.attach(sleeper.handle(), false).await.unwrap();
    let refused = attached
        .wait_timeout(Duration::from_millis(1))
        .await
        .unwrap_err();
    assert_eq!(
        refused.status(),
        Some(yas_client::wire::core::Status::ResourceExhausted),
        "{refused}"
    );
    for (which, (sleeper, holder)) in [(sleeper, holder), (other_sleeper, other_holder)]
        .into_iter()
        .enumerate()
    {
        sleeper.kill().await.unwrap();
        // Killed by YAS at this client's request, as a WAIT would answer it.
        let status = sleeper.wait().await.unwrap();
        assert_eq!(
            (&status.kind, &status.reason),
            (
                &yas_client::process::ExitKind::Killed,
                &yas_client::process::ExitReason::Client
            ),
            "sleeper {which}: {status:?}"
        );
        tokio::time::timeout(TIMEOUT, holder)
            .await
            .unwrap()
            .unwrap();
    }
}

/// A process spawned with `keep_output`: its streams' heads and tails, and what was dropped.
async fn kept(client: &Client, command: &Command) -> yas_client::process::Output {
    let process = client.spawn(command).await.unwrap();
    within("the kept output", process.output_limited(4 << 20))
        .await
        .unwrap()
}

/// What a WHATWG UTF-8 decoder makes of `text`, as an elision counts it.
fn elision_of(offset: usize, text: &str) -> yas_client::process::OutputElision {
    yas_client::process::OutputElision {
        offset: offset as u64,
        bytes: text.len() as u64,
        lines: text.matches('\n').count() as u64,
        code_points: text.chars().count() as u64,
        utf16_units: text.encode_utf16().count() as u64,
    }
}

/// With `keep_output`, each stream is sent as its head and its tail, cut between characters,
/// and the exit says exactly what was dropped between them: far more output than the session
/// takes runs at the speed of its pipe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kept_output_is_its_head_and_tail_and_the_exit_counts_the_rest() {
    use yas_client::wire::schema::process as schema;
    let server = start().await;
    let client = server.connect().await.unwrap();
    assert_ne!(
        client.launcher_flags() & schema::SPAWN_KEEP_OUTPUT as u32,
        0
    );

    // 64 MiB of `yes` on stdout, 1 MiB on stderr: 1,000 + 2,000 bytes of each come.
    const BYTES: usize = 64 << 20;
    let yes = kept(
        &client,
        Command::new("sh")
            .args([
                "-c".to_owned(),
                format!("yes | head -c {BYTES}; yes e | head -c 1048576 >&2; exit 4"),
            ])
            .keep_output(1_000, 2_000),
    )
    .await;
    assert_eq!(yes.status.code(), Some(4), "{}", server_log(&server));
    assert_eq!(yes.stdout, "y\n".repeat(1_500).into_bytes());
    assert_eq!(yes.stderr, "e\n".repeat(1_500).into_bytes());
    assert_eq!(
        yes.elided,
        [
            Some(elision_of(1_000, &"y\n".repeat((BYTES - 3_000) / 2))),
            Some(elision_of(1_000, &"e\n".repeat((1_048_576 - 3_000) / 2))),
        ]
    );
    // Mixed widths: the cuts fall inside emoji and CJK, and move to their ends.
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("mixed");
    let middle = "中文 🙂 ascii\n".repeat(700);
    let whole = format!("ab🙂中{middle}🙂中cd");
    std::fs::write(&file, &whole).unwrap();
    // 3 bytes of head end within 🙂 (2 + 4), 8 of tail start within 🙂 (…🙂 中 c d).
    let (head, tail) = (3, 8);
    let head_len = 6;
    let tail_start = whole.len() - "中cd".len();
    assert!(whole.is_char_boundary(head_len) && whole.is_char_boundary(tail_start));
    let mixed = kept(
        &client,
        Command::new("sh")
            .args([
                "-c".to_owned(),
                format!("cat '{0}'; cat '{0}' >&2", file.display()),
            ])
            .keep_output(head, tail),
    )
    .await;
    assert!(mixed.status.success(), "{}", mixed.status);
    let expected = [&whole[..head_len], &whole[tail_start..]]
        .concat()
        .into_bytes();
    assert_eq!(
        String::from_utf8_lossy(&mixed.stdout),
        String::from_utf8_lossy(&expected)
    );
    assert_eq!(mixed.stderr, expected);
    let dropped = Some(elision_of(head_len, &whole[head_len..tail_start]));
    assert_eq!(mixed.elided, [dropped, dropped]);

    // What fits is all sent, and nothing said dropped; merged stderr is stdout's.
    let fits = kept(
        &client,
        Command::new("sh")
            .args(["-c", "printf 'out 🙂'; printf ' err' >&2"])
            .merge_stderr(true)
            .keep_output(100, 100),
    )
    .await;
    assert_eq!(fits.stdout, "out 🙂 err".as_bytes());
    assert_eq!(fits.elided, [None, None]);
    let merged = kept(
        &client,
        Command::new("sh")
            .args(["-c", "yes o | head -c 100000; yes e | head -c 100000 >&2"])
            .merge_stderr(true)
            .keep_output(10, 10),
    )
    .await;
    assert_eq!(merged.stdout, b"o\no\no\no\no\ne\ne\ne\ne\ne\n");
    assert_eq!(merged.elided[0].unwrap().bytes, 200_000 - 20);
    assert_eq!(merged.elided[1], None);

    // A residue holding the streams past the grace: the tail kept so far goes out before
    // the exit, which counts what was dropped until then.
    let residue = kept(
        &client,
        Command::new("sh")
            .args(["-c", "yes | head -c 3000000; (sleep 2; echo late) &"])
            .leave_residue(Some(Duration::from_millis(300)))
            .keep_output(10, 10),
    )
    .await;
    assert_eq!(residue.status.detail, "residual process group left running");
    assert_eq!(residue.stdout, "y\n".repeat(10).into_bytes());
    assert_eq!(
        residue.elided[0],
        Some(elision_of(10, &"y\n".repeat((3_000_000 - 20) / 2)))
    );
    still_runs_commands(&client).await;
}

/// Against a server from before SPAWN_REPORT_EXIT, the same commands are
/// waited for with WAIT. Run with
/// `YAS_OLD_SERVER=/path/to/yas cargo test -p yas-cli --test client_host -- --ignored`, with a
/// binary that paces process output (its long output must not reset the stream).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs YAS_OLD_SERVER: a yas binary from before SPAWN_REPORT_EXIT"]
async fn a_server_from_before_report_exit_is_waited_for() {
    use yas_client::wire::schema::process as schema;
    let binary = std::env::var_os("YAS_OLD_SERVER").expect("YAS_OLD_SERVER names a yas binary");
    let server = tokio::time::timeout(
        TIMEOUT,
        HostedServer::start(options_for(binary).args(["--process-max-waits", "1"])),
    )
    .await
    .expect("hosted server start timed out")
    .expect("hosted server starts");
    let client = server.connect().await.unwrap();
    assert_eq!(
        client.launcher_flags() & schema::SPAWN_REPORT_EXIT as u32,
        0
    );
    // Its exits take a WAIT: with the only one held, waiting is refused.
    let (sleeper, holder) = hold_the_only_wait(&client).await;
    let process = client
        .spawn(Command::new("sh").args(["-c", "exit 5"]))
        .await
        .unwrap();
    let refused = process.wait().await.unwrap_err();
    assert_eq!(
        refused.status(),
        Some(yas_client::wire::core::Status::ResourceExhausted),
        "{refused}"
    );
    sleeper.kill().await.unwrap();
    tokio::time::timeout(TIMEOUT, holder)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(process.wait().await.unwrap().code(), Some(5));
    exits_come_with_all_their_output(&server, &client).await;
}

/// Every stdout/stderr stream holds its window of the session's receive budget
/// while it is open, writing or not. A client that asks for a wider budget
/// gets windows that still all fit: at 256 processes a session in 256 MiB,
/// 384 KiB each, where 16 MiB gives 24 KiB. With 255 quiet processes holding
/// theirs (510 streams, 191 MiB: more than the default budget), one more still
/// gets credit, and all of its output at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wider_receive_budget_fits_every_process_window() {
    const BUDGET: u64 = 256 << 20;
    const QUIET: usize = 255;
    let server = within(
        "hosted server start",
        HostedServer::start(
            options()
                .args(["--process-max-per-session", "256"])
                .args(["--process-max", "1024"])
                .hello(HelloOptions::named("yas-test").receive_budget(BUDGET)),
        ),
    )
    .await
    .expect("hosted server starts");
    let narrow = server
        .connect_with(&HelloOptions::named("yas-test"))
        .await
        .unwrap();
    assert_eq!(narrow.default_process_window(), 24 * 1024);
    drop(narrow);
    let client = server.connect().await.unwrap();
    assert_eq!(client.receive_budget(), BUDGET);
    assert_eq!(client.default_process_window(), 384 * 1024);
    let mut quiet = Vec::with_capacity(QUIET);
    for _ in 0..QUIET {
        quiet.push(client.spawn(Command::new("sleep").arg("60")).await.unwrap());
    }
    // Three windows of output: it comes whole only if credit keeps coming.
    const BYTES: usize = 3 * 384 * 1024;
    let started = Instant::now();
    let output = within(
        "a command beside the quiet ones",
        client
            .spawn(Command::new("sh").args(["-c", &format!("head -c {BYTES} /dev/zero; echo end")]))
            .await
            .unwrap()
            .output(),
    )
    .await
    .unwrap();
    let took = started.elapsed();
    assert!(output.status.success(), "{}", output.status);
    assert_eq!(output.stdout.len(), BYTES + 4);
    assert!(output.stdout.ends_with(b"end\n"));
    assert!(took < Duration::from_secs(5), "{took:?}");
    for process in &quiet {
        process.kill().await.unwrap();
    }
    for process in &quiet {
        within("a quiet process's exit", process.wait())
            .await
            .unwrap();
    }
    drop(quiet);
    still_runs_commands(&client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commands_background_children_die_with_it() {
    let server = start().await;
    let client = server.connect().await.unwrap();
    // `sleep` joins the command's process group; the server cleans the group
    // up once the command itself exits.
    let output = client
        .spawn(Command::new("sh").args(["-c", "sleep 60 >/dev/null 2>&1 & echo $!"]))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let pid: i32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "background child {pid} survived");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_read_write_list_and_preconditions() {
    let server = start().await;
    let client = server.connect().await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let root = client.open_root(directory.path(), true).await.unwrap();

    let written = root
        .write(
            "a/b/file.txt",
            b"0123456789",
            &WriteOptions::new().create_parents(true),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(directory.path().join("a/b/file.txt")).unwrap(),
        b"0123456789"
    );
    let content = root.read("a/b/file.txt").await.unwrap();
    assert_eq!(content.bytes, b"0123456789");
    assert_eq!(content.hash, written.hash);
    assert!(!content.truncated);

    let limited = root.read_limited("a/b/file.txt", 4).await.unwrap();
    assert_eq!(limited.bytes, b"0123");
    assert!(limited.truncated);
    assert_eq!(limited.len, 10);
    let range = root.read_range("a/b/file.txt", 3, 4).await.unwrap();
    assert_eq!(range.bytes, b"3456");

    // Preconditions: absent fails on an existing file, a stale hash fails,
    // the current hash succeeds.
    let conflict = root
        .write(
            "a/b/file.txt",
            b"x",
            &WriteOptions::new().precondition(Precondition::Absent),
        )
        .await
        .unwrap_err();
    assert!(conflict.is_conflict(), "{conflict:?}");
    let stale = root
        .write(
            "a/b/file.txt",
            b"x",
            &WriteOptions::new().precondition(Precondition::Hash([7; 32])),
        )
        .await
        .unwrap_err();
    assert!(stale.is_conflict(), "{stale:?}");
    root.write(
        "a/b/file.txt",
        b"replaced",
        &WriteOptions::new().precondition(Precondition::Hash(written.hash)),
    )
    .await
    .unwrap();
    assert_eq!(root.read("a/b/file.txt").await.unwrap().bytes, b"replaced");

    // Large files go through a staged transfer.
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    root.write("big.bin", &big, &WriteOptions::new())
        .await
        .unwrap();
    assert_eq!(root.read("big.bin").await.unwrap().bytes, big);

    root.mkdir("x/y/z", true).await.unwrap();
    root.symlink("link", "a/b/file.txt").await.unwrap();
    let mut names: Vec<(String, bool)> = root
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|entry| (entry.name(), entry.is_dir()))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ("a".to_string(), true),
            ("big.bin".to_string(), false),
            ("link".to_string(), false),
            ("x".to_string(), true),
        ]
    );
    let link = root.stat("link").await.unwrap().unwrap();
    assert!(
        matches!(&link.kind, EntryKind::Symlink { target, .. } if target == b"a/b/file.txt"),
        "{link:?}"
    );
    assert_eq!(root.read_link("link").await.unwrap(), b"a/b/file.txt");
    let file = root.stat("a/b/file.txt").await.unwrap().unwrap();
    assert!(
        matches!(file.kind, EntryKind::File { len: 8, .. }),
        "{file:?}"
    );
    assert!(root.stat("nope").await.unwrap().is_none());

    root.rename("a/b/file.txt", "moved/file.txt", true)
        .await
        .unwrap();
    assert!(directory.path().join("moved/file.txt").exists());
    root.remove("x", true, Precondition::Any).await.unwrap();
    assert!(!directory.path().join("x").exists());
    let missing = root.read("x/y").await.unwrap_err();
    assert!(missing.is_not_found(), "{missing:?}");
    root.close().await.unwrap();

    let read_only = client.open_root(directory.path(), false).await.unwrap();
    assert!(
        read_only
            .write("nope.txt", b"x", &WriteOptions::new())
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roots_say_how_paths_are_spelled_and_the_staging_root_lasts_the_session() {
    use std::os::unix::ffi::OsStrExt;

    let server = start().await;
    let client = server.connect().await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let root = client.open_root(directory.path(), false).await.unwrap();
    assert_eq!(root.path_model(), PathModel::PosixBytes);
    assert_eq!(root.case_behavior(), CaseBehavior::Sensitive);
    root.close().await.unwrap();

    let staging = client.open_staging_root(true).await.unwrap();
    assert_eq!(staging.path_model(), PathModel::PosixBytes);
    let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(staging.canonical_path()));
    assert!(path.is_dir(), "{path:?}");
    staging
        .write("dropped.txt", b"dropped", &WriteOptions::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read(path.join("dropped.txt")).unwrap(), b"dropped");
    staging.close().await.unwrap();
    assert!(
        path.is_dir(),
        "closing the root keeps the staging directory"
    );
    let again = client.open_staging_root(false).await.unwrap();
    assert_eq!(again.read("dropped.txt").await.unwrap().bytes, b"dropped");
    drop(again);
    drop(client);
    let deadline = Instant::now() + TIMEOUT;
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "the session ended and {path:?} is still there"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kv_and_environment() {
    let server = start().await;
    let client = server.connect().await.unwrap();

    assert_eq!(
        client.env_var("YAS_CLIENT_TEST_VARIABLE").await.unwrap(),
        Some(b"forty-two".to_vec())
    );
    assert!(
        client
            .env_var("YAS_CLIENT_TEST_UNSET")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        client
            .environment()
            .await
            .unwrap()
            .iter()
            .any(|(key, _)| key == b"YAS_CLIENT_TEST_VARIABLE")
    );

    let kv = client.kv(b"test/").await.unwrap();
    let mut watch = kv.watch(true).await.unwrap();
    assert!(
        matches!(watch.next().await.unwrap(), KvChange::Snapshot(entries) if entries.is_empty())
    );

    assert!(kv.get(b"k").await.unwrap().is_none());
    let put = kv
        .put(b"k", b"v1", KvPrecondition::Absent, false)
        .await
        .unwrap();
    assert!(put.applied());
    let again = kv
        .put(b"k", b"v2", KvPrecondition::Absent, false)
        .await
        .unwrap();
    assert!(
        !again.applied(),
        "absent precondition fails on an existing key"
    );
    let value = kv.get(b"k").await.unwrap().unwrap();
    assert_eq!(value.value, b"v1");

    match tokio::time::timeout(TIMEOUT, watch.next())
        .await
        .unwrap()
        .unwrap()
    {
        KvChange::Put(entry) => {
            assert_eq!(entry.key, b"k");
            assert_eq!(entry.value.as_deref(), Some(&b"v1"[..]));
        }
        other => panic!("expected Put, got {other:?}"),
    }

    let listed = kv.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(
        kv.delete(b"k", KvPrecondition::Any, false)
            .await
            .unwrap()
            .applied()
    );
    assert!(kv.get(b"k").await.unwrap().is_none());
    match tokio::time::timeout(TIMEOUT, watch.next())
        .await
        .unwrap()
        .unwrap()
    {
        KvChange::Deleted { key, .. } => assert_eq!(key, b"k"),
        other => panic!("expected Deleted, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_sessions_cannot_spawn_or_open_writable_roots() {
    let server = start().await;
    let client: Client = server
        .connect_with(&HelloOptions::named("read-only-test").read_only(true))
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let spawn = client
        .spawn(&Command::new("true"))
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(spawn, Error::Unsupported(_)), "{spawn:?}");
    assert!(client.open_root(directory.path(), true).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_process_catalogue_can_be_watched() {
    use yas_client::process::{ProcessChange, ProcessWatch};
    async fn next(watch: &mut ProcessWatch) -> ProcessChange {
        tokio::time::timeout(TIMEOUT, watch.next())
            .await
            .expect("a change of the process catalogue")
            .unwrap()
    }
    let server = start().await;
    let client = server.connect().await.unwrap();
    let mut watch = client.watch_processes().await.unwrap();
    assert!(matches!(next(&mut watch).await, ProcessChange::Snapshot(list) if list.is_empty()));
    // The watch sends what changed between two looks at the catalogue, which an
    // ordinary child leaves as soon as its exit is delivered: one that came and
    // went between two looks never shows. This one waits on its stdin until the
    // watch has shown it running.
    let mut process = client
        .spawn(Command::new("cat").stdin(Stdin::Piped))
        .await
        .unwrap();
    let handle = process.handle();
    let stdin = process.take_stdin().unwrap();
    loop {
        match next(&mut watch).await {
            ProcessChange::Updated(info) if info.handle == handle => {
                assert!(info.exit.is_none(), "{info:?}");
                break;
            }
            other => assert!(
                !matches!(other, ProcessChange::Removed(removed) if removed == handle),
                "{other:?}"
            ),
        }
    }
    stdin.finish().await.unwrap();
    assert!(process.wait().await.unwrap().success());
    loop {
        match next(&mut watch).await {
            ProcessChange::Updated(info) if info.handle == handle && info.exit.is_some() => break,
            ProcessChange::Removed(removed) if removed == handle => break,
            _ => {}
        }
    }
    assert!(client.process_limits().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_stdin_after_the_child_closed_it_is_clean() {
    let server = start().await;
    let client = server.connect().await.unwrap();
    // The child closes its stdin at once and outlives the client's CLOSE:
    // the server sees the input end first, then the peer's CLOSE crosses it.
    let mut process = client
        .spawn(
            Command::new("sh")
                .args(["-c", "exec 0<&-; sleep 1; echo done"])
                .stdin(Stdin::Piped),
        )
        .await
        .unwrap();
    let mut stdin = process.take_stdin().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = stdin.write_all(b"ignored").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = stdin.finish().await;
    let output = process.output().await.unwrap();
    assert_eq!(output.stdout, b"done\n");
    assert!(output.status.success(), "{}", output.status);

    // Many short-lived commands with a null stdin never trip over the race
    // between their exit and the client's stdin CLOSE.
    for _ in 0..40 {
        let output = client
            .spawn(&Command::new("true"))
            .await
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_can_leave_its_background_running_with_a_null_stdin() {
    use yas_client::wire::schema::process as schema;
    let server = start().await;
    let client = server.connect().await.unwrap();
    assert_eq!(
        client.launcher_flags(),
        (schema::SPAWN_LEAVE_RESIDUE
            | schema::SPAWN_STDIN_NULL
            | schema::SPAWN_REPORT_EXIT
            | schema::SPAWN_KEEP_OUTPUT) as u32
    );
    // The background `sleep` holds stdout: the exit comes after the grace, and
    // the sleep keeps running.
    let started = Instant::now();
    let output = client
        .spawn(
            Command::new("sh")
                .args([
                    "-c",
                    "sleep 60 & echo $!; if [ -c /dev/stdin ]; then echo character-device; else echo other; fi",
                ])
                .merge_stderr(true)
                .leave_residue(Some(Duration::from_millis(300))),
        )
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(output.status.success(), "{}", output.status);
    assert_eq!(output.status.detail, "residual process group left running");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut lines = text.lines();
    let pid: i32 = lines.next().unwrap().trim().parse().unwrap();
    let stdin = lines.next().unwrap();
    // The null device (a character device, not a pipe), on Linux and macOS alike.
    assert_eq!(stdin, "character-device");
    assert!(alive(pid), "the background survives its command");
    // SAFETY: the test's own background process.
    unsafe { libc::kill(pid, libc::SIGKILL) };

    // Without a grace, the exit waits for the streams.
    let output = client
        .spawn(
            Command::new("sh")
                .args(["-c", "(sleep 0.3; printf later) & printf now"])
                .leave_residue(None),
        )
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(output.stdout, b"nowlater");
    assert!(output.status.detail.is_empty(), "{}", output.status.detail);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_answer_as_the_os_does() {
    use yas_client::fs::{Kind, os_error};
    use yas_client::wire::schema::fs as schema;
    let server = start().await;
    let client = server.connect().await.unwrap();
    assert_eq!(client.fs_capabilities(), schema::CAPABILITY_FLAGS as u32);
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    std::fs::write(base.join("file"), b"old").unwrap();
    std::fs::create_dir(base.join("dir")).unwrap();
    std::os::unix::fs::symlink(base.join("dir"), base.join("link")).unwrap();
    std::os::unix::fs::symlink(base.join("file"), base.join("to-file")).unwrap();
    let root = client.open_root(&base, true).await.unwrap();
    let named = |error: Error| {
        let os = os_error(&error).unwrap_or_else(|| panic!("no OS error in {error:?}"));
        (os.name, os.operation)
    };
    let pair = |name: &str, operation: &str| (name.to_owned(), operation.to_owned());

    let mut entries = root.list_dir("").await.unwrap();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let kinds: Vec<(&[u8], Kind)> = entries
        .iter()
        .map(|entry| (entry.name.as_slice(), entry.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (b"dir".as_slice(), Kind::Directory),
            (b"file".as_slice(), Kind::File),
            (b"link".as_slice(), Kind::Symlink),
            (b"to-file".as_slice(), Kind::Symlink),
        ]
    );
    assert!(root.list_dir("link").await.unwrap().is_empty());
    assert_eq!(
        named(root.list_dir("file").await.unwrap_err()),
        pair("ENOTDIR", "readdir")
    );
    assert_eq!(
        named(root.list_dir("missing").await.unwrap_err()),
        pair("ENOENT", "readdir")
    );

    assert_eq!(
        root.realpath("link").await.unwrap(),
        base.join("dir").as_os_str().as_encoded_bytes()
    );
    assert_eq!(
        named(root.realpath("missing").await.unwrap_err()),
        pair("ENOENT", "realpath")
    );

    assert_eq!(
        root.stat_only("link", true).await.unwrap().kind,
        Kind::Directory
    );
    assert_eq!(
        root.stat_only("link", false).await.unwrap().kind,
        Kind::Symlink
    );
    assert_eq!(root.stat_only("file", true).await.unwrap().size, 3);
    assert_eq!(
        named(root.stat_only("missing", false).await.unwrap_err()),
        pair("ENOENT", "lstat")
    );

    // In place through a symlink: the same inode, the new bytes. Inline-sized content goes in
    // one APPLY (CAPABILITY_APPLY_IN_PLACE), bigger content is staged: both answer alike.
    use std::os::unix::fs::MetadataExt;
    let inode = std::fs::metadata(base.join("file")).unwrap().ino();
    let staged = vec![b's'; yas_client::wire::fs::MAX_INLINE_BYTES + 1];
    for content in [&b"new content"[..], &staged] {
        let written = root.write_in_place("to-file", content).await.unwrap();
        assert_eq!(written.hash, *blake3::hash(content).as_bytes());
        assert_eq!(std::fs::read(base.join("file")).unwrap(), content);
        assert_eq!(std::fs::metadata(base.join("file")).unwrap().ino(), inode);
        assert!(
            std::fs::symlink_metadata(base.join("to-file"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let error = root.write_in_place("dir", content).await.unwrap_err();
        assert!(error.is_conflict(), "{error:?}");
        assert_eq!(named(error), pair("EISDIR", "open"));
        assert_eq!(
            named(
                root.write_in_place("missing/file", content)
                    .await
                    .unwrap_err()
            ),
            pair("ENOENT", "open")
        );
        // A new file is made where it is named.
        let _ = std::fs::remove_file(base.join("fresh"));
        root.write_in_place("fresh", content).await.unwrap();
        assert_eq!(std::fs::read(base.join("fresh")).unwrap(), content);

        // Only while the file holds what was read: a stale hash is a CONFLICT without an OS
        // error, describing what the file holds and leaving it so; the current one writes.
        let other = [content, &b"!"[..]].concat();
        let stale = root
            .write_in_place_if(
                "file",
                &other,
                Precondition::Hash(*blake3::hash(b"stale").as_bytes()),
            )
            .await
            .unwrap_err();
        assert!(stale.is_conflict(), "{stale:?}");
        assert!(yas_client::fs::os_error(&stale).is_none());
        assert_eq!(
            yas_client::fs::conflict_detail(&stale).and_then(|detail| detail.current_hash),
            Some(*blake3::hash(content).as_bytes())
        );
        assert_eq!(std::fs::read(base.join("file")).unwrap(), content);
        let written = root
            .write_in_place_if(
                "file",
                &other,
                Precondition::Hash(*blake3::hash(content).as_bytes()),
            )
            .await
            .unwrap();
        assert_eq!(written.hash, *blake3::hash(&other).as_bytes());
        assert_eq!(std::fs::read(base.join("file")).unwrap(), other);
        assert_eq!(std::fs::metadata(base.join("file")).unwrap().ino(), inode);
    }

    assert_eq!(
        named(root.read("missing").await.unwrap_err()),
        pair("ENOENT", "open")
    );
    assert_eq!(
        named(root.read("dir").await.unwrap_err()),
        pair("EISDIR", "read")
    );

    root.create_dir("made", 0).await.unwrap();
    root.create_dir("made", 0).await.unwrap();
    assert!(base.join("made").is_dir());
    assert_eq!(
        named(root.create_dir("file", 0).await.unwrap_err()),
        pair("EEXIST", "mkdir")
    );
    assert_eq!(
        named(root.create_dir("file/under", 0).await.unwrap_err()),
        pair("ENOTDIR", "mkdir")
    );
    assert_eq!(
        named(root.create_dir("missing/deeper", 0).await.unwrap_err()),
        pair("ENOENT", "mkdir")
    );

    root.unlink("to-file").await.unwrap();
    assert!(
        base.join("file").exists(),
        "unlink removes the link, not its target"
    );
    assert_eq!(
        named(root.unlink("to-file").await.unwrap_err()),
        pair("ENOENT", "unlink")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_stdio_carries_a_session_over_a_childs_pipes() {
    let server = start().await;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_yas"))
        .arg("--on")
        .arg(format!("socket:{}", server.socket_path().display()))
        .args(["connect", "--stdio", "--no-start"])
        .env("YAS_PROXY", "0")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let transport = yas_client::transport::Transport::from_split(
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
    );
    let client = tokio::time::timeout(
        TIMEOUT,
        Client::from_transport(transport, &HelloOptions::named("stdio-test")),
    )
    .await
    .expect("HELLO over the pipes timed out")
    .unwrap();
    assert_eq!(client.server_name(), server.name());

    // A whole process round trip, with more output than one pipe buffer.
    let output = client
        .spawn(Command::new("sh").args(["-c", "echo over-stdio; head -c 300000 /dev/zero"]))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{:?}", output.status);
    assert!(output.stdout.starts_with(b"over-stdio\n"));
    assert_eq!(output.stdout.len(), "over-stdio\n".len() + 300_000);

    // Closing the session ends the relay cleanly, with nothing on stderr.
    client.close();
    drop(client);
    let status = tokio::time::timeout(TIMEOUT, child.wait())
        .await
        .expect("connect --stdio outlived its session")
        .unwrap();
    let mut stderr = String::new();
    tokio::io::AsyncReadExt::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr)
        .await
        .unwrap();
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.is_empty(), "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_stdio_reports_an_unreachable_server_on_stderr_only() {
    let root = tempfile::tempdir().unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_yas"))
        .arg("--on")
        .arg(format!(
            "socket:{}",
            root.path().join("nobody.sock").display()
        ))
        .args(["connect", "--stdio", "--no-start"])
        .env("YAS_PROXY", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "stdout stays clean: {output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("yas: "),
        "{output:?}"
    );
}

mod over_ssh {
    use std::sync::Arc;

    use yas_client::ssh::{HostKeyPolicy, SshMode, SshOptions, SshPool};
    use yas_client::{Client, ConnectOptions};

    use super::ssh_server::{self, TestSshServer};
    use super::{Command, TIMEOUT, start};

    /// What exec'd commands see: this build's `yas` first on PATH, a home
    /// of their own (nothing reaches the real ~/.local), and no proxy.
    fn exec_env(home: &std::path::Path) -> Vec<(String, String)> {
        let bin = std::path::Path::new(env!("CARGO_BIN_EXE_yas"))
            .parent()
            .unwrap()
            .display()
            .to_string();
        let path = std::env::var("PATH").unwrap_or_default();
        vec![
            ("PATH".into(), format!("{bin}:{path}")),
            ("HOME".into(), home.display().to_string()),
            ("YAS_PROXY".into(), "0".into()),
        ]
    }

    async fn ssh_server(streamlocal: bool, home: &std::path::Path) -> TestSshServer {
        TestSshServer::start(ssh_server::Options {
            client_key: ssh_server::key(0x33).public_key().clone(),
            streamlocal,
            env: exec_env(home),
        })
        .await
    }

    fn ssh_options(server: &TestSshServer, mode: SshMode) -> SshOptions {
        let mut ssh = SshOptions::in_memory(HostKeyPolicy::Pinned(vec![server.host_key.clone()]));
        ssh.keys.push(Arc::new(ssh_server::key(0x33)));
        ssh.port = Some(server.port);
        ssh.install = false;
        ssh.mode = mode;
        ssh
    }

    fn connect_options(ssh: SshOptions) -> ConnectOptions {
        let mut options = ConnectOptions::named("ssh-test");
        options.ssh = Some(SshPool::with_options(ssh));
        options
    }

    async fn echo(client: &Client, word: &str) -> String {
        let output = client
            .spawn(Command::new("echo").arg(word))
            .await
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{:?}", output.status);
        String::from_utf8(output.stdout).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn auto_mode_uses_the_socket_when_the_ssh_server_forwards_it() {
        let hosted = start().await;
        let home = tempfile::tempdir().unwrap();
        let server = ssh_server(true, home.path()).await;
        let options = connect_options(ssh_options(&server, SshMode::Auto));
        let target = format!("ssh:127.0.0.1:{}", hosted.socket_path().display());
        let client = tokio::time::timeout(TIMEOUT, Client::connect(Some(&target), &options))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(echo(&client, "forwarded").await, "forwarded\n");
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests.streamlocal,
            [hosted.socket_path().display().to_string()]
        );
        assert!(requests.exec.is_empty(), "{requests:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn auto_mode_runs_connect_stdio_when_forwarding_is_refused() {
        let hosted = start().await;
        let home = tempfile::tempdir().unwrap();
        let server = ssh_server(false, home.path()).await;
        let options = connect_options(ssh_options(&server, SshMode::Auto));
        let target = format!("ssh:127.0.0.1:{}", hosted.socket_path().display());
        let client = tokio::time::timeout(TIMEOUT, Client::connect(Some(&target), &options))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(client.server_name(), hosted.name());
        assert_eq!(echo(&client, "exec'd").await, "exec'd\n");

        // The pool remembers: a second session goes straight to exec.
        let second = tokio::time::timeout(TIMEOUT, Client::connect(Some(&target), &options))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(echo(&second, "again").await, "again\n");
        assert_ne!(second.session_id(), client.session_id());

        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.streamlocal.len(), 1, "{requests:?}");
        let [probe, relays @ ..] = requests.exec.as_slice() else {
            panic!("{requests:?}");
        };
        assert!(probe.contains("command -v yas"), "{probe}");
        assert_eq!(relays.len(), 2, "{requests:?}");
        for relay in relays {
            assert!(
                relay.contains("exec yas connect --stdio --no-start"),
                "{relay}"
            );
            assert!(relay.contains("YAS_SOCK="), "{relay}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exec_mode_runs_a_plain_command_and_never_forwards() {
        let hosted = start().await;
        let home = tempfile::tempdir().unwrap();
        let server = ssh_server(true, home.path()).await;
        let options = connect_options(ssh_options(&server, SshMode::Exec));
        let target = format!("ssh:127.0.0.1:{}", hosted.socket_path().display());
        let client = tokio::time::timeout(TIMEOUT, Client::connect(Some(&target), &options))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(echo(&client, "plain").await, "plain\n");
        let requests = server.requests.lock().unwrap();
        assert!(requests.streamlocal.is_empty(), "{requests:?}");
        assert_eq!(
            requests.exec,
            [
                "yas --version".to_string(),
                format!(
                    "yas --on socket:{} connect --stdio",
                    hosted.socket_path().display()
                ),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exec_mode_says_when_yas_is_missing() {
        let home = tempfile::tempdir().unwrap();
        let server = TestSshServer::start(ssh_server::Options {
            client_key: ssh_server::key(0x33).public_key().clone(),
            streamlocal: false,
            env: vec![
                ("PATH".into(), "/nonexistent".into()),
                ("HOME".into(), home.path().display().to_string()),
            ],
        })
        .await;
        let pool = SshPool::with_options(ssh_options(&server, SshMode::Exec));
        let error = pool.connect_yas("127.0.0.1", None, None).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("`yas --version` did not run on 127.0.0.1"),
            "{error}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_refused_host_key_names_the_presented_fingerprint() {
        let home = tempfile::tempdir().unwrap();
        let server = ssh_server(true, home.path()).await;
        let mut ssh = ssh_options(&server, SshMode::Auto);
        let other = ssh_server::key(0x44).public_key().clone();
        ssh.host_keys = HostKeyPolicy::Pinned(vec![other]);
        let error = SshPool::with_options(ssh)
            .connect_yas("127.0.0.1", None, Some("/nonexistent.sock"))
            .await
            .unwrap_err();
        match error {
            yas_client::ssh::Error::HostKey {
                host,
                port,
                fingerprint,
                ..
            } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, server.port);
                assert_eq!(fingerprint, yas_client::ssh::fingerprint(&server.host_key));
            }
            other => panic!("expected a HostKey error, got {other}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_session_on_the_read_only_socket_is_read_only() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let read_only = directory.path().join("viewers.sock");
    let server = tokio::time::timeout(
        TIMEOUT,
        HostedServer::start(options().arg("--read-only-sock").arg(&read_only)),
    )
    .await
    .expect("hosted server start timed out")
    .expect("hosted server starts");
    assert_eq!(
        std::fs::metadata(&read_only).unwrap().permissions().mode() & 0o777,
        0o700,
        "the read-only socket is as private as the server's own"
    );

    // The server's own socket still grants everything.
    let full = server.connect().await.unwrap();
    let status = full
        .spawn(&Command::new("true"))
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(status.success(), "{status:?}");

    // A client there that asks for an ordinary session gets a read-only one.
    let target = format!("socket:{}", read_only.display());
    let viewer = tokio::time::timeout(
        TIMEOUT,
        Client::connect(Some(&target), &yas_client::ConnectOptions::named("viewer")),
    )
    .await
    .expect("connect timed out")
    .unwrap();
    let spawn = viewer
        .spawn(&Command::new("true"))
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(spawn, Error::Unsupported(_)), "{spawn:?}");
    assert!(viewer.open_root(directory.path(), false).await.is_err());
    assert!(viewer.env_var("PATH").await.is_err());
}

/// Poll the terminal's screen until it shows `needle`; answers the screen.
async fn wait_for_screen(client: &Client, id: u64, needle: &str) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let screen = client.terminal_screen(id).await.unwrap();
        if screen.contains(needle) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} never showed; the screen:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminals_take_keystrokes_show_their_screen_and_report_their_exit() {
    use yas_client::terminal::{ExitRecord, TerminalCommand, TerminalStatus};
    let server = start().await;
    let client = server.connect().await.unwrap();
    let id = client
        .start_terminal(
            &TerminalCommand::new("sh")
                .env("PS1", "$ ")
                .env("YAS_CLIENT_TERMINAL", "typed")
                .size(10, 60)
                .tag("client-test"),
        )
        .await
        .unwrap();
    let info = client.terminal(id).await.unwrap();
    assert_eq!((info.rows, info.cols), (10, 60));
    assert_eq!(info.tag.as_deref(), Some("client-test"));
    assert!(info.is_running(), "{info:?}");
    assert!(
        client
            .terminals()
            .await
            .unwrap()
            .iter()
            .any(|terminal| terminal.id == id)
    );

    client
        .write_terminal(id, b"echo $YAS_CLIENT_TERMINAL-$((6 * 7))\r")
        .await
        .unwrap();
    wait_for_screen(&client, id, "typed-42").await;
    client.resize_terminal(id, 12, 70).await.unwrap();
    client.write_terminal(id, b"stty size\r").await.unwrap();
    wait_for_screen(&client, id, "12 70").await;
    assert_eq!(client.terminal(id).await.unwrap().cols, 70);

    // Another session sees the same terminal; a read-only one cannot type.
    let viewer = server
        .connect_with(&HelloOptions::named("terminal-viewer").read_only(true))
        .await
        .unwrap();
    assert!(
        viewer
            .terminal_screen(id)
            .await
            .unwrap()
            .contains("typed-42")
    );
    let typed = viewer.write_terminal(id, b"exit\r").await.unwrap_err();
    assert!(matches!(typed, Error::Unsupported(_)), "{typed:?}");

    let waiter = tokio::spawn(async move { viewer.wait_terminal_exit(id).await });
    client.write_terminal(id, b"exit 3\r").await.unwrap();
    let exited = tokio::time::timeout(TIMEOUT, waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            exited.status,
            TerminalStatus::Exited(Some(ExitRecord::Code { code: 3, .. }))
        ),
        "{exited:?}"
    );
    client.close_terminal(id).await.unwrap();
    assert!(client.terminal(id).await.unwrap_err().is_not_found());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminals_restart_take_signals_and_keep_deadlines() {
    use yas_client::terminal::{
        ExitReason, ExitRecord, SignalKind, TerminalCommand, TerminalStatus,
    };
    let server = start().await;
    let client = server.connect().await.unwrap();
    let id = client
        .start_terminal(&TerminalCommand::new("sh").args(["-c", "echo run-$$; exec sleep 600"]))
        .await
        .unwrap();
    wait_for_screen(&client, id, "run-").await;
    let generation = client.terminal(id).await.unwrap().generation;
    client.restart_terminal(id).await.unwrap();
    let restarted = client.terminal(id).await.unwrap();
    assert!(restarted.generation > generation, "{restarted:?}");
    assert!(restarted.is_running(), "{restarted:?}");

    let waiter = {
        let client = client.clone();
        tokio::spawn(async move { client.wait_terminal_exit(id).await })
    };
    client
        .signal_terminal(id, SignalKind::Terminate)
        .await
        .unwrap();
    let exited = tokio::time::timeout(TIMEOUT, waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            exited.status,
            TerminalStatus::Exited(Some(ExitRecord::Signal {
                reason: ExitReason::Terminate,
                ..
            }))
        ),
        "{exited:?}"
    );
    client.close_terminal(id).await.unwrap();

    let started = Instant::now();
    let id = client
        .start_terminal(
            &TerminalCommand::shell_command("sleep 600").deadline(Duration::from_millis(300)),
        )
        .await
        .unwrap();
    let exited = tokio::time::timeout(TIMEOUT, client.wait_terminal_exit(id))
        .await
        .unwrap()
        .unwrap();
    assert!(!exited.is_running(), "{exited:?}");
    assert!(started.elapsed() < Duration::from_secs(20));
    client.close_terminal(id).await.unwrap();
    assert!(client.close_terminal(id).await.unwrap_err().is_not_found());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_send_refuses_unknown_and_exited_terminals_even_with_empty_input() {
    use yas_client::terminal::{SignalKind, TerminalCommand};
    async fn send(on: &str, id: u64, text: &str) -> std::process::Output {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_yas"))
            .args(["--on", on, "terminal", "send"])
            .arg(id.to_string())
            .arg(text)
            .env("YAS_PROXY", "0")
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .unwrap()
    }
    fn refused(output: std::process::Output, message: &str) {
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!("yas: {message}\n")
        );
    }
    let server = start().await;
    let client = server.connect().await.unwrap();
    let on = format!("socket:{}", server.socket_path().display());

    let id = client
        .start_terminal(&TerminalCommand::new("sh").args(["-c", "echo run-$$; exec sleep 600"]))
        .await
        .unwrap();
    wait_for_screen(&client, id, "run-").await;
    for text in ["", "ls\\n"] {
        let output = send(&on, id, text).await;
        assert!(output.status.success(), "{output:?}");
    }

    let waiter = {
        let client = client.clone();
        tokio::spawn(async move { client.wait_terminal_exit(id).await })
    };
    client
        .signal_terminal(id, SignalKind::Terminate)
        .await
        .unwrap();
    tokio::time::timeout(TIMEOUT, waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    for text in ["", "ls\\n"] {
        refused(
            send(&on, id, text).await,
            &format!(
                "cannot send to pty {id}: it is no longer running (status signal(15, Terminate)); \
                 `yas terminal restart {id}` re-runs it"
            ),
        );
    }

    client.close_terminal(id).await.unwrap();
    for text in ["", "ls\\n"] {
        refused(send(&on, id, text).await, &format!("pty {id} not found"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_whose_shell_reports_its_commands_answers_journal_output_and_cwd() {
    use yas_client::terminal::TerminalCommand;
    let server = start().await;
    let client = server.connect().await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    // What a shell with OSC 7 and OSC 133 integration prints for one command
    // (`echo journal`, exit 7), with the command held open for a moment.
    let script = r#"printf '\033]7;file://localhost%s\007' "$PWD"
printf '\033]133;A\007$ \033]133;B\007echo journal\r\n\033]133;C\007'
echo journal-output
sleep 3
printf '\033]133;D;7\007\033]133;A\007$ \033]133;B\007'
exec sleep 600"#;
    let id = client
        .start_terminal(
            &TerminalCommand::new("sh")
                .args(["-c", script])
                .current_dir(directory.path()),
        )
        .await
        .unwrap();
    wait_for_screen(&client, id, "journal-output").await;
    let running = client
        .wait_terminal_command(id, None, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert!(matches!(running, Error::Timeout(_)), "{running:?}");
    let record = client
        .wait_terminal_command(id, None, Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!(record.exit_code, 7, "{record:?}");
    let commands = client.terminal_commands(id, 10).await.unwrap();
    assert!(commands.contains(&record), "{commands:?}");
    let again = client
        .wait_terminal_command(id, Some(record.index), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(again, record);
    let output = client
        .terminal_output(id, Some(record.index), 4096)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&output.text);
    assert!(text.contains("journal-output"), "{output:?}");
    assert!(!output.truncated && !output.evicted, "{output:?}");
    let cwd = client.terminal_cwd(id).await.unwrap();
    assert_eq!(
        std::fs::canonicalize(String::from_utf8(cwd).unwrap()).unwrap(),
        std::fs::canonicalize(directory.path()).unwrap()
    );
    // Nothing else starts: waiting for the next command times out (TIMEOUT
    // since #54; NOT_FOUND is for an exited terminal or an evicted index).
    let waited = client
        .wait_terminal_command(id, None, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert_eq!(
        waited.status(),
        Some(yas_client::wire::core::Status::Timeout),
        "{waited:?}"
    );
    client.close_terminal(id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn surfaces_are_none_without_the_compositor() {
    use yas_client::surface::CaptureFormat;
    let server = start().await;
    let client = server.connect().await.unwrap();
    assert!(client.surfaces().await.unwrap().is_empty());
    let missing = client
        .capture_surface(1, CaptureFormat::Png)
        .await
        .unwrap_err();
    assert!(missing.is_not_found(), "{missing:?}");
    let missing = client
        .capture_surface_at(1, 1, CaptureFormat::Png)
        .await
        .unwrap_err();
    assert!(missing.is_not_found(), "{missing:?}");
}

/// Against a real window: build the compositor's probe client
/// (`cargo build -p yas-compositor --example paste_probe`) and run with
/// `YAS_CLIENT_TEST_PASTE_PROBE=target/debug/examples/paste_probe`. Skipped
/// without it: the probe is a development example, not a test binary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn surfaces_capture_take_input_and_close_with_the_paste_probe() {
    use yas_client::surface::{CaptureFormat, PointerButton, key_combo, typed_keys};
    let Some(probe) = std::env::var_os("YAS_CLIENT_TEST_PASTE_PROBE") else {
        eprintln!("skipped: YAS_CLIENT_TEST_PASTE_PROBE names no paste_probe build");
        return;
    };
    let server = tokio::time::timeout(TIMEOUT, HostedServer::start(options().compositor(true)))
        .await
        .expect("hosted server start timed out")
        .expect("hosted server starts");
    let client = server.connect().await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("probe.log");
    let read_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let wait_for_log = |needle: &'static str| {
        let read_log = &read_log;
        async move {
            let deadline = Instant::now() + TIMEOUT;
            while !read_log().contains(needle) {
                assert!(
                    Instant::now() < deadline,
                    "the probe never logged {needle:?}:\n{}",
                    read_log()
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    let probe_process = client
        .spawn(
            Command::new("sh")
                .args(["-c", r#"exec "$0" > "$1" 2>&1"#])
                .arg(&probe)
                .arg(&log),
        )
        .await
        .unwrap();
    wait_for_log("READY").await;
    let deadline = Instant::now() + TIMEOUT;
    let surface = loop {
        let surfaces = client.surfaces().await.unwrap();
        if let Some(surface) = surfaces
            .into_iter()
            .find(|surface| surface.app_id == "paste-probe")
        {
            break surface;
        }
        assert!(Instant::now() < deadline, "no paste-probe surface");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(surface.title, "paste-probe");
    assert!(surface.width > 0 && surface.height > 0, "{surface:?}");
    assert_eq!(client.surface(surface.id).await.unwrap().id, surface.id);

    let png = client
        .capture_surface(surface.id, CaptureFormat::Png)
        .await
        .unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "{} bytes", png.len());

    client.focus_surface(surface.id).await.unwrap();
    client
        .click_surface(surface.id, 10, 10, PointerButton::Left)
        .await
        .unwrap();
    client.scroll_surface(surface.id, 0.0, 1.0).await.unwrap();
    client
        .press_surface_keys(surface.id, &key_combo("a").unwrap())
        .await
        .unwrap();
    wait_for_log(" down").await;
    wait_for_log(" up").await;
    client
        .press_surface_keys(surface.id, &typed_keys("Hi{enter}").unwrap())
        .await
        .unwrap();
    client.type_surface_text(surface.id, "é").await.unwrap();
    // At the revision listed, the capture goes at once.
    let listed = client.surface(surface.id).await.unwrap();
    let png = client
        .capture_surface_at(surface.id, listed.revision, CaptureFormat::Png)
        .await
        .unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "{} bytes", png.len());
    client.resize_surface(surface.id, 320, 240).await.unwrap();
    // Once the window changed, that revision is stale: the server says so, and
    // capture_surface_at looks the window up again rather than failing.
    let deadline = Instant::now() + TIMEOUT;
    while client.surface(surface.id).await.unwrap().revision == listed.revision {
        assert!(
            Instant::now() < deadline,
            "the resize never changed the window"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stale = client
        .request_raw(
            yas_client::wire::family::SURFACE,
            yas_client::wire::surface::request_kind::CAPTURE,
            yas_client::wire::Encode::encode(&yas_client::wire::surface::Capture {
                surface_handle: surface.id,
                revision: listed.revision,
                initial_receive_credit: 0,
                formats: vec![yas_client::wire::schema::surface::CAPTURE_PNG as u8],
                extensions: Default::default(),
            })
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(stale.status, yas_client::wire::core::Status::Stale);
    let png = client
        .capture_surface_at(surface.id, listed.revision, CaptureFormat::Png)
        .await
        .unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "{} bytes", png.len());

    // A read-only session sees the window but cannot send it input.
    let viewer = server
        .connect_with(&HelloOptions::named("surface-viewer").read_only(true))
        .await
        .unwrap();
    assert!(
        viewer
            .surfaces()
            .await
            .unwrap()
            .iter()
            .any(|seen| seen.id == surface.id)
    );
    let clicked = viewer
        .click_surface(surface.id, 1, 1, PointerButton::Left)
        .await
        .unwrap_err();
    assert!(matches!(clicked, Error::Unsupported(_)), "{clicked:?}");

    client.close_surface(surface.id).await.unwrap();
    wait_for_log("TOPLEVEL-CLOSE").await;
    probe_process.signal(Signal::Terminate).await.unwrap();
    assert!(client.surface(0).await.unwrap_err().is_not_found());
}
