//! Exercise the real Wayland protocol, 10-bit SHM upload and Vulkan float readback.
#![cfg(target_os = "linux")]
use std::{
    os::{
        fd::AsFd,
        unix::{fs::FileExt, net::UnixStream},
    },
    sync::Arc,
    time::{Duration, Instant},
};
use wayland_client::Connection;
use wayland_client::protocol::wl_shm;
use wayland_protocols::wp::color_management::v1::client::wp_color_manager_v1 as cm;
use yas_compositor::{CompositorEvent, PixelData, spawn_compositor};
#[path = "support/color_client.rs"]
mod color_client;
use color_client::App;

#[test]
fn preferred_color_information_can_be_completed_and_requested_again() {
    let handle = spawn_compositor(false, Arc::new(|| {}), "");
    let conn = Connection::from_socket(UnixStream::connect(&handle.socket_name).unwrap()).unwrap();
    let mut queue = conn.new_event_queue();
    let q = queue.handle();
    conn.display().get_registry(&q, ());
    let mut app = App::default();
    queue.roundtrip(&mut app).unwrap();
    let manager = app.color.as_ref().expect("test requires a Vulkan renderer");
    let surface = app.compositor.as_ref().unwrap().create_surface(&q, ());
    let feedback = manager.get_surface_feedback(&surface, &q, ());
    let preferred = feedback.get_preferred(&q, ());
    let parametric = feedback.get_preferred_parametric(&q, ());
    queue.roundtrip(&mut app).unwrap();
    assert_eq!(app.ready, 2);

    // Chromium asks for the preferred description's information on startup.
    // Completing it destroys the new info object. Repeat after each batch so
    // the connection must survive both destruction and object-ID reuse.
    for batch in 1..=3 {
        for description in [&preferred, &parametric] {
            description.get_information(&q, ());
        }
        queue.roundtrip(&mut app).unwrap();
        queue.roundtrip(&mut app).unwrap();
        assert_eq!(app.information_done, batch * 2);
    }
    preferred.destroy();
    parametric.destroy();
    feedback.destroy();
    surface.destroy();
    queue.roundtrip(&mut app).unwrap();
    handle.stop();
}

