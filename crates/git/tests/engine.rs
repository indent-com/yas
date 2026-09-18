use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use yas_git::native::{
    self, BlameRecord, DiffRecord, DiffRequest, Endpoint, LogRequest, PatchRecord, PatchRequest,
    PatchResult, ReflogRecord, StateEvent, StateRecord, Status, WorktreeRecord,
};
use yas_git::{Cancel, StateOptions};

struct Repository(PathBuf);

impl Repository {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "yas-git-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        // Git reports canonical worktree paths. macOS exposes its temporary
        // directory through `/var`, which resolves to `/private/var`, so keep
        // the fixture path in the same form on every platform.
        let repository = Self(std::fs::canonicalize(path).unwrap());
        repository.git(&["init", "-q"]);
        repository.git(&["config", "user.name", "YAS Test"]);
        repository.git(&["config", "user.email", "yas@example.invalid"]);
        repository
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn remote_path(&self) -> String {
        let path = self.0.to_str().unwrap();
        // Git parses remote paths as URLs. A Windows canonical path starts
        // with `\\?\`, which Git mistakes for an SSH host instead of a drive.
        #[cfg(windows)]
        let path = path
            .strip_prefix(r"\\?\")
            .unwrap_or(path)
            .replace('\\', "/");
        path.to_owned()
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.0.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "."]);
        self.git(&["commit", "-q", "-m", message]);
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path(value: &str) -> Vec<Vec<u8>> {
    value
        .split('/')
        .map(|part| part.as_bytes().to_vec())
        .collect()
}

fn open(repository: &Repository) -> (yas_git::RepoHandle, native::Oid) {
    let (handle, info) = native::open_path(&repository.0).unwrap();
    assert_eq!(info.object_format, native::ObjectFormat::Sha1);
    let resolved = handle.native_resolve("HEAD", &Cancel::default()).unwrap();
    (handle, resolved.tips[0])
}

fn resolve(handle: &yas_git::RepoHandle, spec: &str) -> native::Oid {
    handle
        .native_resolve(spec, &Cancel::default())
        .unwrap()
        .tips[0]
}

struct StateStream {
    handle: yas_git::StateHandle,
    events: std::sync::mpsc::Receiver<StateEvent>,
}

impl StateStream {
    fn new(repository: &yas_git::RepoHandle, options: StateOptions) -> Self {
        let (sender, events) = std::sync::mpsc::channel();
        let handle = repository
            .start_native_state(options, Box::new(move |event| sender.send(event).is_ok()));
        Self { handle, events }
    }

