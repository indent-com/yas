//! One color conversion for software and hardware encoders. Samples are
//! right-aligned, limited-range YUV; hardware packing happens at the boundary.
use yas_compositor::color::{OutputColor, output_rgb};

pub(crate) struct ColorYuv {
    pub planes: [Vec<u16>; 3],
    pub width: usize,
    pub height: usize,
    pub is_444: bool,
    pub bit_depth: u8,
}

impl ColorYuv {
    pub fn from_linear(
        pixels: &[f32],
        source: (usize, usize),
        size: (usize, usize),
        output: OutputColor,
        hdr_source: impl Into<yas_compositor::color::ToneMapping>,
        is_444: bool,
    ) -> Option<Self> {
        Self::from_linear_depth(
            pixels,
            source,
            size,
            output,
            hdr_source,
            is_444,
            if output == OutputColor::Hdr10 { 10 } else { 8 },
        )
    }

    pub fn from_linear_depth(
        pixels: &[f32],
        source: (usize, usize),
        size: (usize, usize),
        output: OutputColor,
        hdr_source: impl Into<yas_compositor::color::ToneMapping>,
        is_444: bool,
        bit_depth: u8,
    ) -> Option<Self> {
        let hdr_source = hdr_source.into();
        if ![8, 10, 12].contains(&bit_depth) {
            return None;
        }
        let (sw, sh) = source;
        let (w, h) = size;
        if sw == 0
            || sh == 0
            || w < sw
            || h < sh
            || pixels.len() != sw.checked_mul(sh)?.checked_mul(4)?
        {
            return None;
        }
        let scale = (1u32 << (bit_depth - 8)) as f32;
        let (kr, kb) = match output {
            OutputColor::Srgb => (0.299, 0.114),
            OutputColor::DisplayP3 => (0.2126, 0.0722),
            OutputColor::Hdr10 => (0.2627, 0.0593),
        };
        let sample = |x: usize, y: usize| {
            let i = (y.min(sh - 1) * sw + x.min(sw - 1)) * 4;
            let c = output_rgb(
                [pixels[i], pixels[i + 1], pixels[i + 2]],
                output,
                hdr_source,
            );
            let luma = kr * c[0] + (1.0 - kr - kb) * c[1] + kb * c[2];
            [
                luma,
                (c[2] - luma) / (2.0 * (1.0 - kb)),
                (c[0] - luma) / (2.0 * (1.0 - kr)),
            ]
        };
        let step = if is_444 { 1 } else { 2 };
        let (cw, ch) = (w.div_ceil(step), h.div_ceil(step));
        let mut planes = [
            vec![0; w.checked_mul(h)?],
            vec![0; cw.checked_mul(ch)?],
            vec![0; cw * ch],
        ];
        for y in 0..ch {
            for x in 0..cw {
                let mut chroma = [0.0; 2];
                for dy in 0..step {
                    for dx in 0..step {
                        let (px, py) = (x * step + dx, y * step + dy);
                        let c = sample(px, py);
                        if px < w && py < h {
                            planes[0][py * w + px] = ((16.0 + 219.0 * c[0]) * scale).round() as u16;
                        }
                        chroma[0] += c[1] / (step * step) as f32;
                        chroma[1] += c[2] / (step * step) as f32;
                    }
                }
                for (i, c) in chroma.into_iter().enumerate() {
                    planes[i + 1][y * cw + x] = ((128.0 + 224.0 * c) * scale).round() as u16;
                }
            }
        }
        Some(Self {
            planes,
            width: w,
            height: h,
            is_444,
            bit_depth,
        })
    }

    pub fn planar8(&self) -> Vec<u8> {
        debug_assert_eq!(self.bit_depth, 8);
        self.planes.iter().flatten().map(|&v| v as u8).collect()
    }

    /// NV12/P010, or planar YUV444/YUV444_10BIT. Ten-bit samples occupy
    /// the high bits of little-endian words, as required by NVENC/libva.
    pub fn hardware_bytes(&self) -> Vec<u8> {
        let sample_count: usize = self.planes.iter().map(Vec::len).sum();
        let mut bytes = Vec::with_capacity(sample_count * if self.bit_depth > 8 { 2 } else { 1 });
        let mut append = |v: u16| {
            if self.bit_depth > 8 {
                bytes.extend_from_slice(&(v << (16 - self.bit_depth)).to_le_bytes());
            } else {
                bytes.push(v as u8);
            }
        };
        for &y in &self.planes[0] {
            append(y);
        }
        if self.is_444 {
            for &v in self.planes[1..].iter().flatten() {
                append(v);
            }
        } else {
            for (&u, &v) in self.planes[1].iter().zip(&self.planes[2]) {
                append(u);
                append(v);
            }
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_sizes_replicate_edges_and_average_chroma() {
        let red = yas_compositor::color::Primaries::DisplayP3
            .to_bt2020()
            .map(|row| row[0]);
        let blue = yas_compositor::color::Primaries::DisplayP3
            .to_bt2020()
            .map(|row| row[2]);
        let mut pixels = Vec::new();
        for rgb in [red, blue, red, blue, red, blue, red, blue, red] {
            pixels.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 1.0]);
        }
        let full =
            ColorYuv::from_linear(&pixels, (3, 3), (4, 4), OutputColor::DisplayP3, false, true)
                .unwrap();
        let odd = ColorYuv::from_linear(
            &pixels,
            (3, 3),
            (3, 3),
            OutputColor::DisplayP3,
            false,
            false,
        )
        .unwrap();
        let padded = ColorYuv::from_linear(
            &pixels,
            (3, 3),
            (4, 4),
            OutputColor::DisplayP3,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            odd.planes.iter().map(Vec::len).collect::<Vec<_>>(),
            [9, 4, 4]
        );
        for y in 0..3 {
            assert_eq!(
                &odd.planes[0][y * 3..y * 3 + 3],
                &full.planes[0][y * 4..y * 4 + 3]
            );
        }
        assert_eq!(padded.planes[0], full.planes[0]);
        for channel in 1..3 {
            assert_eq!(odd.planes[channel], padded.planes[channel]);
            for y in 0..2 {
                for x in 0..2 {
                    let i = y * 8 + x * 2;
                    let average = [i, i + 1, i + 4, i + 5]
                        .map(|i| u32::from(full.planes[channel][i]))
                        .iter()
                        .sum::<u32>() as f32
                        / 4.0;
                    assert!((odd.planes[channel][y * 2 + x] as f32 - average).abs() <= 1.0);
                }
            }
        }
    }

    #[test]
    fn hardware_packing_preserves_ten_bit_samples_and_chroma_order() {
        let mut yuv = ColorYuv {
            planes: [vec![64, 723, 940, 65], vec![512], vec![513]],
            width: 2,
            height: 2,
            is_444: false,
            bit_depth: 10,
        };
        let words = |bytes: Vec<u8>| {
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            words(yuv.hardware_bytes()),
            [64, 723, 940, 65, 512, 513].map(|v| v << 6)
        );
        yuv.is_444 = true;
        yuv.planes[1] = vec![501, 502, 503, 504];
        yuv.planes[2] = vec![601, 602, 603, 604];
        assert_eq!(
            words(yuv.hardware_bytes()),
            [64, 723, 940, 65, 501, 502, 503, 504, 601, 602, 603, 604].map(|v| v << 6)
        );
    }
}
