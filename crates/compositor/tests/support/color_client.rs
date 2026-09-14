//! Shared real Wayland client for color/encoder integration tests.
#![allow(dead_code)]
#![cfg(target_os = "linux")]
use std::{
    os::{
        fd::AsFd,
        unix::{fs::FileExt, net::UnixStream},
    },
    sync::Arc,
    time::{Duration, Instant},
};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::{
    wp::color_management::v1::client::{
        wp_color_management_surface_feedback_v1 as feedback, wp_color_management_surface_v1 as cs,
        wp_color_manager_v1 as cm, wp_image_description_creator_icc_v1 as ci,
        wp_image_description_creator_params_v1 as cp, wp_image_description_info_v1 as info,
        wp_image_description_v1 as id,
    },
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
use yas_compositor::{CompositorEvent, PixelData, spawn_compositor};
#[derive(Default)]
pub struct App {
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub shm: Option<wl_shm::WlShm>,
    pub wm: Option<xdg_wm_base::XdgWmBase>,
    pub color: Option<cm::WpColorManagerV1>,
    pub ready: usize,
    pub information_done: usize,
}
impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = e
        {
            match interface.as_str() {
                "wl_compositor" => s.compositor = Some(r.bind(name, 4, q, ())),
                "wl_shm" => s.shm = Some(r.bind(name, 1, q, ())),
                "xdg_wm_base" => s.wm = Some(r.bind(name, 1, q, ())),
                "wp_color_manager_v1" => s.color = Some(r.bind(name, 2, q, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        _: &mut Self,
        s: &xdg_surface::XdgSurface,
        e: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = e {
            s.ack_configure(serial);
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        s: &xdg_wm_base::XdgWmBase,
        e: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            s.pong(serial);
        }
    }
}
impl Dispatch<id::WpImageDescriptionV1, ()> for App {
    fn event(
        s: &mut Self,
        _: &id::WpImageDescriptionV1,
        e: id::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            id::Event::Ready { .. } => s.ready += 1,
            id::Event::Failed { .. } => panic!("description failed: {e:?}"),
            _ => {}
        }
    }
}
delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(App: ignore cm::WpColorManagerV1);
delegate_noop!(App: ignore cs::WpColorManagementSurfaceV1);
delegate_noop!(App: ignore feedback::WpColorManagementSurfaceFeedbackV1);
delegate_noop!(App: ignore cp::WpImageDescriptionCreatorParamsV1);
delegate_noop!(App: ignore ci::WpImageDescriptionCreatorIccV1);

impl Dispatch<info::WpImageDescriptionInfoV1, ()> for App {
    fn event(
        s: &mut Self,
        _: &info::WpImageDescriptionInfoV1,
        event: info::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let info::Event::Done = event {
            s.information_done += 1;
        }
    }
}

pub struct ColorClient {
    pub handle: TestCompositor,
    pub surface_id: u16,
    pub initial_pixels: PixelData,
    pub cpu_frames_seen: std::cell::Cell<usize>,
    connection: Connection,
    queue: wayland_client::EventQueue<App>,
    app: App,
    root: wl_surface::WlSurface,
    _buffer: wl_buffer::WlBuffer,
    _file: std::fs::File,
}
impl ColorClient {
    pub fn new(device: &str, output: yas_compositor::color::OutputColor) -> Self {
        Self::with_mastering(device, output, None, 1000.0)
    }
    pub fn with_mastering(
        device: &str,
        output: yas_compositor::color::OutputColor,
        peak_nits: Option<u32>,
        level_nits: f32,
    ) -> Self {
        use yas_compositor::color::OutputColor;
        let handle = spawn_compositor(false, Arc::new(|| {}), device);
        let connection =
            Connection::from_socket(UnixStream::connect(&handle.socket_name).unwrap()).unwrap();
        let mut queue = connection.new_event_queue();
        let q = queue.handle();
        connection.display().get_registry(&q, ());
        let mut app = App::default();
        queue.roundtrip(&mut app).unwrap();
        let manager = app.color.as_ref().expect("Vulkan color manager");
        let root = app.compositor.as_ref().unwrap().create_surface(&q, ());
        let xdg = app.wm.as_ref().unwrap().get_xdg_surface(&root, &q, ());
        let _top = xdg.get_toplevel(&q, ());
        root.commit();
        let color = manager.get_surface(&root, &q, ());
        let creator = manager.create_parametric_creator(&q, ());
        let hdr = output == OutputColor::Hdr10;
        creator.set_primaries_named(if hdr {
            cm::Primaries::Bt2020
        } else {
            cm::Primaries::DisplayP3
        });
        creator.set_tf_named(if hdr {
            cm::TransferFunction::St2084Pq
        } else {
            cm::TransferFunction::Srgb
        });
        if let Some(peak) = peak_nits {
            creator.set_mastering_luminance(50, peak);
        }
        let description = creator.create(&q, ());
        queue.roundtrip(&mut app).unwrap();
        color.set_image_description(&description, cm::RenderIntent::Perceptual);
        let path = std::env::temp_dir().join(format!("yas-direct-color-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        file.set_len(256 * 256 * 4).unwrap();
        let code = if hdr {
            {
                let q = (yas_compositor::color::pq_encode(level_nits) * 1023.0).round() as u32;
                (q << 20) | (q << 10) | q
            }
        } else {
            1023u32 << 20
        };
        file.write_all_at(&code.to_le_bytes().repeat(256 * 256), 0)
            .unwrap();
        let pool = app
            .shm
            .as_ref()
            .unwrap()
            .create_pool(file.as_fd(), 256 * 256 * 4, &q, ());
        let buffer = pool.create_buffer(0, 256, 256, 256 * 4, wl_shm::Format::Xrgb2101010, &q, ());
        root.attach(Some(&buffer), 0, 0);
        root.damage_buffer(0, 0, 256, 256);
        root.commit();
        queue.roundtrip(&mut app).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let (surface_id, initial_pixels) = loop {
            assert!(Instant::now() < deadline);
            if let CompositorEvent::SurfaceCommit {
                surface_id,
                pixels: pixels @ PixelData::LinearRgba { .. },
                ..
            } = handle
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
            {
                break (surface_id, pixels);
            }
        };
        Self {
            handle: TestCompositor(Some(handle)),
            surface_id,
            initial_pixels,
            cpu_frames_seen: std::cell::Cell::new(0),
            connection,
            queue,
            app,
            root,
            _buffer: buffer,
            _file: file,
        }
    }
    pub fn repaint(&mut self) {
        self.root.damage_buffer(0, 0, 256, 256);
        self.root.commit();
        self.queue.roundtrip(&mut self.app).unwrap();
        self.connection.flush().unwrap();
    }
    pub fn gpu_frame(&self, width: u32, dma: bool) -> PixelData {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "no GPU color output");
            if let CompositorEvent::SurfaceCommit {
                width: w, pixels, ..
            } = self
                .handle
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
            {
                if matches!(pixels, PixelData::LinearRgba { .. } | PixelData::Bgra(_)) {
                    self.cpu_frames_seen.set(self.cpu_frames_seen.get() + 1);
                }
                if w == width
                    && matches!(
                        (&pixels, dma),
                        (PixelData::GpuVariants(_), _)
                            | (PixelData::Nv12OpaqueFd { .. }, false)
                            | (PixelData::Nv12DmaBuf { color: Some(_), .. }, true)
                    )
                {
                    return pixels;
                }
            }
        }
    }
}
pub struct TestCompositor(Option<yas_compositor::CompositorHandle>);
impl std::ops::Deref for TestCompositor {
    type Target = yas_compositor::CompositorHandle;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}
impl Drop for TestCompositor {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.stop();
        }
    }
}