    fn status(&self) -> BTreeMap<String, (u8, u8)> {
        let StateEvent::Snapshot { state_id, records } = self
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("state snapshot timed out")
        else {
            panic!("state engine closed unexpectedly")
        };
        self.handle.ack(state_id);
        records
            .into_iter()
            .filter_map(|record| match record {
                StateRecord::Status {
                    path,
                    index_status,
                    worktree_status,
                    ..
                } => Some((
                    path.iter()
                        .map(|part| String::from_utf8_lossy(part))
                        .collect::<Vec<_>>()
                        .join("/"),
                    (index_status, worktree_status),
                )),
                _ => None,
            })
            .collect()
    }
}

fn status_options(untracked: bool, ignored: bool) -> StateOptions {
    StateOptions {
        status: true,
        untracked,
        ignored,
        refs_latency: Duration::from_millis(5),
        status_latency: Duration::from_millis(5),
        ..Default::default()
    }
}

#[test]
fn semantic_reads_cover_log_tree_and_blob() {
    let repository = Repository::new();
    repository.write("src/main.rs", b"fn main() {}\n");
    repository.commit("first");
    let (handle, head) = open(&repository);
    let cancel = Cancel::default();
    let log = handle
        .native_log(
            &LogRequest {
                flags: 0,
                limit: 16,
                path: Vec::new(),
                tips: vec![head],
                hides: Vec::new(),
            },
            &cancel,
        )
        .unwrap();
    assert!(log.records.iter().any(|record| matches!(
        record,
        native::LogRecord::Commit { message, .. } if message == b"first"
    )));
    let tree = handle
        .native_tree(head, &path("src"), &[], &cancel)
        .unwrap();
    let blob = tree
        .records
        .iter()
        .find_map(|record| match record {
            native::TreeRecord::Entry { object, name, .. } if name == b"main.rs" => Some(*object),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        handle.native_blob(blob, None, 0, 1024, 0).unwrap().bytes,
        b"fn main() {}\n"
    );
}

#[test]
fn semantic_diff_patch_and_index_have_no_packet_round_trip() {
    let repository = Repository::new();
    repository.write("file.txt", b"old\n");
    repository.commit("base");
    let (handle, head) = open(&repository);
    repository.write("file.txt", b"new\n");
    let cancel = Cancel::default();
    let diff = handle
        .native_diff(
            &DiffRequest {
                flags: 0,
                rename_threshold: 0,
                old: Endpoint::Commit(head),
                new: Endpoint::Worktree,
                path: Vec::new(),
                after: Vec::new(),
            },
            &cancel,
        )
        .unwrap();
    assert!(diff.records.iter().any(|record| matches!(
        record,
        DiffRecord::Entry { new_path: Some(value), .. } if value == &path("file.txt")
    )));
    let patch = handle
        .native_patch(
            &PatchRequest {
                flags: 0,
                context_lines: 3,
                rename_threshold: 0,
                old: Endpoint::Commit(head),
                new: Endpoint::Worktree,
                path: path("file.txt"),
                max_bytes: 64 * 1024,
                after: Vec::new(),
                after_position: 0,
            },
            &cancel,
        )
        .unwrap();
    let PatchResult::Structured(records) = patch else {
        panic!("expected a structured patch")
    };
    assert!(records.iter().any(|record| matches!(record,
        PatchRecord::File { new_path: Some(value), .. } if value == &path("file.txt")
    )));
    assert!(records.iter().any(|record| matches!(record,
        PatchRecord::Row { old_text, new_text, .. }
            if old_text == b"old" && new_text == b"new"
    )));
    assert!(
        !handle
            .native_index(&[], &[], &cancel)
            .unwrap()
            .records
            .is_empty()
    );
}

#[test]
fn semantic_state_stream_is_typed_and_acked() {
    let repository = Repository::new();
    repository.write("tracked", b"one\n");
    repository.commit("base");
    repository.git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/repository",
    ]);
    let (handle, _) = open(&repository);
    repository.write("tracked", b"two\n");
    repository.write("untracked", b"new\n");
    let (tx, rx) = std::sync::mpsc::channel();
    let state = handle.start_native_state(
        StateOptions {
            status: true,
            untracked: true,
            remotes: true,
            refs_latency: Duration::from_millis(5),
            status_latency: Duration::from_millis(5),
            ..StateOptions::default()
        },
        Box::new(move |event| tx.send(event).is_ok()),
    );
    let StateEvent::Snapshot { state_id, records } =
        rx.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("state engine closed unexpectedly")
    };
    assert!(
        records
            .iter()
            .any(|record| matches!(record, StateRecord::Head { .. }))
    );
    assert!(records.iter().any(|record| matches!(record,
        StateRecord::Status { path: value, .. } if value == &path("tracked")
    )));
    assert!(records.iter().any(|record| matches!(record,
        StateRecord::Status { path: value, .. } if value == &path("untracked")
    )));
    assert!(records.iter().any(|record| matches!(record,
        StateRecord::Remote { name, fetch_url, .. }
            if name == b"origin" && fetch_url.starts_with(b"https://example.invalid")
    )));
    state.ack(state_id);
}

#[test]
fn discovery_returns_platform_paths() {
    let repository = Repository::new();
    repository.write("tracked", b"one\n");
    repository.commit("base");
    let page = native::discover_path(0, 1, &repository.0, None, &Cancel::default()).unwrap();
    assert!(page.records.iter().any(|record| matches!(
        record,
        native::DiscoveryRecord::Repository { git_dir, .. } if git_dir.starts_with(&repository.0)
    )));
}

