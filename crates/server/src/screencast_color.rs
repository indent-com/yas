//! Raw PipeWire formats. PQ samples are absolute; no implicit SDR-white
//! convention is needed by consumers of the 10-bit and float formats.
use yas_compositor::{
    PixelData,
    color::{OutputColor, Primaries, output_rgb},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum RawVideoFormat {
    #[default]
    Srgb = 0,
    DisplayP3 = 1,
    HdrXrgb10 = 2,
    HdrXbgr10 = 3,
    HdrFloat16 = 4,
}
impl RawVideoFormat {
    // An older RGBA consumer may omit color constraints entirely. Keep
    // its default sRGB; color-aware consumers can explicitly select P3/PQ.
    pub const SCREEN: [Self; 5] = [
        Self::Srgb,
        Self::HdrFloat16,
        Self::HdrXbgr10,
        Self::HdrXrgb10,
        Self::DisplayP3,
    ];
    pub fn from_id(id: u32) -> Self {
        match id {
            1 => Self::DisplayP3,
            2 => Self::HdrXrgb10,
            3 => Self::HdrXbgr10,
            4 => Self::HdrFloat16,
            _ => Self::Srgb,
        }
    }
    pub fn bytes_per_pixel(self) -> usize {
        if self == Self::HdrFloat16 { 8 } else { 4 }
    }
    /// SPA enum values, checked against PipeWire's public headers.
    pub fn spa_format(self) -> u32 {
        match self {
            Self::Srgb | Self::DisplayP3 => 11,
            Self::HdrFloat16 => 78,
            Self::HdrXrgb10 => 80,
            Self::HdrXbgr10 => 81,
        }
    }
    pub fn spa_color(self) -> [u32; 4] {
        // range, matrix, transfer function, primaries (SPA values, not CICP)
        match self {
            Self::Srgb => [1, 1, 7, 1],
            Self::DisplayP3 => [1, 1, 7, 11],
            _ => [1, 1, 14, 7],
        }
    }
    pub fn from_spa(format: u32, primaries: u32, transfer: u32) -> Option<Self> {
        Self::SCREEN
            .into_iter()
            .find(|f| {
                f.spa_format() == format
                    && (primaries == 0 || f.spa_color()[3] == primaries)
                    && (transfer == 0 || f.spa_color()[2] == transfer)
            })
            .map(|f| {
                if format == 11 && primaries == 0 {
                    Self::Srgb
                } else {
                    f
                }
            })
    }
    pub fn convert(self, pixels: &PixelData, width: u32, height: u32) -> Option<Vec<u8>> {
        let count = (width as usize).checked_mul(height as usize)?;
        if count == 0 {
            return None;
        }
        if self == Self::Srgb {
            let bytes = pixels.to_rgba(width, height);
            return (bytes.len() == count.checked_mul(4)?).then_some(bytes);
        }
        let owned;
        let (linear, hdr) = if let PixelData::LinearRgba {
            data,
            hdr,
            peak_nits,
        } = pixels
        {
            (
                &data[..],
                yas_compositor::color::ToneMapping {
                    hdr: *hdr,
                    peak_nits: *peak_nits,
                },
            )
        } else {
            let rgba = pixels.to_rgba(width, height);
            if rgba.len() != count.checked_mul(4)? {
                return None;
            }
            let matrix = Primaries::Srgb.to_bt2020();
            owned = rgba
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| {
                    let rgb = [p[0], p[1], p[2]].map(|v| {
                        let v = f32::from(v) / 255.0;
                        if v <= 0.04045 {
                            v / 12.92
                        } else {
                            ((v + 0.055) / 1.055).powf(2.4)
                        }
                    });
                    let rgb = matrix.map(|r| r[0] * rgb[0] + r[1] * rgb[1] + r[2] * rgb[2]);
                    [rgb[0], rgb[1], rgb[2], 1.0]
                })
                .collect::<Vec<_>>();
            (&owned[..], false.into())
        };
        if linear.len() != count.checked_mul(4)? {
            return None;
        }
        let output = if self == Self::DisplayP3 {
            OutputColor::DisplayP3
        } else {
            OutputColor::Hdr10
        };
        let mut bytes = Vec::with_capacity(count.checked_mul(self.bytes_per_pixel())?);
        for p in linear.as_chunks::<4>().0 {
            let rgb = output_rgb([p[0], p[1], p[2]], output, hdr);
            match self {
                Self::DisplayP3 => {
                    bytes.extend([rgb[0], rgb[1], rgb[2], 1.0].map(|v| (v * 255.0).round() as u8))
                }
                Self::HdrFloat16 => {
                    for v in [rgb[0], rgb[1], rgb[2], 1.0] {
                        bytes.extend_from_slice(&half::f16::from_f32(v).to_bits().to_le_bytes());
                    }
                }
                Self::HdrXrgb10 | Self::HdrXbgr10 => {
                    let [r, g, b] = rgb.map(|v| (v * 1023.0).round() as u32);
                    let word = if self == Self::HdrXrgb10 {
                        (r << 20) | (g << 10) | b
                    } else {
                        (b << 20) | (g << 10) | r
                    };
                    bytes.extend_from_slice(&(word | 0xc0000000).to_le_bytes());
                }
                Self::Srgb => unreachable!(),
            }
        }
        Some(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn hdr_formats_keep_1000_nit_highlights_and_sdr_maps_them() {
        let pixels = PixelData::LinearRgba {
            peak_nits: None,
            data: Arc::new(vec![1000.0 / 203.0, 1000.0 / 203.0, 1000.0 / 203.0, 1.0]),
            hdr: true,
        };
        for format in RawVideoFormat::SCREEN {
            let bytes = format.convert(&pixels, 1, 1).unwrap();
            assert_eq!(bytes.len(), format.bytes_per_pixel());
            match format {
                RawVideoFormat::HdrFloat16 => {
                    let value =
                        half::f16::from_bits(u16::from_le_bytes([bytes[0], bytes[1]])).to_f32();
                    assert!((value - 0.7518271).abs() < 0.001);
                }
                RawVideoFormat::HdrXrgb10 | RawVideoFormat::HdrXbgr10 => {
                    assert_eq!(u32::from_le_bytes(bytes.try_into().unwrap()) & 1023, 769)
                }
                _ => assert!(bytes[0] > 240),
            }
        }
    }
}
