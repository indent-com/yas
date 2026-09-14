//! Bounded ICC v2/v4 parsing and GPU lookup tables. Parsing and transform
//! construction use moxcms; no client-supplied code or native CMS is loaded.
use crate::color::{ColorLut, ImageDescription, Intent, Primaries, Transfer};
use moxcms::{
    ColorProfile, Layout, ParsingOptions, RenderingIntent, ToneReprCurve, TransformOptions,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

pub(super) struct IccProfile {
    profile: ColorProfile,
    luts: Mutex<std::collections::HashMap<Intent, Arc<ColorLut>>>,
}

impl IccProfile {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if !matches!(bytes.get(8), Some(2 | 4))
            || !matches!(bytes.get(12..16), Some(b"mntr" | b"spac"))
        {
            return None;
        }
        let profile = ColorProfile::new_from_slice_with_options(
            bytes,
            ParsingOptions {
                max_profile_size: 32 * 1024 * 1024,
                max_allowed_clut_size: 16 * 1024 * 1024,
                max_allowed_trc_size: 65536,
            },
        )
        .ok()?;
        // Wayland buffers have three color channels. Other valid ICC input
        // spaces are rejected by transform construction if unsupported.
        if !matches!(bytes.get(16..20), Some(b"RGB " | b"XYZ " | b"Lab ")) {
            return None;
        }
        Some(Self {
            profile,
            luts: Mutex::new(Default::default()),
        })
    }

    pub fn description(&self, intent: Intent) -> Option<ImageDescription> {
        let hdr_peak = self
            .profile
            .cicp
            .as_ref()
            .and_then(|c| match c.transfer_characteristics {
                moxcms::TransferCharacteristics::Smpte2084 => Some(10000.0f32),
                moxcms::TransferCharacteristics::Hlg => Some(1000.0f32),
                _ => None,
            });
        let mut cache = self.luts.lock().ok()?;
        let lut = if let Some(lut) = cache.get(&intent) {
            lut.clone()
        } else {
            let mut target = ColorProfile::new_bt2020();
            target.red_trc = Some(ToneReprCurve::Lut(vec![]));
            target.green_trc = target.red_trc.clone();
            target.blue_trc = target.red_trc.clone();
            let transform = self
                .profile
                .create_transform_f32(
                    Layout::Rgb,
                    &target,
                    Layout::Rgb,
                    TransformOptions {
                        rendering_intent: match intent {
                            Intent::Perceptual => RenderingIntent::Perceptual,
                            Intent::Relative | Intent::RelativeBpc => {
                                RenderingIntent::RelativeColorimetric
                            }
                            Intent::Absolute => RenderingIntent::AbsoluteColorimetric,
                            Intent::Saturation => RenderingIntent::Saturation,
                        },
                        ..Default::default()
                    },
                )
                .ok()?;
            const SIZE: usize = 65;
            let mut input = Vec::with_capacity(SIZE * SIZE * SIZE * 3);
            for b in 0..SIZE {
                for g in 0..SIZE {
                    for r in 0..SIZE {
                        input.extend([r, g, b].map(|v| v as f32 / (SIZE - 1) as f32));
                    }
                }
            }
            let mut output = vec![0.0; input.len()];
            transform.transform(&input, &mut output).ok()?;
            if output.iter().any(|v| !v.is_finite() || v.abs() > 65504.0) {
                return None;
            }
            if intent == Intent::RelativeBpc {
                let black = [output[0], output[1], output[2]];
                let white = [
                    output[output.len() - 3],
                    output[output.len() - 2],
                    output[output.len() - 1],
                ];
                for p in output.as_chunks_mut::<3>().0 {
                    for i in 0..3 {
                        p[i] = (p[i] - black[i]) / (white[i] - black[i]).max(0.00001);
                    }
                }
            }
            let scale = hdr_peak.map(|peak| peak / 203.0).unwrap_or_else(|| {
                if intent == Intent::Absolute {
                    self.profile
                        .luminance
                        .map(|v| v.y as f32)
                        .filter(|v| v.is_finite() && *v > 0.0 && *v <= 10000.0)
                        .unwrap_or(80.0)
                        / 203.0
                } else {
                    1.0
                }
            });
            for value in &mut output {
                *value *= scale;
            }
            if output.iter().any(|v| !v.is_finite() || v.abs() > 65504.0) {
                return None;
            }
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let lut = Arc::new(ColorLut {
                id: NEXT.fetch_add(1, Ordering::Relaxed),
                size: SIZE as u32,
                pixels: output
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .flat_map(|p| [p[0], p[1], p[2], 1.0])
                    .collect(),
            });
            cache.insert(intent, lut.clone());
            lut
        };
        Some(ImageDescription {
            primaries: Primaries::Bt2020,
            transfer: Transfer::Srgb,
            intent,
            lut: Some(lut),
            reference_nits: if hdr_peak.is_some() { 203.0 } else { 80.0 },
            peak_nits: hdr_peak.unwrap_or(80.0),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn icc_p3_luts_preserve_primaries_and_cache_each_intent() {
        let bytes = ColorProfile::new_display_p3().encode().unwrap();
        let profile = IccProfile::parse(&bytes).unwrap();
        for intent in [
            Intent::Perceptual,
            Intent::Relative,
            Intent::RelativeBpc,
            Intent::Saturation,
        ] {
            let desc = profile.description(intent).unwrap();
            let lut = desc.lut.unwrap();
            let red = &lut.pixels[(lut.size as usize - 1) * 4..][..3];
            let expected = Primaries::DisplayP3.to_bt2020().map(|r| r[0]);
            for i in 0..3 {
                assert!((red[i] - expected[i]).abs() < 0.003, "{intent:?}: {red:?}");
            }
            assert_eq!(lut.id, profile.description(intent).unwrap().lut.unwrap().id);
        }
        let absolute = profile.description(Intent::Absolute).unwrap().lut.unwrap();
        let white = &absolute.pixels[absolute.pixels.len() - 4..][..3];
        assert!(
            white.iter().all(|v| (*v - 80.0 / 203.0).abs() < 0.003),
            "{white:?}"
        );
    }
    #[test]
    fn icc_pq_is_hdr_and_invalid_profiles_are_rejected() {
        let bytes = ColorProfile::new_bt2020_pq().encode().unwrap();
        let desc = IccProfile::parse(&bytes)
            .unwrap()
            .description(Intent::Perceptual)
            .unwrap();
        assert!(desc.is_hdr());
        let lut = desc.lut.unwrap();
        let white = &lut.pixels[lut.pixels.len() - 4..][..3];
        assert!(
            white.iter().all(|v| (*v - 10000.0 / 203.0).abs() < 0.05),
            "{white:?}"
        );
        assert!(IccProfile::parse(&[]).is_none());
        let mut bad = bytes;
        bad[12..16].copy_from_slice(b"prtr");
        assert!(IccProfile::parse(&bad).is_none());
    }
}