#[test]
fn p3_and_pq_survive_composition_and_unset_restores_sdr() {
    let handle = spawn_compositor(false, Arc::new(|| {}), "");
    let conn = Connection::from_socket(UnixStream::connect(&handle.socket_name).unwrap()).unwrap();
    let mut queue = conn.new_event_queue();
    let q = queue.handle();
    conn.display().get_registry(&q, ());
    let mut app = App::default();
    queue.roundtrip(&mut app).unwrap();
    let Some(manager) = app.color.clone() else {
        handle.stop();
        panic!("test requires a Vulkan renderer (lavapipe is sufficient)");
    };
    let root = app.compositor.as_ref().unwrap().create_surface(&q, ());
    let xdg = app.wm.as_ref().unwrap().get_xdg_surface(&root, &q, ());
    let _top = xdg.get_toplevel(&q, ());
    root.commit();
    queue.roundtrip(&mut app).unwrap();
    let color = manager.get_surface(&root, &q, ());
    let path = std::env::temp_dir().join(format!("yas-color-test-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    std::fs::remove_file(path).unwrap();
    file.set_len(64 * 64 * 8).unwrap();
    let pool = app
        .shm
        .as_ref()
        .unwrap()
        .create_pool(file.as_fd(), 64 * 64 * 8, &q, ());
    let buffer = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Xrgb2101010, &q, ());
    for (primaries, tf, code, expected, hdr) in [
        (
            cm::Primaries::DisplayP3,
            cm::TransferFunction::Srgb,
            1023u32 << 20,
            [0.753833, 0.045744, -0.001210],
            false,
        ),
        (
            cm::Primaries::DciP3,
            cm::TransferFunction::St428,
            1023u32 << 20,
            [0.711783, 0.041615, -0.000845],
            false,
        ),
        (
            cm::Primaries::Bt2020,
            cm::TransferFunction::St2084Pq,
            (769u32 << 20) | (769 << 10) | 769,
            [4.927; 3],
            true,
        ),
    ] {
        let creator = manager.create_parametric_creator(&q, ());
        creator.set_primaries_named(primaries);
        creator.set_tf_named(tf);
        let image = creator.create(&q, ());
        queue.roundtrip(&mut app).unwrap();
        color.set_image_description(&image, cm::RenderIntent::Perceptual);
        file.write_all_at(&code.to_ne_bytes().repeat(64 * 64), 0)
            .unwrap();
        root.attach(Some(&buffer), 0, 0);
        root.damage_buffer(0, 0, 64, 64);
        root.commit();
        queue.roundtrip(&mut app).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "no matching managed frame");
            if let CompositorEvent::SurfaceCommit {
                pixels:
                    PixelData::LinearRgba {
                        data, hdr: actual, ..
                    },
                ..
            } = handle
                .event_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
            {
                if actual != hdr {
                    continue;
                }
                assert!(data[3] > 0.999, "XRGB padding must not become alpha");
                for i in 0..3 {
                    assert!(
                        (data[i] - expected[i]).abs() < 0.035,
                        "{primaries:?}/{tf:?}: {data:?}"
                    );
                }
                break;
            }
        }
        image.destroy();
    }
    // Exercise both channel orders, alpha conventions and 16-bit sample
    // types. scRGB values above one must survive SHM staging and composition.
    let scrgb = manager.create_windows_scrgb(&q, ());
    queue.roundtrip(&mut app).unwrap();
    for (index, format) in [
        wl_shm::Format::Abgr16161616f,
        wl_shm::Format::Argb16161616f,
        wl_shm::Format::Xbgr16161616f,
        wl_shm::Format::Xrgb16161616f,
        wl_shm::Format::Abgr16161616,
        wl_shm::Format::Argb16161616,
        wl_shm::Format::Xbgr16161616,
        wl_shm::Format::Xrgb16161616,
    ]
    .into_iter()
    .enumerate()
    {
        let floating = index < 4;
        let rgb = if floating {
            [2.0 + index as f32, 0.5, 0.25]
        } else {
            [index as f32 / 10.0, 0.25, 0.125]
        };
        let matrix = yas_compositor::color::Primaries::Srgb.to_bt2020();
        let expected =
            matrix.map(|r| (r[0] * rgb[0] + r[1] * rgb[1] + r[2] * rgb[2]) * 80.0 / 203.0);
        let channels = if index % 2 == 0 {
            rgb
        } else {
            [rgb[2], rgb[1], rgb[0]]
        };
        let alpha = if index % 4 < 2 { 1.0 } else { 0.0 };
        let raw: Vec<u8> = [channels[0], channels[1], channels[2], alpha]
            .into_iter()
            .flat_map(|v| {
                if floating {
                    half::f16::from_f32(v).to_bits().to_le_bytes()
                } else {
                    ((v * 65535.0).round() as u16).to_le_bytes()
                }
            })
            .collect();
        file.write_all_at(&raw.repeat(64 * 64), 0).unwrap();
        let buffer = pool.create_buffer(0, 64, 64, 64 * 8, format, &q, ());
        color.set_image_description(&scrgb, cm::RenderIntent::Perceptual);
        root.attach(Some(&buffer), 0, 0);
        root.damage_buffer(0, 0, 64, 64);
        root.commit();
        queue.roundtrip(&mut app).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "no correct {format:?} frame");
            if let CompositorEvent::SurfaceCommit {
                pixels: PixelData::LinearRgba { data, hdr, .. },
                ..
            } = handle
                .event_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                && hdr
                && data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|p| p[3] > 0.999 && (0..3).all(|i| (p[i] - expected[i]).abs() < 0.005))
            {
                break;
            }
        }
        buffer.destroy();
    }
    // Custom P3 coordinates, power transfer, and mastering metadata coexist.
    let creator = manager.create_parametric_creator(&q, ());
    creator.set_primaries(
        680000, 320000, 265000, 690000, 150000, 60000, 312700, 329000,
    );
    creator.set_tf_power(22000);
    creator.set_luminances(0, 80, 80);
    creator.set_mastering_display_primaries(
        680000, 320000, 265000, 690000, 150000, 60000, 312700, 329000,
    );
    creator.set_mastering_luminance(0, 1000);
    creator.set_max_cll(2000); // v2 permits light levels above the mastering range
    let custom = creator.create(&q, ());
    queue.roundtrip(&mut app).unwrap();
    color.set_image_description(&custom, cm::RenderIntent::Perceptual);
    let code = (512u32 << 20).to_le_bytes();
    file.write_all_at(&code.repeat(64 * 64), 0).unwrap();
    root.attach(Some(&buffer), 0, 0);
    root.damage_buffer(0, 0, 64, 64);
    root.commit();
    queue.roundtrip(&mut app).unwrap();
    let expected = yas_compositor::color::Primaries::DisplayP3
        .to_bt2020()
        .map(|r| r[0] * (512.0f32 / 1023.0).powf(2.2));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "no custom color frame");
        if let CompositorEvent::SurfaceCommit {
            pixels: PixelData::LinearRgba { data, hdr, .. },
            ..
        } = handle
            .event_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            && hdr
            && (0..3).all(|i| (data[i] - expected[i]).abs() < 0.005)
        {
            break;
        }
    }
    let icc_bytes = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let icc_offset = 64 * 64 * 8;
    file.set_len(icc_offset + 17 + icc_bytes.len() as u64)
        .unwrap();
    file.write_all_at(&icc_bytes, icc_offset + 17).unwrap();
    let creator = manager.create_icc_creator(&q, ());
    creator.set_icc_file(
        file.as_fd(),
        (icc_offset + 17) as u32,
        icc_bytes.len() as u32,
    );
    let icc = creator.create(&q, ());
    queue.roundtrip(&mut app).unwrap();
    // The buffer and profile share an fd at different offsets. Destroying
    // the image-description object after commit must not invalidate its LUT.
    color.set_image_description(&icc, cm::RenderIntent::RelativeBpc);
    file.write_all_at(&(1023u32 << 20).to_le_bytes().repeat(64 * 64), 0)
        .unwrap();
    root.attach(Some(&buffer), 0, 0);
    root.damage_buffer(0, 0, 64, 64);
    root.commit();
    icc.destroy();
    queue.roundtrip(&mut app).unwrap();
    let expected = yas_compositor::color::Primaries::DisplayP3
        .to_bt2020()
        .map(|r| r[0]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no ICC color frame");
        if let CompositorEvent::SurfaceCommit {
            pixels: PixelData::LinearRgba {
                data, hdr: false, ..
            },
            ..
        } = handle
            .event_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            && data
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| (0..3).all(|i| (p[i] - expected[i]).abs() < 0.003))
        {
            break;
        }
    }
    custom.destroy();
    scrgb.destroy();
    color.unset_image_description();
    root.commit();
    queue.roundtrip(&mut app).unwrap();
    let (sid, native) = loop {
        if let CompositorEvent::SurfaceCommit {
            surface_id,
            pixels: PixelData::Bgra(pixels),
            ..
        } = handle
            .event_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
        {
            break (surface_id, pixels);
        }
    };
    // A CPU subscriber can arrive after the image exists. Its first frame
    // must provision target-sized readback storage and deliver real pixels.
    handle
        .command_tx
        .send(yas_compositor::CompositorCommand::RegisterDownscaleTarget {
            surface_id: u32::from(sid),
            target_w: 32,
            target_h: 32,
            native_w: 64,
            native_h: 64,
            want_nv12_opaque: false,
            want_cpu_pixels: true,
            opaque_is_444: false,
            opaque_color: yas_compositor::color::OutputColor::Srgb,
        })
        .unwrap();
    handle.wake();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "no CPU downscale readback");
        if let CompositorEvent::SurfaceCommit {
            width: 32,
            height: 32,
            pixels: PixelData::Bgra(pixels),
            ..
        } = handle
            .event_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
        {
            assert_eq!(pixels.len(), 32 * 32 * 4);
            assert!(
                pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| pixel == &native[..4])
            );
            break;
        }
    }
    assert_eq!(app.ready, 6);
    handle.stop();
}

