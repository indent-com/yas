//! Real compositor → hardware encoder → independent FFmpeg decode checks.
#[path = "../../compositor/tests/support/color_client.rs"]
pub(crate) mod client;
use crate::surface_encoder::{
    ChromaSubsampling as C, SurfaceEncoder, SurfaceEncoderPreference as P, SurfaceEncoding,
};
use std::{
    io::Write,
    process::{Command, Stdio},
};
use yas_compositor::{CompositorCommand as Cmd, color::OutputColor as O};

#[test]
#[ignore = "requires NVIDIA NVENC or Intel VA-API hardware and FFmpeg; select YAS_COLOR_DIRECT_BACKEND=nvenc|vaapi"]
fn direct_gpu_color_pixels_and_metadata() {
    let backend = std::env::var("YAS_COLOR_DIRECT_BACKEND").unwrap_or_default();
    let vaapi = backend.starts_with("vaapi");
    let device = std::env::var("YAS_COLOR_GPU").unwrap_or_else(|_| {
        if vaapi {
            "/dev/dri/renderD128"
        } else {
            "/dev/dri/renderD129"
        }
        .into()
    });
    let cases = if backend == "vaapi-av1" {
        vec![
            (P::AV1Vaapi, O::Hdr10, C::Cs420),
            (P::AV1Vaapi, O::DisplayP3, C::Cs420),
            (P::AV1Vaapi, O::Hdr10, C::Cs444),
            (P::AV1Vaapi, O::DisplayP3, C::Cs444),
        ]
    } else if vaapi {
        vec![(P::H264Vaapi, O::DisplayP3, C::Cs420)]
    } else {
        vec![
            (P::NvencAV1, O::Hdr10, C::Cs420),
            (P::NvencAV1, O::DisplayP3, C::Cs420),
            (P::NvencH264, O::DisplayP3, C::Cs420),
            (P::NvencH264, O::DisplayP3, C::Cs444),
        ]
    };
    let mut cases: Vec<_> = cases
        .into_iter()
        .map(|(p, o, c)| (p, o, c, false))
        .collect();
    for color in [O::Srgb, O::DisplayP3] {
        cases.push((
            if vaapi { P::H264Vaapi } else { P::NvencH264 },
            color,
            C::Cs420,
            true,
        ));
    }
    for (pref, color, chroma, tone_map) in cases {
        let mut source = client::ColorClient::new(&device, if tone_map { O::Hdr10 } else { color });
        let mut previous = None;
        let mut resize_time = std::time::Duration::ZERO;
        let mut recreate_time = std::time::Duration::ZERO;
        for size in [256, 192, 256] {
            let create = |previous| {
                SurfaceEncoder::new_color_or_resize(
                    previous,
                    &[pref],
                    size,
                    size,
                    &device,
                    SurfaceEncoding::default(),
                    true,
                    if matches!(pref, P::NvencAV1 | P::AV1Vaapi) {
                        2
                    } else {
                        1
                    },
                    if color == O::Srgb {
                        0
                    } else if color == O::DisplayP3 {
                        25
                    } else {
                        31
                    },
                    color == O::Hdr10 || tone_map,
                    chroma,
                    false,
                )
                .unwrap()
            };
            let resizing = previous.is_some();
            let start = std::time::Instant::now();
            let mut encoder = create(previous.take());
            if resizing {
                resize_time += start.elapsed();
                let start = std::time::Instant::now();
                let fresh = create(None);
                recreate_time += start.elapsed();
                drop(fresh);
            }
            assert_eq!(encoder.preference(), pref, "hardware fallback");
            assert_eq!(encoder.output_color, color);
            let buffers = if vaapi {
                encoder.allocate_nv12_buffers(encoder.drm_fd_raw(), 5);
                assert!(
                    !encoder.gbm_nv12_buffers().is_empty(),
                    "VA-API exports unavailable"
                );
                encoder
                    .gbm_nv12_buffers()
                    .iter()
                    .map(|b| yas_compositor::ColorDmaBuffer {
                        fd: b.fd.clone(),
                        fourcc: b.fourcc,
                        modifier: b.modifier,
                        planes: b.planes.clone(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let (width, height) = encoder.encoder_dimensions();
            let command = Cmd::SetColorOutputTargets {
                surface_id: source.surface_id as u32,
                target_w: size,
                target_h: size,
                native_w: 256,
                native_h: 256,
                want_cpu_pixels: false,
                targets: vec![yas_compositor::ColorOutputTarget {
                    width,
                    height,
                    color,
                    is_444: chroma == C::Cs444,
                    buffers,
                }],
            };
            source.handle.command_tx.send(command).unwrap();
            source.handle.wake();
            let mut bytes = Vec::new();
            let mut packets = 0;
            for _ in 0..12 {
                source.repaint();
                let pixels = source.gpu_frame(size, vaapi);
                if let Some((packet, keyframe)) = encoder.encode_pixels(&pixels) {
                    if packets == 0 {
                        assert!(keyframe, "first frame after resize must be a keyframe");
                    }
                    bytes.extend(packet);
                    packets += 1;
                }
                if packets == 3 {
                    break;
                }
            }
            assert_eq!(packets, 3, "no hardware GOP");
            verify_stream(
                &bytes,
                size,
                color,
                chroma,
                tone_map,
                if matches!(pref, P::NvencAV1 | P::AV1Vaapi) {
                    2
                } else {
                    1
                },
            );
            let command = if vaapi {
                Cmd::SetExternalOutputBuffers {
                    surface_id: source.surface_id as u32,
                    target_w: size,
                    target_h: size,
                    native_w: 256,
                    native_h: 256,
                    buffers: Vec::new(),
                }
            } else {
                Cmd::ClearDownscaleTarget {
                    surface_id: source.surface_id as u32,
                    target_w: size,
                    target_h: size,
                }
            };
            source.handle.command_tx.send(command).unwrap();
            source.handle.wake();
            previous = Some(encoder);
        }
        eprintln!(
            "{pref:?} {color:?} {chroma:?} tone_map={tone_map}: resize setup {resize_time:?}; fresh setup {recreate_time:?} (2 sizes)"
        );
    }
}

fn verify_stream(bytes: &[u8], size: u32, color: O, chroma: C, tone_map: bool, codec: u8) {
    let format = if codec == 2 { "obu" } else { "h264" };
    let pixel_format = if color == O::Hdr10 && chroma == C::Cs444 {
        "yuv444p10le"
    } else if color == O::Hdr10 {
        "yuv420p10le"
    } else if chroma == C::Cs444 {
        "yuv444p"
    } else {
        "yuv420p"
    };
    let mut probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-f",
            format,
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
    probe.stdin.take().unwrap().write_all(bytes).unwrap();
    let info = probe.wait_with_output().unwrap();
    assert!(info.status.success());
    let info = String::from_utf8(info.stdout).unwrap();
    for expected in if color == O::Hdr10 {
        [pixel_format, "bt2020", "smpte2084", "bt2020nc"]
    } else if color == O::Srgb {
        [pixel_format, "bt709", "iec61966-2-1", "smpte170m"]
    } else {
        [pixel_format, "smpte432", "iec61966-2-1", "bt709"]
    } {
        assert!(info.contains(expected), "{expected} absent: {info}");
    }
    let mut decode = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            format,
            "-i",
            "pipe:0",
            "-frames:v",
            "3",
            "-pix_fmt",
            pixel_format,
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    decode.stdin.take().unwrap().write_all(bytes).unwrap();
    let raw = decode.wait_with_output().unwrap();
    assert!(raw.status.success());
    let samples = (size * size) as usize;
    let frame_samples = if chroma == C::Cs444 {
        samples * 3
    } else {
        samples * 3 / 2
    };
    let frame_bytes = frame_samples * if color == O::Hdr10 { 2 } else { 1 };
    assert_eq!(raw.stdout.len(), frame_bytes * 3);
    for frame in raw.stdout.chunks_exact(frame_bytes) {
        let (y, expected) = if color == O::Hdr10 {
            (
                u16::from_le_bytes([frame[samples], frame[samples + 1]]),
                723,
            )
        } else {
            (frame[samples / 2] as u16, if tone_map { 235 } else { 63 })
        };
        assert!(
            y.abs_diff(expected) <= 4,
            "{codec} {color:?} {size}: Y={y}, expected {expected}"
        );
    }
    eprintln!(
        "{codec} {color:?} {chroma:?} {size} tone_map={tone_map}: direct GPU encode/decode passed"
    );
    if let Ok(directory) = std::env::var("YAS_COLOR_PROBE_DIRECTORY") {
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            format!(
                "{directory}/direct-{codec}-{color:?}-{chroma:?}-{size}-tone{tone_map}.{format}"
            ),
            bytes,
        )
        .unwrap();
    }
}

#[test]
#[ignore = "requires NVENC or VA-API and FFmpeg; tests simultaneous color/chroma pools"]
fn direct_gpu_mixed_viewers() {
    use yas_compositor::{ColorDmaBuffer, ColorOutputTarget, CompositorEvent, PixelData};
    let vaapi = std::env::var("YAS_COLOR_DIRECT_BACKEND").is_ok_and(|v| v == "vaapi");
    let device = std::env::var("YAS_COLOR_GPU").unwrap_or_else(|_| {
        if vaapi {
            "/dev/dri/renderD128".into()
        } else {
            "/dev/dri/renderD129".into()
        }
    });
    for size in [256, 192] {
        let mut source = client::ColorClient::new(&device, O::Hdr10);
        let profiles = if vaapi {
            vec![
                (P::H264Vaapi, O::DisplayP3, C::Cs420),
                (P::H264Vaapi, O::Srgb, C::Cs420),
                (P::H264Vaapi, O::DisplayP3, C::Cs420),
            ]
        } else {
            vec![
                (P::NvencAV1, O::Hdr10, C::Cs420),
                (P::NvencH264, O::DisplayP3, C::Cs420),
                (P::NvencH264, O::DisplayP3, C::Cs444),
                (P::NvencH264, O::Srgb, C::Cs420),
            ]
        };
        let mut encoders = Vec::new();
        let mut targets = Vec::new();
        for &(pref, color, chroma) in &profiles {
            let mut encoder = SurfaceEncoder::new_color(
                &[pref],
                size,
                size,
                &device,
                SurfaceEncoding::default(),
                true,
                if pref == P::NvencAV1 { 2 } else { 1 },
                if color == O::Srgb { 0 } else { 31 },
                true,
                chroma,
            )
            .unwrap();
            assert_eq!(encoder.preference(), pref);
            if vaapi {
                encoder.allocate_nv12_buffers(encoder.drm_fd_raw(), 5);
            }
            let buffers = encoder
                .gbm_nv12_buffers()
                .iter()
                .map(|b| ColorDmaBuffer {
                    fd: b.fd.clone(),
                    fourcc: b.fourcc,
                    modifier: b.modifier,
                    planes: b.planes.clone(),
                })
                .collect();
            let (width, height) = encoder.encoder_dimensions();
            targets.push(ColorOutputTarget {
                width,
                height,
                color,
                is_444: chroma == C::Cs444,
                buffers,
            });
            encoders.push(encoder);
        }
        let install = |targets, want_cpu_pixels| {
            source
                .handle
                .command_tx
                .send(Cmd::SetColorOutputTargets {
                    surface_id: source.surface_id as u32,
                    target_w: size,
                    target_h: size,
                    native_w: 256,
                    native_h: 256,
                    targets,
                    want_cpu_pixels,
                })
                .unwrap();
            source.handle.wake();
        };
        // Identical NVENC requests share conversion; VA pools remain separate.
        let mut requested = targets.clone();
        if !vaapi {
            requested.push(targets[0].clone());
        }
        install(requested.clone(), false);
        source.repaint();
        // Commits are sorted by target size: a 192px GPU bundle can precede
        // a queued 256px startup readback in the same batch. Wait for native
        // GPU-only completion before measuring steady-state readbacks.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            assert!(std::time::Instant::now() < deadline);
            if matches!(
                source
                    .handle
                    .event_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap(),
                CompositorEvent::SurfaceCommit {
                    width: 256,
                    pixels: PixelData::GpuOnlyColor { .. },
                    ..
                }
            ) {
                break;
            }
        }
        let mut streams = vec![Vec::new(); encoders.len()];
        let mut frames = vec![0; encoders.len()];
        let mut ids = std::collections::HashSet::new();
        for frame in 0..10 {
            // Reinstall to check existing allocations and per-reader ownership survive.
            if frame == 3 {
                source
                    .handle
                    .command_tx
                    .send(Cmd::SetColorOutputTargets {
                        surface_id: source.surface_id as u32,
                        target_w: size,
                        target_h: size,
                        native_w: 256,
                        native_h: 256,
                        targets: requested.clone(),
                        want_cpu_pixels: false,
                    })
                    .unwrap();
                source.handle.wake();
            }
            source.repaint();
            let bundle = source.gpu_frame(size, vaapi);
            let PixelData::GpuVariants(variants) = &bundle else {
                panic!("expected profile bundle")
            };
            assert_eq!(variants.len(), profiles.len());
            for p in variants.iter() {
                if let PixelData::Nv12OpaqueFd { buf_id, .. } = p {
                    ids.insert(*buf_id);
                }
            }
            for (i, encoder) in encoders.iter_mut().enumerate() {
                if let Some((packet, _)) = encoder.encode_pixels(&bundle) {
                    streams[i].extend(packet);
                    frames[i] += 1;
                }
            }
            if frames.iter().all(|n| *n >= 6) {
                break;
            }
        }
        assert!(frames.iter().all(|n| *n >= 6));
        assert_eq!(
            source.cpu_frames_seen.get(),
            0,
            "GPU viewers caused CPU readback"
        );
        if !vaapi {
            assert_eq!(
                ids.len(),
                profiles.len() * 3,
                "reinstall replaced GPU allocations"
            );
        }
        for (i, &(pref, color, chroma)) in profiles.iter().enumerate() {
            // Decoder verification reads the first three frames of the GOP.
            verify_stream(
                &streams[i],
                size,
                color,
                chroma,
                color != O::Hdr10,
                if pref == P::NvencAV1 { 2 } else { 1 },
            );
        }
        // Removing a viewer must leave every other profile alive.
        targets.pop();
        targets.push(yas_compositor::ColorOutputTarget {
            width: size,
            height: size,
            color: O::Srgb,
            is_444: false,
            buffers: vec![yas_compositor::ColorDmaBuffer {
                fd: std::sync::Arc::new(std::fs::File::open("/dev/null").unwrap().into()),
                fourcc: 0,
                modifier: 0,
                planes: Vec::new(),
            }],
        });
        source
            .handle
            .command_tx
            .send(Cmd::SetColorOutputTargets {
                surface_id: source.surface_id as u32,
                target_w: size,
                target_h: size,
                native_w: 256,
                native_h: 256,
                targets,
                want_cpu_pixels: false,
            })
            .unwrap();
        source.handle.wake();
        source.repaint();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let (mut saw_cpu, mut saw_gpu) = (false, false);
        while !saw_cpu || !saw_gpu {
            assert!(std::time::Instant::now() < deadline);
            if let yas_compositor::CompositorEvent::SurfaceCommit { width, pixels, .. } = source
                .handle
                .event_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                && width == size
            {
                match pixels {
                    PixelData::LinearRgba { .. } => saw_cpu = true,
                    PixelData::GpuVariants(v) if v.len() == profiles.len() - 1 => saw_gpu = true,
                    _ => {}
                }
            }
        }
    }
}

#[test]
#[ignore = "requires Vulkan Video H.264 and AV1 plus FFmpeg; tests conflicting simultaneous profiles"]
fn direct_gpu_mixed_vulkan_viewers() {
    use yas_compositor::CompositorEvent as Event;
    let device = std::env::var("YAS_COLOR_GPU").unwrap_or_else(|_| "/dev/dri/renderD129".into());
    for size in [256, 192] {
        let source = client::ColorClient::new(&device, O::Hdr10);
        let profiles = [
            (2, O::Hdr10),
            (1, O::DisplayP3),
            (2, O::DisplayP3),
            (1, O::Srgb),
        ];
        for (i, &(codec, output)) in profiles.iter().enumerate() {
            source
                .handle
                .command_tx
                .send(Cmd::SetVulkanEncoder {
                    surface_id: source.surface_id as u32,
                    client_id: i as u64 + 1,
                    codec,
                    qp: 18,
                    width: size,
                    height: size,
                    native_w: 256,
                    native_h: 256,
                    is_444: false,
                    output,
                })
                .unwrap();
        }
        source
            .handle
            .command_tx
            .send(Cmd::Recomposite {
                surface_id: source.surface_id,
            })
            .unwrap();
        source.handle.wake();
        let mut streams = vec![Vec::new(); profiles.len()];
        let mut counts = vec![0; profiles.len()];
        let mut gpu_only = false;
        for round in 1..=3 {
            while counts.iter().any(|n| *n < round) {
                match source
                    .handle
                    .event_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|e| panic!("{e}: round {round}, counts {counts:?}"))
                {
                    Event::SurfaceEncoded { frame, .. } => {
                        let i = (frame.client_id - 1) as usize;
                        streams[i].extend_from_slice(&frame.data);
                        counts[i] += 1;
                    }
                    Event::VulkanEncoderUnavailable { .. } => panic!("Vulkan profile refused"),
                    Event::SurfaceCommit {
                        pixels: yas_compositor::PixelData::GpuOnlyColor { .. },
                        ..
                    } => gpu_only = true,
                    _ => {}
                }
            }
            for i in 0..profiles.len() {
                source
                    .handle
                    .command_tx
                    .send(Cmd::RequestVulkanFrame {
                        surface_id: source.surface_id as u32,
                        client_id: i as u64 + 1,
                    })
                    .unwrap();
            }
            source
                .handle
                .command_tx
                .send(Cmd::Recomposite {
                    surface_id: source.surface_id,
                })
                .unwrap();
            source.handle.wake();
        }
        assert!(
            gpu_only,
            "mixed Vulkan viewers unexpectedly require CPU pixels"
        );
        for (i, &(codec, color)) in profiles.iter().enumerate() {
            verify_stream(&streams[i], size, color, C::Cs420, color != O::Hdr10, codec);
        }
    }
}

#[test]
#[ignore = "requires NVENC or VA-API and FFmpeg; compares mastering-aware GPU output to CPU output"]
fn direct_gpu_mastering_tone_mapping() {
    use yas_compositor::{ColorDmaBuffer, ColorOutputTarget, PixelData};
    let vaapi = std::env::var("YAS_COLOR_DIRECT_BACKEND").is_ok_and(|v| v == "vaapi");
    let device = std::env::var("YAS_COLOR_GPU").unwrap_or_else(|_| {
        if vaapi {
            "/dev/dri/renderD128".into()
        } else {
            "/dev/dri/renderD129".into()
        }
    });
    for color in [O::Srgb, O::DisplayP3] {
        let mut source = client::ColorClient::with_mastering(&device, O::Hdr10, Some(1000), 203.0);
        let PixelData::LinearRgba {
            data,
            hdr,
            peak_nits,
        } = &source.initial_pixels
        else {
            unreachable!()
        };
        assert_eq!(*peak_nits, Some(1000.0));
        let tone = yas_compositor::color::ToneMapping {
            hdr: *hdr,
            peak_nits: *peak_nits,
        };
        let cpu = crate::color_yuv::ColorYuv::from_linear(
            data,
            (256, 256),
            (256, 256),
            color,
            tone,
            false,
        )
        .unwrap();
        let old = crate::color_yuv::ColorYuv::from_linear(
            data,
            (256, 256),
            (256, 256),
            color,
            *hdr,
            false,
        )
        .unwrap();
        assert!(cpu.planes[0][0].abs_diff(old.planes[0][0]) >= 2);
        let pref = if vaapi { P::H264Vaapi } else { P::NvencH264 };
        let mut encoder = SurfaceEncoder::new_color(
            &[pref],
            256,
            256,
            &device,
            SurfaceEncoding::default(),
            true,
            1,
            if color == O::Srgb { 0 } else { 31 },
            true,
            C::Cs420,
        )
        .unwrap();
        assert_eq!(encoder.preference(), pref);
        if vaapi {
            encoder.allocate_nv12_buffers(encoder.drm_fd_raw(), 3);
        }
        let buffers = encoder
            .gbm_nv12_buffers()
            .iter()
            .map(|b| ColorDmaBuffer {
                fd: b.fd.clone(),
                fourcc: b.fourcc,
                modifier: b.modifier,
                planes: b.planes.clone(),
            })
            .collect();
        source
            .handle
            .command_tx
            .send(Cmd::SetColorOutputTargets {
                surface_id: source.surface_id as u32,
                target_w: 256,
                target_h: 256,
                native_w: 256,
                native_h: 256,
                want_cpu_pixels: false,
                targets: vec![ColorOutputTarget {
                    width: 256,
                    height: 256,
                    color,
                    is_444: false,
                    buffers,
                }],
            })
            .unwrap();
        source.handle.wake();
        let mut bitstream = None;
        for _ in 0..6 {
            source.repaint();
            if let Some((bytes, _)) = encoder.encode_pixels(&source.gpu_frame(256, vaapi)) {
                bitstream = Some(bytes);
                break;
            }
        }
        let mut decode = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-frames:v",
                "1",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        decode
            .stdin
            .take()
            .unwrap()
            .write_all(&bitstream.unwrap())
            .unwrap();
        let raw = decode.wait_with_output().unwrap();
        assert!(raw.status.success());
        assert_eq!(raw.stdout.len(), 256 * 256 * 3 / 2);
        assert!(u16::from(raw.stdout[128 * 256 + 128]).abs_diff(cpu.planes[0][0]) <= 2);
        eprintln!(
            "mastering {color:?}: GPU Y={} CPU Y={}",
            raw.stdout[128 * 256 + 128],
            cpu.planes[0][0]
        );
    }
}