#[test]
fn semantic_history_helpers_cover_resolve_merge_base_blame_and_reflog() {
    let repository = Repository::new();
    repository.write("story.txt", b"first\nsecond\n");
    repository.commit("first");
    repository.write("story.txt", b"first\nchanged\n");
    repository.commit("second");
    let (handle, head) = open(&repository);
    let parent = resolve(&handle, "HEAD^");
    let range = handle
        .native_resolve("HEAD^..HEAD", &Cancel::default())
        .unwrap();
    assert_eq!(range.tips, vec![head]);
    assert_eq!(range.hides, vec![parent]);
    assert_eq!(
        handle
            .native_merge_base(&[head, parent], &Cancel::default())
            .unwrap(),
        vec![parent]
    );

    let blame = handle
        .native_blame(head, &path("story.txt"), 1, 16, 0, &Cancel::default())
        .unwrap();
    assert!(blame.records.iter().any(|record| matches!(record,
        BlameRecord::Range { commit, .. } if *commit == parent
    )));
    assert!(blame.records.iter().any(|record| matches!(record,
        BlameRecord::Range { commit, .. } if *commit == head
    )));

    let reflog = handle
        .native_reflog("HEAD", 0, 16, 0, &Cancel::default())
        .unwrap();
    assert!(reflog.records.iter().any(|record| matches!(record,
        ReflogRecord::Entry { message, new_object, .. }
            if message.windows(b"second".len()).any(|part| part == b"second")
                && *new_object == head
    )));
}

#[test]
fn semantic_worktrees_submodules_and_invalid_paths_are_typed() {
    let child = Repository::new();
    child.write("child.txt", b"child\n");
    child.commit("child");

    let parent = Repository::new();
    parent.write("parent.txt", b"parent\n");
    parent.commit("parent");
    parent.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        &child.remote_path(),
        "deps/child",
    ]);
    parent.commit("submodule");
    let (handle, _) = open(&parent);

    let worktrees = handle.native_worktrees(0, &Cancel::default()).unwrap();
    assert!(worktrees.records.iter().any(|record| matches!(record,
        WorktreeRecord::Worktree { main: true, path: Some(value), .. } if value == &parent.0
    )));

    let (_, info) = native::open_submodule_path(&handle, std::path::Path::new("deps/child"))
        .expect("open initialized submodule");
    assert!(
        info.worktree_path
            .as_deref()
            .is_some_and(|value| value.ends_with("deps/child"))
    );

    let error = handle
        .native_tree([0; 32], &[b"bad/slash".to_vec()], &[], &Cancel::default())
        .unwrap_err();
    assert_eq!(error.status, Status::Invalid);
}

#[test]
fn semantic_fetch_reports_local_remote_ref_updates() {
    if !native::fetch_available() {
        return;
    }
    let source = Repository::new();
    source.write("remote.txt", b"remote\n");
    source.commit("remote");

    let consumer = Repository::new();
    consumer.git(&["remote", "add", "origin", &source.remote_path()]);
    let (handle, _) = native::open_path(&consumer.0).unwrap();
    let result = handle
        .native_fetch("origin", &[], 0, 10_000, &Cancel::default())
        .unwrap();
    assert!(result.refs.iter().any(|reference| {
        reference.status == Status::Ok
            && reference.new_ref
            && reference.name.starts_with(b"refs/remotes/origin/")
    }));
}

#[test]
fn status_keeps_tracked_files_and_ancestors_observable_despite_ignore_rules() {
    for untracked in [false, true] {
        let repository = Repository::new();
        repository.write("tracked.log", b"initial\n");
        repository.write("generated/nested/tracked", b"initial\n");
        repository.commit("track files before adding ignore rules");
        repository.write(".gitignore", b"*.log\ngenerated/\n");
        repository.commit("ignore tracked paths");
        repository.write("generated/cache/ignored", b"ignored\n");
        let (handle, _) = open(&repository);
        let state = StateStream::new(&handle, status_options(untracked, false));
        assert!(state.status().is_empty());
        let watches = yas_git::debug_worktree_watches(&repository.0.join(".git")).unwrap();
        assert!(watches.contains(&repository.0.join("generated/nested")));
        assert!(!watches.contains(&repository.0.join("generated/cache")));

        repository.write("tracked.log", b"modified tracked file\n");
        assert_eq!(state.status()["tracked.log"], (b' ', b'M'));
        repository.write("generated/nested/tracked", b"modified tracked descendant\n");
        assert_eq!(state.status()["generated/nested/tracked"], (b' ', b'M'));
        std::fs::remove_file(repository.0.join("tracked.log")).unwrap();
        assert_eq!(state.status()["tracked.log"], (b' ', b'D'));
    }
}