#[test]
#[ignore = "requires Vulkan Video hardware and FFmpeg; set YAS_COLOR_GPU for the render node"]
fn vulkan_video_color_pixels_and_metadata() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use yas_compositor::{CompositorCommand as Cmd, color::OutputColor};
    let gpu = std::env::var("YAS_COLOR_GPU").unwrap_or_default();
    let handle = spawn_compositor(false, Arc::new(|| {}), &gpu);
    let conn = Connection::from_socket(UnixStream::connect(&handle.socket_name).unwrap()).unwrap();
    let mut queue = conn.new_event_queue();
    let q = queue.handle();
    conn.display().get_registry(&q, ());
    let mut app = App::default();
    queue.roundtrip(&mut app).unwrap();
    let manager = app.color.as_ref().unwrap();
    let root = app.compositor.as_ref().unwrap().create_surface(&q, ());
    let xdg = app.wm.as_ref().unwrap().get_xdg_surface(&root, &q, ());
    let _top = xdg.get_toplevel(&q, ());
    root.commit();
    let color = manager.get_surface(&root, &q, ());
    let creator = manager.create_parametric_creator(&q, ());
    creator.set_primaries_named(cm::Primaries::Bt2020);
    creator.set_tf_named(cm::TransferFunction::St2084Pq);
    let description = creator.create(&q, ());
    queue.roundtrip(&mut app).unwrap();
    let path = std::env::temp_dir().join(format!("yas-vulkan-color-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    std::fs::remove_file(path).unwrap();
    file.set_len(256 * 256 * 4).unwrap();
    let code = (769u32 << 20) | (769 << 10) | 769;
    file.write_all_at(&code.to_le_bytes().repeat(256 * 256), 0)
        .unwrap();
    let pool = app
        .shm
        .as_ref()
        .unwrap()
        .create_pool(file.as_fd(), 256 * 256 * 4, &q, ());
    let buffer = pool.create_buffer(0, 256, 256, 256 * 4, wl_shm::Format::Xrgb2101010, &q, ());
    color.set_image_description(&description, cm::RenderIntent::Perceptual);
    root.attach(Some(&buffer), 0, 0);
    root.damage_buffer(0, 0, 256, 256);
    root.commit();
    queue.roundtrip(&mut app).unwrap();
    let sid = loop {
        if let CompositorEvent::SurfaceCommit {
            surface_id,
            pixels: PixelData::LinearRgba { .. },
            ..
        } = handle
            .event_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
        {
            break surface_id;
        }
    };
    let mut cases = vec![
        (2, OutputColor::Hdr10, false, 256),
        (2, OutputColor::Hdr10, false, 192),
        (2, OutputColor::DisplayP3, false, 256),
        (1, OutputColor::DisplayP3, false, 256),
    ];
    if std::env::var_os("YAS_COLOR_VULKAN_H264_444").is_some() {
        cases.push((1, OutputColor::DisplayP3, true, 256));
    }
    'cases: for (codec, output, is_444, width) in cases {
        if output == OutputColor::DisplayP3 {
            let creator = app
                .color
                .as_ref()
                .unwrap()
                .create_parametric_creator(&q, ());
            creator.set_primaries_named(cm::Primaries::DisplayP3);
            creator.set_tf_named(cm::TransferFunction::Srgb);
            let p3 = creator.create(&q, ());
            queue.roundtrip(&mut app).unwrap();
            color.set_image_description(&p3, cm::RenderIntent::Perceptual);
            file.write_all_at(&(1023u32 << 20).to_le_bytes().repeat(256 * 256), 0)
                .unwrap();
            root.attach(Some(&buffer), 0, 0);
            root.damage_buffer(0, 0, 256, 256);
            root.commit();
            queue.roundtrip(&mut app).unwrap();
            loop {
                if let CompositorEvent::SurfaceCommit {
                    pixels: PixelData::LinearRgba { hdr: false, .. },
                    ..
                } = handle
                    .event_rx
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                {
                    break;
                }
            }
            p3.destroy();
        }
        handle
            .command_tx
            .send(Cmd::SetVulkanEncoder {
                surface_id: sid as u32,
                client_id: 1,
                codec,
                qp: 18,
                width,
                height: width,
                native_w: 256,
                native_h: 256,
                is_444,
                output,
            })
            .unwrap();
        handle
            .command_tx
            .send(Cmd::Recomposite { surface_id: sid })
            .unwrap();
        handle.wake();
        let deadline = Instant::now() + Duration::from_secs(20);
        let data = loop {
            assert!(Instant::now() < deadline, "Vulkan Video timeout");
            let event = match handle.event_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    handle
                        .command_tx
                        .send(Cmd::RequestVulkanKeyframe {
                            surface_id: sid as u32,
                            client_id: 1,
                        })
                        .unwrap();
                    handle.wake();
                    continue;
                }
                Err(error) => panic!("compositor stopped: {error}"),
            };
            match event {
                CompositorEvent::SurfaceEncoded { frame, .. } if frame.client_id == 1 => {
                    assert!(frame.is_keyframe);
                    break frame.data;
                }
                CompositorEvent::VulkanEncoderUnavailable {
                    after_encode_failures,
                    ..
                } if is_444 => {
                    eprintln!(
                        "Vulkan H.264 4:4:4 refused (after encoding={after_encode_failures}); server fallback is required"
                    );
                    handle
                        .command_tx
                        .send(Cmd::DestroyVulkanEncoder {
                            surface_id: sid as u32,
                            client_id: Some(1),
                        })
                        .unwrap();
                    handle.wake();
                    continue 'cases;
                }
                CompositorEvent::VulkanEncoderUnavailable { .. } => panic!(
                    "Vulkan profile unavailable: codec={codec} output={output:?} 444={is_444}"
                ),
                _ => {}
            }
        };
        let mut data = data.as_ref().clone();
        // Reference frames use the same depth/profile and DPB lifetime as
        // the keyframe. Decode the complete GOP, not just its first frame.
        for _ in 0..2 {
            handle
                .command_tx
                .send(Cmd::RequestVulkanFrame {
                    surface_id: sid as u32,
                    client_id: 1,
                })
                .unwrap();
            handle
                .command_tx
                .send(Cmd::Recomposite { surface_id: sid })
                .unwrap();
            handle.wake();
            loop {
                match handle
                    .event_rx
                    .recv_timeout(Duration::from_secs(20))
                    .unwrap()
                {
                    CompositorEvent::SurfaceEncoded { frame, .. } if frame.client_id == 1 => {
                        assert!(!frame.is_keyframe);
                        data.extend_from_slice(&frame.data);
                        break;
                    }
                    CompositorEvent::VulkanEncoderUnavailable { .. } => {
                        panic!("Vulkan inter-frame failed")
                    }
                    _ => {}
                }
            }
        }
        let mut ffprobe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-f",
                if codec == 2 { "obu" } else { "h264" },
                "-show_entries",
                "stream=pix_fmt,color_primaries,color_transfer,color_space",
                "-of",
                "default=noprint_wrappers=1",
                "pipe:0",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        ffprobe.stdin.take().unwrap().write_all(&data).unwrap();
        let metadata = ffprobe.wait_with_output().unwrap();
        assert!(metadata.status.success());
        let metadata = String::from_utf8(metadata.stdout).unwrap();
        for expected in if output == OutputColor::Hdr10 {
            [
                "pix_fmt=yuv420p10le",
                "color_primaries=bt2020",
                "color_transfer=smpte2084",
                "color_space=bt2020nc",
            ]
        } else {
            [
                if is_444 {
                    "pix_fmt=yuv444p"
                } else {
                    "pix_fmt=yuv420p"
                },
                "color_primaries=smpte432",
                "color_transfer=iec61966-2-1",
                "color_space=bt709",
            ]
        } {
            assert!(
                metadata.contains(expected),
                "missing {expected}: {metadata}"
            );
        }
        let mut ffmpeg = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                if codec == 2 { "obu" } else { "h264" },
                "-i",
                "pipe:0",
                "-frames:v",
                "3",
                "-pix_fmt",
                if output == OutputColor::Hdr10 {
                    "yuv420p10le"
                } else if is_444 {
                    "yuv444p"
                } else {
                    "yuv420p"
                },
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        ffmpeg.stdin.take().unwrap().write_all(&data).unwrap();
        let decoded = ffmpeg.wait_with_output().unwrap();
        assert!(
            decoded.status.success(),
            "{}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        let frame_bytes = width as usize * width as usize * 3
            / if output == OutputColor::Hdr10 || is_444 {
                1
            } else {
                2
            };
        assert_eq!(
            decoded.stdout.len(),
            frame_bytes * 3,
            "all three frames must decode"
        );
        let actual = if output == OutputColor::Hdr10 {
            u16::from_le_bytes(decoded.stdout[..2].try_into().unwrap())
        } else {
            decoded.stdout[0] as u16
        };
        let expected = if output == OutputColor::Hdr10 {
            723
        } else {
            63
        };
        assert!(
            actual.abs_diff(expected) <= 3,
            "{output:?} codec={codec} {width}: Y={actual}, expected {expected}"
        );
        if let Ok(dir) = std::env::var("YAS_COLOR_PROBE_DIRECTORY") {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                format!(
                    "{dir}/vulkan-{codec}-{output:?}-{is_444}-{width}.{}",
                    if codec == 2 { "av1" } else { "h264" }
                ),
                &data,
            )
            .unwrap();
        }
        eprintln!("Vulkan Video {output:?} codec={codec} 444={is_444} {width}: Y={actual}");
        handle
            .command_tx
            .send(Cmd::DestroyVulkanEncoder {
                surface_id: sid as u32,
                client_id: Some(1),
            })
            .unwrap();
        handle.wake();
    }
    handle.stop();
}
