//! Color-managed surfaces use linear BT.2020, with 1.0 = 203 cd/m².
//! Untagged surfaces retain the existing sRGB/BT.601 video path.
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Intent {
    #[default]
    Perceptual = 0,
    Relative = 1,
    Absolute = 2,
    RelativeBpc = 3,
    Saturation = 4,
}

#[derive(Debug)]
pub struct ColorLut {
    pub id: u64,
    pub size: u32,
    /// Red varies fastest, then green, then blue. Linear BT.2020 RGBA.
    pub pixels: Vec<f32>,
}
impl PartialEq for ColorLut {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputColor {
    #[default]
    Srgb,
    DisplayP3,
    Hdr10,
}

impl OutputColor {
    /// H.273 primaries, transfer, matrix, and full-range flag.
    pub fn cicp(self) -> [u8; 4] {
        match self {
            Self::Srgb => [1, 13, 6, 0],
            Self::DisplayP3 => [12, 13, 1, 0],
            Self::Hdr10 => [9, 16, 9, 0],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Primaries {
    #[default]
    Srgb,
    DisplayP3,
    DciP3,
    Bt2020,
    Custom {
        matrix: [[f32; 3]; 3],
        absolute: [[f32; 3]; 3],
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Transfer {
    #[default]
    Srgb,
    Gamma22,
    Gamma26,
    Linear,
    Pq,
    Hlg,
    WindowsScrgb,
    Power(f32),
    Bt1886,
    St240,
    Log100,
    Log316,
    Xvycc,
    ExtSrgb,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageDescription {
    pub intent: Intent,
    pub lut: Option<Arc<ColorLut>>,
    pub primaries: Primaries,
    pub transfer: Transfer,
    pub reference_nits: f32,
    pub peak_nits: f32,
    pub min_nits: f32,
    pub target_peak_nits: Option<f32>,
}

impl Default for ImageDescription {
    fn default() -> Self {
        Self {
            intent: Intent::Perceptual,
            lut: None,
            primaries: Primaries::Srgb,
            transfer: Transfer::Srgb,
            reference_nits: 80.0,
            peak_nits: 80.0,
            min_nits: 0.2,
            target_peak_nits: None,
        }
    }
}

impl ImageDescription {
    pub const HDR: Self = Self {
        intent: Intent::Perceptual,
        lut: None,
        primaries: Primaries::Bt2020,
        transfer: Transfer::Pq,
        reference_nits: 203.0,
        peak_nits: 10000.0,
        min_nits: 0.005,
        target_peak_nits: None,
    };

    /// Luminance in the compositor's 203-nit reference scale.
    pub fn tone_mapping_peak(&self) -> Option<f32> {
        self.target_peak_nits
            .map(|peak| {
                if matches!(self.transfer, Transfer::WindowsScrgb)
                    || self.intent == Intent::Absolute
                {
                    peak
                } else {
                    peak * 203.0 / self.reference_nits
                }
            })
            .filter(|p| p.is_finite() && *p > 203.0)
    }

    pub fn is_hdr(&self) -> bool {
        matches!(
            self.transfer,
            Transfer::Pq | Transfer::Hlg | Transfer::Linear | Transfer::WindowsScrgb
        ) || self.peak_nits > self.reference_nits
            || self
                .target_peak_nits
                .is_some_and(|peak| peak > self.reference_nits)
    }

    /// Three matrix rows followed by transfer parameters; fragment push constants.
    /// Reference whites are anchored at 203 nits; Windows-scRGB stays absolute.
    pub fn shader_params(&self) -> [f32; 16] {
        let m = if self.intent == Intent::Absolute {
            self.primaries.absolute_to_bt2020()
        } else {
            self.primaries.to_bt2020()
        };
        [
            m[0][0],
            m[0][1],
            m[0][2],
            if let Transfer::Power(gamma) = self.transfer {
                gamma
            } else {
                0.0
            },
            m[1][0],
            m[1][1],
            m[1][2],
            self.min_nits,
            m[2][0],
            m[2][1],
            m[2][2],
            self.intent as u8 as f32,
            if self.lut.is_some() {
                14.0
            } else {
                match self.transfer {
                    Transfer::Srgb => 0.0,
                    Transfer::Gamma22 => 1.0,
                    Transfer::Gamma26 => 2.0,
                    Transfer::Linear => 3.0,
                    Transfer::Pq => 4.0,
                    Transfer::Hlg => 5.0,
                    Transfer::WindowsScrgb => 6.0,
                    Transfer::Power(_) => 7.0,
                    Transfer::Bt1886 => 8.0,
                    Transfer::St240 => 9.0,
                    Transfer::Log100 => 10.0,
                    Transfer::Log316 => 11.0,
                    Transfer::Xvycc => 12.0,
                    Transfer::ExtSrgb => 13.0,
                }
            },
            self.peak_nits / self.reference_nits,
            self.reference_nits / 203.0,
            self.peak_nits,
        ]
    }
}

impl Primaries {
    pub fn absolute_to_bt2020(self) -> [[f32; 3]; 3] {
        match self {
            Self::Custom { absolute, .. } => absolute,
            Self::DciP3 => Self::from_xy([0.68, 0.32, 0.265, 0.69, 0.15, 0.06, 0.314, 0.351])
                .unwrap()
                .absolute_to_bt2020(),
            _ => self.to_bt2020(),
        }
    }

    pub fn to_bt2020(self) -> [[f32; 3]; 3] {
        match self {
            Self::Srgb => [
                [0.627404, 0.329283, 0.043313],
                [0.069097, 0.919540, 0.011362],
                [0.016391, 0.088013, 0.895595],
            ],
            Self::DisplayP3 => [
                [0.753833, 0.198597, 0.047570],
                [0.045744, 0.941777, 0.012479],
                [-0.001210, 0.017602, 0.983609],
            ],
            // DCI white is Bradford-adapted to D65 before the gamut transform.
            Self::DciP3 => [
                [0.711783, 0.243660, 0.044556],
                [0.041615, 0.949842, 0.008543],
                [-0.000845, 0.019110, 0.981735],
            ],
            Self::Bt2020 => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            Self::Custom { matrix, .. } => matrix,
        }
    }

    /// Build RGB→XYZ from chromaticities, Bradford-adapt the white to D65,
    /// then convert XYZ→BT.2020. Unnormalised XYZ columns also handle the
    /// CIE XYZ primaries, whose y coordinates can be zero.
    pub fn from_xy(xy: [f64; 8]) -> Option<Self> {
        if xy.iter().any(|v| !v.is_finite()) || xy[7] <= 0.0 {
            return None;
        }
        let xyz = |x: f64, y: f64| [x, y, 1.0 - x - y];
        let columns = [xyz(xy[0], xy[1]), xyz(xy[2], xy[3]), xyz(xy[4], xy[5])];
        let mut basis = [[0.0; 3]; 3];
        for (i, row) in basis.iter_mut().enumerate() {
            for j in 0..3 {
                row[j] = columns[j][i];
            }
        }
        let white = xyz(xy[6], xy[7]).map(|v| v / xy[7]);
        let scale = mat_vec(inverse(basis)?, white);
        if scale.iter().any(|v| *v <= 0.0 || !v.is_finite()) {
            return None;
        }
        for row in &mut basis {
            for j in 0..3 {
                row[j] *= scale[j];
            }
        }
        let bradford = [
            [0.8951, 0.2664, -0.1614],
            [-0.7502, 1.7135, 0.0367],
            [0.0389, -0.0685, 1.0296],
        ];
        let source = mat_vec(bradford, white);
        let target = mat_vec(
            bradford,
            [0.3127 / 0.3290, 1.0, (1.0 - 0.3127 - 0.3290) / 0.3290],
        );
        let mut adaptation = bradford;
        for i in 0..3 {
            for value in &mut adaptation[i] {
                *value *= target[i] / source[i];
            }
        }
        let xyz_to_2020 = [
            [1.7166511880, -0.3556707838, -0.2533662814],
            [-0.6666843518, 1.6164812366, 0.0157685458],
            [0.0176398574, -0.0427706133, 0.9421031212],
        ];
        let matrix = mat_mul(
            xyz_to_2020,
            mat_mul(inverse(bradford)?, mat_mul(adaptation, basis)),
        );
        if matrix
            .iter()
            .flatten()
            .any(|v| !v.is_finite() || v.abs() > 10000.0)
        {
            return None;
        }
        Some(Self::Custom {
            matrix: matrix.map(|r| r.map(|v| v as f32)),
            absolute: mat_mul(xyz_to_2020, basis).map(|r| r.map(|v| v as f32)),
        })
    }
}

fn mat_vec(m: [[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    m.map(|r| r[0] * v[0] + r[1] * v[1] + r[2] * v[2])
}
fn mat_mul(a: [[f64; 3]; 3], b: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    a.map(|r| std::array::from_fn(|j| (0..3).map(|k| r[k] * b[k][j]).sum()))
}
fn inverse(m: [[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let [[a, b, c], [d, e, f], [g, h, i]] = m;
    let cof = [
        [e * i - f * h, c * h - b * i, b * f - c * e],
        [f * g - d * i, a * i - c * g, c * d - a * f],
        [d * h - e * g, b * g - a * h, a * e - b * d],
    ];
    let determinant = a * cof[0][0] + b * cof[1][0] + c * cof[2][0];
    (determinant.abs() > 1e-12).then(|| cof.map(|r| r.map(|v| v / determinant)))
}

pub fn srgb_encode(v: f32) -> f32 {
    if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

pub fn pq_encode(nits: f32) -> f32 {
    let p = (nits.clamp(0.0, 10000.0) / 10000.0).powf(2610.0 / 16384.0);
    ((3424.0 / 4096.0 + 2413.0 / 128.0 * p) / (1.0 + 2392.0 / 128.0 * p)).powf(2523.0 / 32.0)
}

/// Source luminance metadata for HDR-to-SDR output. Browsers map retained
/// HDR to their physical display; this curve is for server-side SDR views.
#[derive(Clone, Copy, Debug, Default)]
pub struct ToneMapping {
    pub hdr: bool,
    pub peak_nits: Option<f32>,
}
impl From<bool> for ToneMapping {
    fn from(hdr: bool) -> Self {
        Self {
            hdr,
            peak_nits: None,
        }
    }
}
impl ToneMapping {
    pub fn shader_parameter(self) -> u32 {
        if !self.hdr {
            return 0;
        }
        self.peak_nits
            .filter(|p| p.is_finite() && *p > 203.0)
            .map_or(1, |p| p.min(10000.0).to_bits())
    }
    fn luminance(self, y: f32) -> f32 {
        if !self.hdr || y <= 0.75 {
            return y;
        }
        if let Some(peak) = self.peak_nits.filter(|p| p.is_finite() && *p > 203.0) {
            // A rational shoulder is continuous with slope 1 at the knee,
            // and maps the declared peak to SDR white without collapsing
            // the upper range into the exponential curve's flat tail.
            let range = peak.min(10000.0) / 203.0 - 0.75;
            let x = (y - 0.75).min(range);
            0.75 + x / (1.0 + x * (4.0 - 1.0 / range))
        } else {
            0.75 + 0.25 * (1.0 - (-(y - 0.75) / 0.25).exp())
        }
    }
}

/// Convert linear BT.2020 to the negotiated encoded RGB space. A luminance
/// shoulder and neutral-axis gamut compression preserve hue in SDR fallback.
pub fn output_rgb(
    mut rgb: [f32; 3],
    output: OutputColor,
    hdr_source: impl Into<ToneMapping>,
) -> [f32; 3] {
    let tone = hdr_source.into();
    for v in &mut rgb {
        if !v.is_finite() {
            *v = 0.0;
        }
    }
    if output == OutputColor::Hdr10 {
        return rgb.map(|v| pq_encode(v * 203.0));
    }
    if tone.hdr {
        let y = (0.2627 * rgb[0] + 0.6780 * rgb[1] + 0.0593 * rgb[2]).max(0.0);
        if y > 0.75 {
            let mapped = tone.luminance(y);
            rgb = rgb.map(|v| v * mapped / y);
        }
    }
    let matrix = match output {
        OutputColor::Srgb => [
            [1.660491, -0.587641, -0.072850],
            [-0.124550, 1.1329, -0.008349],
            [-0.018151, -0.100579, 1.11873],
        ],
        OutputColor::DisplayP3 => [
            [1.343578, -0.282180, -0.061399],
            [-0.065298, 1.075788, -0.010490],
            [0.002822, -0.019599, 1.016777],
        ],
        OutputColor::Hdr10 => unreachable!(),
    };
    rgb = matrix.map(|r| r[0] * rgb[0] + r[1] * rgb[1] + r[2] * rgb[2]);
    let luma = if output == OutputColor::DisplayP3 {
        [0.228975, 0.691739, 0.079287]
    } else {
        [0.2126, 0.7152, 0.0722]
    };
    let y = (luma[0] * rgb[0] + luma[1] * rgb[1] + luma[2] * rgb[2]).clamp(0.0, 1.0);
    let mut saturation = 1.0f32;
    for v in rgb {
        if v < 0.0 {
            saturation = saturation.min(y / (y - v));
        }
        if v > 1.0 {
            saturation = saturation.min((1.0 - y) / (v - y));
        }
    }
    rgb.map(|v| srgb_encode((y + (v - y) * saturation).clamp(0.0, 1.0)))
}

pub fn rgba8(pixels: &[f32], output: OutputColor, hdr_source: impl Into<ToneMapping>) -> Vec<u8> {
    let hdr_source = hdr_source.into();
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let c = output_rgb([p[0], p[1], p[2]], output, hdr_source);
            [
                (c[0] * 255.0).round() as u8,
                (c[1] * 255.0).round() as u8,
                (c[2] * 255.0).round() as u8,
                255,
            ]
        })
        .collect()
}

/// Bilinear resize before transfer encoding, retaining HDR values and alpha.
pub fn resize_linear(pixels: &[f32], width: u32, height: u32, tw: u32, th: u32) -> Vec<f32> {
    if width == 0
        || height == 0
        || tw == 0
        || th == 0
        || pixels.len() != width as usize * height as usize * 4
    {
        return Vec::new();
    }
    let mut result = Vec::with_capacity(tw as usize * th as usize * 4);
    for y in 0..th {
        let sy =
            ((y as f32 + 0.5) * height as f32 / th as f32 - 0.5).clamp(0.0, height as f32 - 1.0);
        let y0 = sy as u32;
        let y1 = (y0 + 1).min(height - 1);
        for x in 0..tw {
            let sx =
                ((x as f32 + 0.5) * width as f32 / tw as f32 - 0.5).clamp(0.0, width as f32 - 1.0);
            let x0 = sx as u32;
            let x1 = (x0 + 1).min(width - 1);
            for c in 0..4 {
                let sample = |x, y| pixels[((y * width + x) * 4) as usize + c];
                let top =
                    sample(x0, y0) * (1.0 - (sx - x0 as f32)) + sample(x1, y0) * (sx - x0 as f32);
                let bottom =
                    sample(x0, y1) * (1.0 - (sx - x0 as f32)) + sample(x1, y1) * (sx - x0 as f32);
                result.push(top * (1.0 - (sy - y0 as f32)) + bottom * (sy - y0 as f32));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_primaries_match_named_spaces_and_adapt_white() {
        for (xy, named) in [
            (
                [0.68, 0.32, 0.265, 0.69, 0.15, 0.06, 0.3127, 0.3290],
                Primaries::DisplayP3,
            ),
            (
                [0.68, 0.32, 0.265, 0.69, 0.15, 0.06, 0.314, 0.351],
                Primaries::DciP3,
            ),
            (
                [0.64, 0.33, 0.30, 0.60, 0.15, 0.06, 0.3127, 0.3290],
                Primaries::Srgb,
            ),
        ] {
            let matrix = Primaries::from_xy(xy).unwrap().to_bt2020();
            for (custom, named) in matrix
                .iter()
                .flatten()
                .zip(named.to_bt2020().iter().flatten())
            {
                assert!((custom - named).abs() < 0.00001);
            }
            for row in matrix {
                assert!((row.iter().sum::<f32>() - 1.0).abs() < 0.00001);
            }
        }
        assert!(Primaries::from_xy([0.5; 8]).is_none());
        assert!(Primaries::from_xy([f64::NAN; 8]).is_none());
        assert!(Primaries::from_xy([1., 0., 0., 1., 0., 0., 1. / 3., 1. / 3.]).is_some());
    }

    #[test]
    fn pq_reference_levels() {
        assert!((pq_encode(100.0) - 0.5080784).abs() < 0.00002);
        assert!((pq_encode(1000.0) - 0.7518271).abs() < 0.00002);
        assert!((pq_encode(10000.0) - 1.0).abs() < 0.00002);
    }
    #[test]
    fn p3_red_survives_and_srgb_fallback_is_bounded() {
        let red = Primaries::DisplayP3.to_bt2020().map(|row| row[0]);
        let p3 = output_rgb(red, OutputColor::DisplayP3, false);
        assert!(p3[0] > 0.999 && p3[1].abs() < 0.0001 && p3[2].abs() < 0.0001);
        assert!(
            output_rgb(red, OutputColor::Srgb, false)
                .iter()
                .all(|v| (0.0..=1.0).contains(v))
        );
    }
    #[test]
    fn hdr_fallback_is_monotonic_and_neutral() {
        let mut last = -1.0;
        for n in 0..100 {
            let rgb = output_rgb([n as f32 / 10.0; 3], OutputColor::Srgb, true);
            assert!(rgb[0] >= last && rgb[0] <= 1.0);
            assert!((rgb[0] - rgb[1]).abs() < 0.00001);
            last = rgb[0];
        }
    }
    #[test]
    fn resize_keeps_highlights() {
        assert_eq!(
            resize_linear(&[0., 0., 0., 1., 8., 8., 8., 1.], 2, 1, 1, 1),
            [4., 4., 4., 1.]
        );
    }
}

#[cfg(test)]
mod tone_mapping_tests {
    use super::*;
    #[test]
    fn declared_peaks_preserve_shadows_and_highlight_detail() {
        for peak in [400.0, 1000.0, 4000.0, 10000.0] {
            let tone = ToneMapping {
                hdr: true,
                peak_nits: Some(peak),
            };
            for shadow in [0.0, 0.01, 0.18, 0.5, 0.75] {
                assert_eq!(tone.luminance(shadow), shadow);
            }
            assert!((tone.luminance(peak / 203.0) - 1.0).abs() < 1e-6);
            let mut previous = 0.0;
            for i in 1..=1000 {
                let mapped = tone.luminance(peak / 203.0 * i as f32 / 1000.0);
                assert!(mapped > previous && mapped <= 1.000001);
                previous = mapped;
            }
        }
    }
    #[test]
    fn metadata_changes_sdr_shoulder_without_changing_hdr() {
        let tone = ToneMapping {
            hdr: true,
            peak_nits: Some(1000.0),
        };
        let rgb = [2.0; 3];
        assert_ne!(
            output_rgb(rgb, OutputColor::Srgb, tone),
            output_rgb(rgb, OutputColor::Srgb, true)
        );
        assert_eq!(
            output_rgb(rgb, OutputColor::Hdr10, tone),
            output_rgb(rgb, OutputColor::Hdr10, true)
        );
        for peak in [f32::NAN, -1.0, 0.0, 203.0, f32::INFINITY] {
            let invalid = ToneMapping {
                hdr: true,
                peak_nits: Some(peak),
            };
            assert_eq!(invalid.shader_parameter(), 1);
            assert_eq!(
                output_rgb(rgb, OutputColor::Srgb, invalid),
                output_rgb(rgb, OutputColor::Srgb, true)
            );
        }
    }
    #[test]
    fn mastering_peak_uses_the_same_reference_scale_as_pixels() {
        let description = ImageDescription {
            reference_nits: 100.0,
            target_peak_nits: Some(1000.0),
            ..ImageDescription::HDR
        };
        assert_eq!(description.tone_mapping_peak(), Some(2030.0));
        assert_eq!(
            ImageDescription {
                intent: Intent::Absolute,
                ..description.clone()
            }
            .tone_mapping_peak(),
            Some(1000.0)
        );
        assert_eq!(
            ImageDescription {
                transfer: Transfer::WindowsScrgb,
                ..description
            }
            .tone_mapping_peak(),
            Some(1000.0)
        );
    }
}