#[test]
fn index_changes_reconcile_tracked_exceptions_to_ignore_pruning() {
    let repository = Repository::new();
    repository.write(".gitignore", b"generated/\n");
    repository.commit("base");
    repository.write("generated/nested/tracked", b"initial\n");
    let (handle, _) = open(&repository);
    let state = StateStream::new(&handle, status_options(true, false));
    assert!(state.status().is_empty());
    let watches = || yas_git::debug_worktree_watches(&repository.0.join(".git")).unwrap();
    assert!(!watches().contains(&repository.0.join("generated")));

    repository.git(&["add", "-f", "generated/nested/tracked"]);
    assert_eq!(state.status()["generated/nested/tracked"], (b'A', b' '));
    assert!(watches().contains(&repository.0.join("generated/nested")));
    repository.write("generated/nested/tracked", b"modified after force-add\n");
    assert_eq!(state.status()["generated/nested/tracked"], (b'A', b'M'));

    repository.git(&["rm", "--cached", "-f", "generated/nested/tracked"]);
    assert!(state.status().is_empty());
    assert!(!watches().contains(&repository.0.join("generated")));
}

#[test]
fn status_budgets_are_independent_across_shared_selections() {
    for ignored in [false, true] {
        let repository = Repository::new();
        repository.write("z-tracked", b"initial\n");
        repository.write(".gitignore", if ignored { b"a-generated/\n" } else { b"" });
        repository.commit("base");
        repository.write("z-tracked", b"modified\n");
        repository.write("z-untracked", b"untracked\n");
        for index in 0..10 {
            repository.write(&format!("a-generated/{index}"), b"generated\n");
        }
        let (mut handle, _) = open(&repository);
        handle.budgets = std::sync::Arc::new(yas_git::Budgets {
            entries_max: 8,
            ..Default::default()
        });
        // Tracked-only beside untracked, then untracked beside ignored:
        // each broader selection exhausts its own budget before z-tracked.
        let selected_options = status_options(ignored, false);
        let selected = StateStream::new(&handle, selected_options.clone());
        let expected = selected.status();
        assert_eq!(expected["z-tracked"], (b' ', b'M'));
        assert_eq!(expected.contains_key("z-untracked"), ignored);
        let broader = StateStream::new(&handle, status_options(true, ignored));
        let broad_status = broader.status();
        assert_eq!(broad_status.len(), 8);
        assert!(
            broad_status
                .keys()
                .all(|name| name.starts_with("a-generated/"))
        );

        // Attaching in the other order must also use the selected budget.
        let another = StateStream::new(&handle, selected_options);
        assert_eq!(another.status(), expected);
        repository.write("z-tracked", b"modified again\n");
        assert_eq!(selected.status(), expected);
        assert_eq!(another.status(), expected);
    }
}

#[test]
fn non_status_subscribers_do_not_widen_status_collection() {
    let repository = Repository::new();
    repository.write(".gitignore", b"ignored/\n");
    repository.write("tracked", b"initial\n");
    repository.commit("base");
    repository.write("ignored/nested/file", b"ignored\n");
    repository.write("tracked", b"modified\n");
    let (handle, _) = open(&repository);
    let refs = StateStream::new(
        &handle,
        StateOptions {
            status: false,
            ..status_options(true, true)
        },
    );
    assert!(refs.status().is_empty());
    let selected = StateStream::new(&handle, status_options(true, false));
    assert_eq!(selected.status().len(), 1);
    let watches = yas_git::debug_worktree_watches(&repository.0.join(".git")).unwrap();
    assert!(!watches.contains(&repository.0.join("ignored")));
    assert_eq!(
        yas_git::debug_status_recomputes(&repository.0.join(".git")),
        1
    );
}

#[test]
fn reattached_status_selection_observes_changes_made_while_pruned() {
    let repository = Repository::new();
    repository.write(".gitignore", b"ignored/\n");
    repository.commit("base");
    repository.write("ignored/first", b"ignored\n");
    let (handle, _) = open(&repository);
    let selected = StateStream::new(&handle, status_options(true, false));
    assert!(selected.status().is_empty());
    let broader = StateStream::new(&handle, status_options(true, true));
    assert_eq!(broader.status().len(), 1);
    drop(broader);

    // Wait for the asynchronous detach to disarm this directory. Writes
    // inside it then cannot invalidate a cached ignored-status segment.
    let deadline = Instant::now() + Duration::from_secs(5);
    while yas_git::debug_worktree_watches(&repository.0.join(".git"))
        .unwrap()
        .contains(&repository.0.join("ignored"))
    {
        assert!(
            Instant::now() < deadline,
            "ignored directory was not pruned"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    repository.write("ignored/second", b"new ignored file\n");
    let broader = StateStream::new(&handle, status_options(true, true));
    assert_eq!(broader.status().len(), 2);
}
