//! End-to-end tests of the `yas-client` library against real, private YAS
//! servers hosted with `yas_client::host` (fd-channel + private socket).

#![cfg(unix)]

use std::time::{Duration, Instant};

use yas_client::fs::{EntryKind, Precondition, WriteOptions};
use yas_client::host::{HostOptions, HostedServer};
use yas_client::kv::{KvChange, KvPrecondition};
use yas_client::process::{Command, Signal, Stdin};
use yas_client::{Client, Error, HelloOptions};

const TIMEOUT: Duration = Duration::from_secs(30);

fn options() -> HostOptions {
    HostOptions::new(env!("CARGO_BIN_EXE_yas"))
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
    assert!(output.status.success());
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
    use yas_client::process::ProcessChange;
    let server = start().await;
    let client = server.connect().await.unwrap();
    let mut watch = client.watch_processes().await.unwrap();
    assert!(
        matches!(watch.next().await.unwrap(), ProcessChange::Snapshot(list) if list.is_empty())
    );
    let process = client.spawn(&Command::new("true")).await.unwrap();
    let handle = process.handle();
    assert!(process.wait().await.unwrap().success());
    let deadline = Instant::now() + TIMEOUT;
    let mut saw_exit = false;
    while !saw_exit {
        assert!(Instant::now() < deadline, "no exited record for {handle}");
        match tokio::time::timeout(TIMEOUT, watch.next())
            .await
            .unwrap()
            .unwrap()
        {
            ProcessChange::Updated(info) if info.handle == handle => saw_exit = info.exit.is_some(),
            ProcessChange::Removed(removed) if removed == handle => saw_exit = true,
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
        (schema::SPAWN_LEAVE_RESIDUE | schema::SPAWN_STDIN_NULL) as u32
    );
    // The background `sleep` holds stdout: the exit comes after the grace, and
    // the sleep keeps running.
    let started = Instant::now();
    let output = client
        .spawn(
            Command::new("sh")
                .args([
                    "-c",
                    "sleep 60 & echo $!; readlink /proc/self/fd/0 || echo no-proc",
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
    assert!(stdin == "/dev/null" || stdin == "no-proc", "{stdin}");
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

    // In place through a symlink: the same inode, the new bytes.
    use std::os::unix::fs::MetadataExt;
    let inode = std::fs::metadata(base.join("file")).unwrap().ino();
    root.write_in_place("to-file", b"new content")
        .await
        .unwrap();
    assert_eq!(std::fs::read(base.join("file")).unwrap(), b"new content");
    assert_eq!(std::fs::metadata(base.join("file")).unwrap().ino(), inode);
    assert!(
        std::fs::symlink_metadata(base.join("to-file"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        named(root.write_in_place("dir", b"x").await.unwrap_err()),
        pair("EISDIR", "open")
    );
    assert_eq!(
        named(root.write_in_place("missing/file", b"x").await.unwrap_err()),
        pair("ENOENT", "open")
    );

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
