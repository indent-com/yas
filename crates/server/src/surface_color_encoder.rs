//! Color-managed AV1. Uses u16 planes so PQ never passes through 8-bit RGB.
use super::color_yuv::ColorYuv;
use super::surface_encoder::ChromaSubsampling;
use super::surface_encoder::SurfaceEncoding;
use rav1e::prelude::*;
use yas_compositor::color::OutputColor;

pub(super) struct ColorEncoder {
    context: Context<u16>,
    width: usize,
    height: usize,
    pub output: OutputColor,
    chroma: ChromaSubsampling,
    bit_depth: u8,
    pub force_keyframe: bool,
}

impl ColorEncoder {
    pub fn new(
        width: u32,
        height: u32,
        encoding: SurfaceEncoding,
        output: OutputColor,
        chroma: ChromaSubsampling,
    ) -> Result<Self, String> {
        Self::new_inner(width, height, encoding, output, chroma, false)
    }

    fn new_inner(
        width: u32,
        height: u32,
        encoding: SurfaceEncoding,
        output: OutputColor,
        chroma: ChromaSubsampling,
        still: bool,
    ) -> Result<Self, String> {
        let hdr = output == OutputColor::Hdr10;
        let bit_depth: u8 = if still {
            12
        } else if hdr {
            10
        } else {
            8
        };
        let mut speed_settings = SpeedSettings::from_preset(encoding.speed.av1_speed());
        speed_settings.rdo_lookahead_frames = 1;
        let mut config = EncoderConfig {
            width: width as usize,
            height: height as usize,
            bit_depth: usize::from(bit_depth),
            chroma_sampling: if chroma.is_444() {
                ChromaSampling::Cs444
            } else {
                ChromaSampling::Cs420
            },
            chroma_sample_position: ChromaSamplePosition::Unknown,
            pixel_range: PixelRange::Limited,
            color_description: Some(ColorDescription {
                color_primaries: if hdr {
                    ColorPrimaries::BT2020
                } else if output == OutputColor::DisplayP3 {
                    ColorPrimaries::SMPTE432
                } else {
                    ColorPrimaries::BT709
                },
                transfer_characteristics: if hdr {
                    TransferCharacteristics::SMPTE2084
                } else {
                    TransferCharacteristics::SRGB
                },
                matrix_coefficients: if hdr {
                    MatrixCoefficients::BT2020NCL
                } else if output == OutputColor::DisplayP3 {
                    MatrixCoefficients::BT709
                } else {
                    MatrixCoefficients::BT601
                },
            }),
            speed_settings,
            low_latency: true,
            still_picture: still,
            quantizer: encoding.bandwidth.av1_quantizer(),
            min_quantizer: encoding.bandwidth.av1_min_quantizer(),
            ..Default::default()
        };
        config.set_key_frame_interval(0, 0);
        let context = Config::new()
            .with_encoder_config(config)
            .new_context()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            context,
            width: width as usize,
            height: height as usize,
            output,
            chroma,
            bit_depth,
            force_keyframe: true,
        })
    }

    pub fn still(
        pixels: &[f32],
        width: u32,
        height: u32,
        output: OutputColor,
        hdr: bool,
        quality: u8,
    ) -> Result<Vec<u8>, String> {
        let encoding = SurfaceEncoding {
            bandwidth: super::surface_encoder::SurfaceBandwidth::Custom {
                quantizer: if quality == 0 {
                    0
                } else {
                    ((100 - quality.min(100)) as u16 * 255 / 100) as u8
                },
            },
            speed: super::surface_encoder::SurfaceSpeed::Medium,
        };
        let mut encoder = Self::new_inner(
            width,
            height,
            encoding,
            output,
            ChromaSubsampling::Cs444,
            true,
        )?;
        if let Some((data, _)) = encoder.encode(pixels, hdr) {
            return Ok(data);
        }
        encoder.context.flush();
        loop {
            match encoder.context.receive_packet() {
                Ok(packet) => return Ok(packet.data),
                Err(EncoderStatus::Encoded) => continue,
                Err(status) => return Err(format!("AVIF encode: {status}")),
            }
        }
    }

    pub fn encode(
        &mut self,
        pixels: &[f32],
        hdr_source: impl Into<yas_compositor::color::ToneMapping>,
    ) -> Option<(Vec<u8>, bool)> {
        let yuv = ColorYuv::from_linear_depth(
            pixels,
            (self.width, self.height),
            (self.width, self.height),
            self.output,
            hdr_source,
            self.chroma.is_444(),
            self.bit_depth,
        )?;
        let mut frame = self.context.new_frame();
        for (i, plane) in yuv.planes.iter().enumerate() {
            let w = if i == 0 || self.chroma.is_444() {
                self.width
            } else {
                self.width.div_ceil(2)
            };
            let stride = frame.planes[i].cfg.stride;
            for (y, row) in plane.chunks_exact(w).enumerate() {
                frame.planes[i].data_origin_mut()[y * stride..y * stride + w].copy_from_slice(row);
            }
        }
        let parameters = FrameParameters {
            frame_type_override: if self.force_keyframe {
                FrameTypeOverride::Key
            } else {
                FrameTypeOverride::No
            },
            ..Default::default()
        };
        self.context.send_frame((frame, parameters)).ok()?;
        self.force_keyframe = false;
        match self.context.receive_packet() {
            Ok(packet) => Some((packet.data, packet.frame_type == FrameType::KEY)),
            _ => None,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::super::surface_encoder::SurfaceEncoder;
    use super::*;
    use rav1d::include::dav1d::{data::Dav1dData, dav1d::Dav1dSettings, picture::Dav1dPicture};
    use rav1d::src::lib::{
        dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings,
        dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data,
    };
    use std::{
        mem::MaybeUninit,
        ptr::{self, NonNull},
    };

    // A real decoder checks both the independently parsed CICP and the pixel
    // values. This catches padded plane origins and accidental 8-bit staging.
    fn decode_check(bytes: &[u8], output: OutputColor, expected_y: u16, chroma: ChromaSubsampling) {
        // SAFETY: Every FFI input is owned initialized storage; plane reads
        // stay inside the returned 64x64 picture, before its single unref.
        unsafe {
            let mut settings = MaybeUninit::<Dav1dSettings>::zeroed();
            dav1d_default_settings(NonNull::new(settings.as_mut_ptr()).unwrap());
            let mut settings = settings.assume_init();
            settings.n_threads = 1;
            settings.max_frame_delay = 1;
            let mut context = None;
            assert_eq!(
                dav1d_open(
                    Some(NonNull::from(&mut context)),
                    Some(NonNull::from(&mut settings))
                )
                .0,
                0
            );
            let mut data = Dav1dData::default();
            let dst = dav1d_data_create(Some(NonNull::from(&mut data)), bytes.len());
            assert!(!dst.is_null());
            ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            assert_eq!(
                dav1d_send_data(context, Some(NonNull::from(&mut data))).0,
                0
            );
            dav1d_data_unref(Some(NonNull::from(&mut data)));
            let mut picture = Dav1dPicture::default();
            assert_eq!(
                dav1d_get_picture(context, Some(NonNull::from(&mut picture))).0,
                0
            );
            assert_eq!((picture.p.w, picture.p.h), (64, 64));
            assert_eq!(
                picture.p.bpc,
                if output == OutputColor::Hdr10 { 10 } else { 8 }
            );
            let seq = picture.seq_hdr.unwrap().as_ref();
            assert_eq!(seq.profile, if chroma.is_444() { 1 } else { 0 });
            assert_eq!(
                [
                    seq.pri as u8,
                    seq.trc as u8,
                    seq.mtrx as u8,
                    seq.color_range
                ],
                output.cicp()
            );
            let plane = picture.data[0].unwrap().as_ptr().cast::<u8>();
            for (x, y) in [(0, 0), (32, 32), (63, 63)] {
                let address = plane.add(y * picture.stride[0] as usize);
                let v = if picture.p.bpc == 10 {
                    *address.cast::<u16>().add(x)
                } else {
                    *address.add(x) as u16
                };
                assert!(
                    v.abs_diff(expected_y) <= 4,
                    "{output:?} decoded Y {v}, expected {expected_y}"
                );
            }
            dav1d_picture_unref(Some(NonNull::from(&mut picture)));
            dav1d_close(Some(NonNull::from(&mut context)));
        }
    }
    #[test]
    fn color_capabilities_are_per_view_and_hdr_requires_av1() {
        const P3: u8 = yas_wire::schema::surface::COLOR_CAP_DISPLAY_P3 as u8;
        const HDR: u8 = yas_wire::schema::surface::COLOR_CAP_HDR10_AV1 as u8;
        assert_eq!(
            SurfaceEncoder::negotiate_color(2, 0, true),
            OutputColor::Srgb
        );
        assert_eq!(
            SurfaceEncoder::negotiate_color(1, P3 | HDR, true),
            OutputColor::DisplayP3
        );
        assert_eq!(
            SurfaceEncoder::negotiate_color(2, P3, true),
            OutputColor::DisplayP3
        );
        assert_eq!(
            SurfaceEncoder::negotiate_color(2, P3 | HDR, true),
            OutputColor::Hdr10
        );
        assert_eq!(
            SurfaceEncoder::negotiate_color(2, P3 | HDR, false),
            OutputColor::DisplayP3
        );
    }
    #[test]
    fn native_color_capabilities_select_chroma_without_legacy_codec_bits() {
        use super::super::surface_encoder::SurfaceEncoderPreference as P;
        for (caps, hdr, profile, depth) in [
            (1, false, 0, 8),
            (9, false, 1, 8),
            (3, true, 0, 10),
            (7, true, 1, 10),
        ] {
            let encoder = SurfaceEncoder::new_color(
                &[P::AV1Software],
                64,
                64,
                "",
                SurfaceEncoding::default(),
                false,
                2,
                caps,
                hdr,
                ChromaSubsampling::Cs444,
            )
            .unwrap();
            let codec = encoder.webcodecs_codec_string();
            assert!(codec.starts_with(&format!("av01.{profile}.")), "{codec}");
            assert!(codec.ends_with(&format!(".{depth:02}")), "{codec}");
        }
    }

    #[test]
    fn p3_and_hdr_bitstreams_preserve_color_and_precision() {
        for chroma in [ChromaSubsampling::Cs420, ChromaSubsampling::Cs444] {
            for (output, rgb, expected_y) in [
                (
                    OutputColor::DisplayP3,
                    yas_compositor::color::Primaries::DisplayP3
                        .to_bt2020()
                        .map(|row| row[0]),
                    63,
                ),
                (OutputColor::Hdr10, [1000.0 / 203.0; 3], 723),
            ] {
                let mut encoder =
                    ColorEncoder::new(64, 64, SurfaceEncoding::default(), output, chroma).unwrap();
                let pixels = [rgb[0], rgb[1], rgb[2], 1.0].repeat(64 * 64);
                let mut packet = None;
                for _ in 0..8 {
                    packet = encoder.encode(&pixels, output == OutputColor::Hdr10);
                    if packet.is_some() {
                        break;
                    }
                }
                let (packet, key) = packet.expect("encoder must produce a frame");
                assert!(key);
                decode_check(&packet, output, expected_y, chroma);
                if output == OutputColor::Hdr10
                    && chroma == ChromaSubsampling::Cs420
                    && let Some(path) = std::env::var_os("YAS_WRITE_HDR_PROBE")
                {
                    std::fs::write(path, &packet).unwrap();
                }
            }
        }
    }

    fn packet_for(
        pref: super::super::surface_encoder::SurfaceEncoderPreference,
        output: OutputColor,
        device: &str,
        size: usize,
        chroma: ChromaSubsampling,
    ) -> Result<Vec<u8>, String> {
        let caps = 31;
        let codec = if matches!(
            pref,
            super::super::surface_encoder::SurfaceEncoderPreference::H264Software
                | super::super::surface_encoder::SurfaceEncoderPreference::NvencH264
                | super::super::surface_encoder::SurfaceEncoderPreference::H264Vaapi
        ) {
            1
        } else {
            2
        };
        let mut encoder = SurfaceEncoder::new_color(
            &[pref],
            size as u32,
            size as u32,
            device,
            SurfaceEncoding::default(),
            true,
            codec,
            caps,
            output == OutputColor::Hdr10,
            chroma,
        )?;
        if encoder.preference() != pref {
            return Err(format!(
                "{pref:?} unavailable; selected {:?}",
                encoder.preference()
            ));
        }
        let rgb = if output == OutputColor::Hdr10 {
            [1000.0 / 203.0; 3]
        } else {
            yas_compositor::color::Primaries::DisplayP3
                .to_bt2020()
                .map(|row| row[0])
        };
        let pixels = yas_compositor::PixelData::LinearRgba {
            peak_nits: None,
            data: std::sync::Arc::new([rgb[0], rgb[1], rgb[2], 1.0].repeat(size * size)),
            hdr: output == OutputColor::Hdr10,
        };
        for _ in 0..12 {
            if let Some((bytes, _)) = encoder.encode_pixels(&pixels) {
                return Ok(bytes);
            }
        }
        Err("encoder produced no packet".into())
    }

    #[test]
    #[cfg(feature = "openh264")]
    fn h264_p3_preserves_pixels_through_sps_rewrite() {
        use openh264::formats::YUVSource;
        let bytes = packet_for(
            super::super::surface_encoder::SurfaceEncoderPreference::H264Software,
            OutputColor::DisplayP3,
            "",
            64,
            ChromaSubsampling::Cs420,
        )
        .unwrap();
        let mut decoder = openh264::decoder::Decoder::new().unwrap();
        let picture = decoder.decode(&bytes).unwrap().unwrap();
        assert_eq!(picture.dimensions(), (64, 64));
        assert!(picture.y()[32 * picture.strides().0 + 32].abs_diff(63) < 4);
        assert_eq!(
            crate::h264_color::set_color(&bytes, OutputColor::DisplayP3.cicp()).unwrap(),
            bytes
        );
        if let Ok(path) = std::env::var("YAS_WRITE_P3_H264_PROBE") {
            std::fs::write(path, bytes).unwrap();
        }
    }

    #[test]
    #[ignore = "requires NVIDIA GPU, VA-API device, and ffprobe"]
    fn managed_hardware_bitstreams() {
        use super::super::surface_encoder::SurfaceEncoderPreference as P;
        use std::io::Write;
        for (pref, color, device, chroma) in [
            (
                P::NvencAV1,
                OutputColor::Hdr10,
                "",
                ChromaSubsampling::Cs420,
            ),
            (
                P::NvencAV1,
                OutputColor::DisplayP3,
                "",
                ChromaSubsampling::Cs420,
            ),
            (
                P::NvencH264,
                OutputColor::DisplayP3,
                "",
                ChromaSubsampling::Cs420,
            ),
            (
                P::NvencH264,
                OutputColor::DisplayP3,
                "",
                ChromaSubsampling::Cs444,
            ),
            (
                P::H264Vaapi,
                OutputColor::DisplayP3,
                "/dev/dri/renderD128",
                ChromaSubsampling::Cs420,
            ),
        ] {
            let bytes = packet_for(pref, color, device, 512, chroma).unwrap();
            let path = format!("/tmp/yas-managed-{pref:?}-{color:?}-{chroma:?}.bin");
            std::fs::write(&path, &bytes).unwrap();
            let mut child = std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=color_primaries,color_transfer,color_space,pix_fmt",
                    "-of",
                    "json",
                    "-i",
                    "pipe:0",
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&bytes).unwrap();
            let result = child.wait_with_output().unwrap();
            assert!(result.status.success());
            let info = String::from_utf8(result.stdout).unwrap();
            eprintln!("{pref:?}: {info}");
            assert!(info.contains(if color == OutputColor::Hdr10 {
                "bt2020"
            } else {
                "smpte432"
            }));
            assert!(info.contains(if color == OutputColor::Hdr10 {
                "smpte2084"
            } else {
                "iec61966-2-1"
            }));
            assert!(info.contains(if chroma.is_444() {
                "yuv444p"
            } else if color == OutputColor::Hdr10 {
                "yuv420p10"
            } else {
                "yuv420p"
            }));
        }
    }
}
