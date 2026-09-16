//! Opt-in test of keyboard delivery through a real nested compositor.
//!
//! Run with `cargo test -p yas-compositor --test keyboard_weston -- --ignored`.
//! Requires Weston, Alacritty, and Python 3. YAS_TEST_WESTON and
//! YAS_TEST_ALACRITTY can name executables outside PATH.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use yas_compositor::{
    CompositorCommand, CompositorEvent, CompositorHandle, spawn_compositor_without_renderer,
};

const RECORD_INPUT: &str = r#"
import os
import pathlib
import sys
import tty

tty.setraw(0)
directory = pathlib.Path(sys.argv[1])
with (directory / 'typed').open('wb', buffering=0) as output:
    (directory / 'ready').touch()
    while True:
        output.write(os.read(0, 1024))
"#;

struct Fixture {
    children: Vec<Child>,
    handle: Option<CompositorHandle>,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for child in self.children.iter_mut().rev() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.handle.take() {
            handle.stop();
        }
        if std::thread::panicking() {
            eprintln!("Weston keyboard test logs: {}", self.dir.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[test]
#[ignore = "starts Weston and Alacritty"]
fn alacritty_receives_keys_through_weston() {
    let dir = std::env::temp_dir().join(format!("yas-keyboard-weston-{}", std::process::id()));
    std::fs::create_dir(&dir).expect("scratch dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let handle = spawn_compositor_without_renderer(false, Arc::new(|| {}));
    let mut fx = Fixture {
        children: Vec::new(),
        handle: Some(handle),
        dir,
    };
    let handle = fx.handle.as_ref().unwrap();
    let weston = std::env::var_os("YAS_TEST_WESTON").unwrap_or_else(|| "weston".into());
    fx.children.push(
        Command::new(weston)
            .args([
                "--backend=wayland",
                "--renderer=pixman",
                "--shell=kiosk-shell.so",
                "--no-config",
                "--idle-time=0",
                "--socket=nested",
                "--width=800",
                "--height=600",
            ])
            .arg(format!("--display={}", handle.socket_name))
            .env("XDG_RUNTIME_DIR", &fx.dir)
            .env_remove("WAYLAND_SOCKET")
            .env("WAYLAND_DEBUG", "client")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(fx.dir.join("weston.log")).unwrap())
            .spawn()
            .expect("start Weston"),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let surface_id = loop {
        assert!(Instant::now() < deadline, "Weston did not create an output");
        if let Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) =
            handle.event_rx.recv_timeout(Duration::from_millis(100))
        {
            break surface_id;
        }
    };
    while !fx.dir.join("nested").exists() {
        assert!(Instant::now() < deadline, "Weston did not open its socket");
        std::thread::sleep(Duration::from_millis(20));
    }
    let alacritty = std::env::var_os("YAS_TEST_ALACRITTY").unwrap_or_else(|| "alacritty".into());
    fx.children.push(
        Command::new(alacritty)
            .args([
                "--config-file",
                "/dev/null",
                "--hold",
                "-e",
                "python3",
                "-c",
                RECORD_INPUT,
            ])
            .arg(&fx.dir)
            .env("XDG_RUNTIME_DIR", &fx.dir)
            .env("WAYLAND_DISPLAY", "nested")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(fx.dir.join("alacritty.log")).unwrap())
            .spawn()
            .expect("start Alacritty"),
    );
    while !fx.dir.join("ready").exists() {
        assert!(
            Instant::now() < deadline,
            "Alacritty did not start its child"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The PTY child starts before Alacritty finishes mapping its window.
    std::thread::sleep(Duration::from_millis(500));
    handle
        .command_tx
        .send(CompositorCommand::SurfaceFocus { surface_id })
        .unwrap();
    handle
        .command_tx
        .send(CompositorCommand::TextInput { text: "aA".into() })
        .unwrap();
    for (keycode, pressed) in [
        (28, true),
        (28, false),
        (29, true),
        (46, true),
        (46, false),
        (29, false),
    ] {
        handle
            .command_tx
            .send(CompositorCommand::KeyInput {
                surface_id,
                keycode,
                pressed,
                caps_lock: None,
                time_ms: 0,
            })
            .unwrap();
    }
    handle.wake();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let typed = std::fs::read(fx.dir.join("typed")).unwrap_or_default();
        if typed == b"aA\r\x03" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Alacritty received {typed:?}, expected aA, Enter, Ctrl+C"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
