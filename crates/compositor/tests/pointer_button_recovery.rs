//! A press on a button the client still holds must be preceded by its release.
//!
//! A viewer can lose a `mouseup` (the browser swallows it, the connection
//! drops). The compositor then used to hand the client a second press with no
//! release between, which toolkits ignore, so the next click did nothing.

#![cfg(target_os = "linux")]

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use yas_compositor::{CompositorCommand, CompositorEvent, spawn_compositor};

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    pointer: Option<wl_pointer::WlPointer>,
    buttons: Vec<(u32, bool)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => state.compositor = Some(registry.bind(name, 4, qh, ())),
            "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
            "wl_seat" => state.seat = Some(registry.bind(name, version.min(5), qh, ())),
            "xdg_wm_base" => state.wm_base = Some(registry.bind(name, 1, qh, ())),
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        _: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button {
            button, state: s, ..
        } = event
        {
            state.buttons.push((
                button,
                matches!(s, WEnum::Value(wl_pointer::ButtonState::Pressed)),
            ));
        }
    }
}

delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_seat::WlSeat);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);

#[test]
fn a_repeated_press_is_preceded_by_the_lost_release() {
    const BTN_LEFT: u32 = 0x110;
    let handle = spawn_compositor(false, Arc::new(|| {}), "");
    let stream = UnixStream::connect(&handle.socket_name).expect("connect to compositor socket");
    let conn = Connection::from_socket(stream).expect("wayland connection");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut app = App::default();
    queue.roundtrip(&mut app).expect("registry roundtrip");
    let compositor = app.compositor.clone().expect("wl_compositor advertised");
    let wm_base = app.wm_base.clone().expect("xdg_wm_base advertised");
    app.pointer = Some(
        app.seat
            .as_ref()
            .expect("wl_seat advertised")
            .get_pointer(&qh, ()),
    );

    const W: i32 = 160;
    const H: i32 = 120;
    let raw_fd = unsafe { libc::memfd_create(c"pointer-button".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw_fd >= 0, "memfd_create failed");
    assert_eq!(unsafe { libc::ftruncate(raw_fd, (W * H * 4).into()) }, 0);
    let backing = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let pool = app.shm.as_ref().expect("wl_shm advertised").create_pool(
        backing.as_fd(),
        W * H * 4,
        &qh,
        (),
    );
    let buffer = pool.create_buffer(0, W, H, W * 4, wl_shm::Format::Xrgb8888, &qh, ());

    let root = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&root, &qh, ());
    let _toplevel = xdg.get_toplevel(&qh, ());
    root.commit();
    queue.roundtrip(&mut app).expect("configure roundtrip");
    root.attach(Some(&buffer), 0, 0);
    root.damage_buffer(0, 0, W, H);
    root.commit();
    queue.roundtrip(&mut app).expect("buffer roundtrip");

    let surface_id = loop {
        match handle.event_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) => break surface_id,
            Ok(_) => {}
            Err(error) => panic!("compositor never announced the toplevel: {error}"),
        }
    };

    let send = |command| handle.command_tx.send(command).expect("send command");
    send(CompositorCommand::PointerMotion {
        surface_id,
        x: 40.0,
        y: 40.0,
        time_ms: 0,
    });
    for pressed in [true, true, false] {
        send(CompositorCommand::PointerButton {
            surface_id,
            button: BTN_LEFT,
            pressed,
            time_ms: 0,
        });
    }
    handle.wake();
    // The command queue and the Wayland socket are independent inputs: keep
    // round-tripping until the commands' effects arrive.
    for _ in 0..50 {
        queue.roundtrip(&mut app).expect("event roundtrip");
        if app.buttons.len() >= 4 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(
        app.buttons,
        vec![
            (BTN_LEFT, true),
            (BTN_LEFT, false),
            (BTN_LEFT, true),
            (BTN_LEFT, false)
        ],
        "a press on a held button must arrive after the release it lost"
    );
    let _keep_alive = (pool, backing);
    handle.stop();
}
