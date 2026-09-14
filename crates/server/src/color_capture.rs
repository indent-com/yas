//! Wide-gamut/HDR captures retain their precision and describe their pixels.
use super::CaptureEncoding;
use yas_compositor::color::{OutputColor, output_rgb};

pub(super) fn encode(
    pixels: &[f32],
    width: u32,
    height: u32,
    hdr: bool,
    format: CaptureEncoding,
    quality: u8,
) -> Option<Vec<u8>> {
    if width == 0
        || height == 0
        || pixels.len()
            != (width as usize)
                .checked_mul(height as usize)?
                .checked_mul(4)?
    {
        return None;
    }
    let color = if hdr {
        OutputColor::Hdr10
    } else {
        OutputColor::DisplayP3
    };
    let mut bytes = Vec::new();
    match format {
        CaptureEncoding::Png => {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().ok()?;
            let [primaries, transfer, _, _] = color.cicp();
            // PNG samples are RGB and full range, irrespective of the video
            // stream's matrix and limited quantization range.
            writer
                .write_chunk(
                    png::chunk::ChunkType(*b"cICP"),
                    &[primaries, transfer, 0, 1],
                )
                .ok()?;
            let rgb: Vec<u8> = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| {
                    output_rgb([p[0], p[1], p[2]], color, hdr)
                        .into_iter()
                        .flat_map(|v| ((v * 65535.0).round() as u16).to_be_bytes())
                })
                .collect();
            writer.write_image_data(&rgb).ok()?;
            writer.finish().ok()?;
        }
        CaptureEncoding::Avif => {
            use avif_serialize::{
                Aviffy,
                constants::{ColorPrimaries, MatrixCoefficients, TransferCharacteristics},
            };
            let av1 = super::surface_color_encoder::ColorEncoder::still(
                pixels, width, height, color, hdr, quality,
            )
            .ok()?;
            Aviffy::new()
                .set_width(width)
                .set_height(height)
                .set_bit_depth(12)
                .set_color_primaries(if hdr {
                    ColorPrimaries::Bt2020
                } else {
                    ColorPrimaries::DisplayP3
                })
                .set_transfer_characteristics(if hdr {
                    TransferCharacteristics::Smpte2084
                } else {
                    TransferCharacteristics::Srgb
                })
                .set_matrix_coefficients(if hdr {
                    MatrixCoefficients::Bt2020Ncl
                } else {
                    MatrixCoefficients::Bt709
                })
                .set_full_color_range(false)
                .write_slice(&mut bytes, &av1, None)
                .ok()?;
        }
    }
    Some(bytes)
}

pub(super) fn thumbnail(
    pixels: &[f32],
    width: u32,
    height: u32,
    hdr: bool,
    max_width: u32,
    max_height: u32,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || max_width == 0 || max_height == 0 {
        return None;
    }
    let scale = (max_width as f64 / width as f64)
        .min(max_height as f64 / height as f64)
        .min(1.0);
    let w = (width as f64 * scale).floor().max(1.0) as u32;
    let h = (height as f64 * scale).floor().max(1.0) as u32;
    let pixels = yas_compositor::color::resize_linear(pixels, width, height, w, h);
    encode(&pixels, w, h, hdr, CaptureEncoding::Png, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thumbnails_keep_hdr_precision_and_p3_primaries() {
        for hdr in [false, true] {
            let pixels = [1000.0 / 203.0, 1000.0 / 203.0, 1000.0 / 203.0, 1.0].repeat(40 * 20);
            let png = thumbnail(&pixels, 40, 20, hdr, 8, 8).unwrap();
            let reader = png::Decoder::new(std::io::Cursor::new(png))
                .read_info()
                .unwrap();
            let info = reader.info();
            assert_eq!((info.width, info.height), (8, 4));
            assert_eq!(info.bit_depth, png::BitDepth::Sixteen);
            let cicp = info.coding_independent_code_points.as_ref().unwrap();
            assert_eq!(cicp.color_primaries, if hdr { 9 } else { 12 });
            assert_eq!(cicp.transfer_function, if hdr { 16 } else { 13 });
        }
    }
    #[test]
    fn png_preserves_highlights_and_cicp() {
        let pixels = [1000.0 / 203.0, 1000.0 / 203.0, 1000.0 / 203.0, 1.0];
        let png = encode(&pixels, 1, 1, true, CaptureEncoding::Png, 0).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(png))
            .read_info()
            .unwrap();
        assert_eq!(reader.info().bit_depth, png::BitDepth::Sixteen);
        let cicp = reader.info().coding_independent_code_points.unwrap();
        assert_eq!(cicp.color_primaries, 9);
        assert_eq!(cicp.transfer_function, 16);
        assert_eq!(cicp.matrix_coefficients, 0);
        assert!(cicp.is_video_full_range_image);
        let mut buf = [0; 6];
        reader.next_frame(&mut buf).unwrap();
        let value = u16::from_be_bytes([buf[0], buf[1]]);
        assert!(value.abs_diff((0.7518271 * 65535.0) as u16) <= 1);
    }
    #[test]
    fn avif_retains_hdr_and_p3_metadata() {
        for hdr in [false, true] {
            let rgb = if hdr {
                [1000.0 / 203.0; 3]
            } else {
                yas_compositor::color::Primaries::DisplayP3
                    .to_bt2020()
                    .map(|r| r[0])
            };
            let pixels = [rgb[0], rgb[1], rgb[2], 1.0].repeat(64 * 64);
            let bytes = encode(&pixels, 64, 64, hdr, CaptureEncoding::Avif, 0).unwrap();
            let nclx = if hdr {
                [0, 9, 0, 16, 0, 9, 0]
            } else {
                [0, 12, 0, 13, 0, 1, 0]
            };
            let pos = bytes.windows(4).position(|v| v == b"nclx").unwrap();
            assert_eq!(&bytes[pos + 4..pos + 11], &nclx);
            if std::env::var_os("YAS_WRITE_COLOR_CAPTURES").is_some() {
                std::fs::write(format!("/tmp/yas-capture-{hdr}.avif"), &bytes).unwrap();
            }
        }
    }
}
