//! Parametric Wayland color management. Output descriptions name YAS's virtual
//! HDR output; each remote viewer receives its own gamut/tone conversion.
use super::*;
use crate::color::{ImageDescription, Intent, Primaries, Transfer};
use std::sync::Mutex;
use wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::{self as output, WpColorManagementOutputV1},
    wp_color_management_surface_feedback_v1::{
        self as feedback, WpColorManagementSurfaceFeedbackV1,
    },
    wp_color_management_surface_v1::{self as surface, WpColorManagementSurfaceV1},
    wp_color_manager_v1::{self as manager},
    wp_image_description_creator_icc_v1::{self as icc, WpImageDescriptionCreatorIccV1},
    wp_image_description_creator_params_v1::{self as params, WpImageDescriptionCreatorParamsV1},
    wp_image_description_v1::{self as description, WpImageDescriptionV1},
};

pub(super) use wayland_protocols::wp::color_management::v1::server::{
    wp_color_manager_v1::WpColorManagerV1, wp_image_description_info_v1::WpImageDescriptionInfoV1,
};

struct Description {
    icc: Option<Arc<super::icc::IccProfile>>,
    color: Option<ImageDescription>,
    information: bool,
}
#[derive(Default)]
struct Params {
    primaries: Option<Primaries>,
    transfer: Option<Transfer>,
    luminances: Option<(u32, u32, u32)>,
    max_cll: Option<u32>,
    max_fall: Option<u32>,
    mastering_primaries: Option<Primaries>,
    mastering_luminance: Option<(u32, u32)>,
    invalid: bool,
}

#[derive(Default)]
struct IccParams {
    bytes: Option<Result<Vec<u8>, ()>>,
}
impl Dispatch<WpImageDescriptionCreatorIccV1, Mutex<IccParams>> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        object: &WpImageDescriptionCreatorIccV1,
        request: icc::Request,
        data: &Mutex<IccParams>,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::FileExt;
        let mut params = data.lock().unwrap();
        match request {
            icc::Request::SetIccFile {
                icc_profile,
                offset,
                length,
            } => {
                if params.bytes.is_some() {
                    object.post_error(icc::Error::AlreadySet, "ICC file already set");
                    return;
                }
                if length == 0 || length > 32 * 1024 * 1024 {
                    object.post_error(icc::Error::BadSize, "ICC file must contain 1..32 MiB");
                    return;
                }
                let mut file = std::fs::File::from(icc_profile);
                let Ok(size) = file.seek(SeekFrom::End(0)) else {
                    object.post_error(icc::Error::BadFd, "ICC file must be seekable");
                    return;
                };
                if u64::from(offset) + u64::from(length) > size {
                    object.post_error(icc::Error::OutOfFile, "ICC range exceeds file");
                    return;
                }
                let mut bytes = vec![0; length as usize];
                match file.read_exact_at(&mut bytes, u64::from(offset)) {
                    Ok(()) => params.bytes = Some(Ok(bytes)),
                    Err(e) if matches!(e.raw_os_error(), Some(libc::EBADF | libc::ESPIPE)) => {
                        object.post_error(icc::Error::BadFd, "ICC file must be readable")
                    }
                    Err(_) => params.bytes = Some(Err(())),
                }
            }
            icc::Request::Create { image_description } => {
                let Some(bytes) = params.bytes.take() else {
                    object.post_error(icc::Error::IncompleteSet, "ICC file is required");
                    return;
                };
                let Ok(bytes) = bytes else {
                    init.init(
                        image_description,
                        Description {
                            color: None,
                            icc: None,
                            information: false,
                        },
                    )
                    .failed(
                        description::Cause::OperatingSystem,
                        "could not read ICC file".into(),
                    );
                    return;
                };
                let profile = super::icc::IccProfile::parse(&bytes).map(Arc::new);
                let color = profile
                    .as_ref()
                    .and_then(|p| p.description(Intent::Perceptual));
                image_with_icc(init, image_description, color, false, profile);
            }
            _ => {}
        }
    }
}

fn image(
    init: &mut DataInit<'_, Compositor>,
    id: New<WpImageDescriptionV1>,
    color: Option<ImageDescription>,
    information: bool,
) {
    image_with_icc(init, id, color, information, None);
}
fn image_with_icc(
    init: &mut DataInit<'_, Compositor>,
    id: New<WpImageDescriptionV1>,
    color: Option<ImageDescription>,
    information: bool,
    icc: Option<Arc<super::icc::IccProfile>>,
) {
    static NEXT: AtomicU32 = AtomicU32::new(2);
    let ready = color.is_some();
    let object = init.init(
        id,
        Description {
            color,
            information,
            icc,
        },
    );
    if ready {
        object.ready(if information {
            1
        } else {
            NEXT.fetch_add(1, Ordering::Relaxed).max(2)
        });
    } else {
        object.failed(
            description::Cause::Unsupported,
            "unsupported image description".into(),
        );
    }
}

fn primaries(value: manager::Primaries) -> Option<Primaries> {
    Some(match value {
        manager::Primaries::Srgb => Primaries::Srgb,
        manager::Primaries::DisplayP3 => Primaries::DisplayP3,
        manager::Primaries::DciP3 => Primaries::DciP3,
        manager::Primaries::Bt2020 => Primaries::Bt2020,
        manager::Primaries::PalM => {
            Primaries::from_xy([0.67, 0.33, 0.21, 0.71, 0.14, 0.08, 0.31006, 0.31616])?
        }
        manager::Primaries::Pal => {
            Primaries::from_xy([0.64, 0.33, 0.29, 0.60, 0.15, 0.06, 0.3127, 0.3290])?
        }
        manager::Primaries::Ntsc => {
            Primaries::from_xy([0.63, 0.34, 0.31, 0.595, 0.155, 0.07, 0.3127, 0.3290])?
        }
        manager::Primaries::GenericFilm => {
            Primaries::from_xy([0.681, 0.319, 0.243, 0.692, 0.145, 0.049, 0.31006, 0.31616])?
        }
        manager::Primaries::Cie1931Xyz => {
            Primaries::from_xy([1., 0., 0., 1., 0., 0., 1. / 3., 1. / 3.])?
        }
        manager::Primaries::AdobeRgb => {
            Primaries::from_xy([0.64, 0.33, 0.21, 0.71, 0.15, 0.06, 0.3127, 0.3290])?
        }
        _ => return None,
    })
}
fn transfer(value: manager::TransferFunction) -> Option<Transfer> {
    Some(match value {
        manager::TransferFunction::Srgb => Transfer::Srgb,
        manager::TransferFunction::Gamma22 => Transfer::Gamma22,
        manager::TransferFunction::St428 => Transfer::Gamma26,
        manager::TransferFunction::ExtLinear => Transfer::Linear,
        manager::TransferFunction::St2084Pq => Transfer::Pq,
        manager::TransferFunction::Hlg => Transfer::Hlg,
        manager::TransferFunction::Bt1886 => Transfer::Bt1886,
        manager::TransferFunction::Gamma28 => Transfer::Power(2.8),
        manager::TransferFunction::St240 => Transfer::St240,
        manager::TransferFunction::Log100 => Transfer::Log100,
        manager::TransferFunction::Log316 => Transfer::Log316,
        manager::TransferFunction::Xvycc => Transfer::Xvycc,
        manager::TransferFunction::ExtSrgb => Transfer::ExtSrgb,
        manager::TransferFunction::CompoundPower24 => Transfer::Srgb,
        _ => return None,
    })
}

impl GlobalDispatch<WpColorManagerV1, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpColorManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let object = init.init(resource, ());
        for intent in [
            manager::RenderIntent::Perceptual,
            manager::RenderIntent::Relative,
            manager::RenderIntent::Absolute,
            manager::RenderIntent::RelativeBpc,
            manager::RenderIntent::Saturation,
        ] {
            object.supported_intent(intent);
        }
        for f in [
            manager::Feature::IccV2V4,
            manager::Feature::Parametric,
            manager::Feature::SetLuminances,
            manager::Feature::WindowsScrgb,
            manager::Feature::SetPrimaries,
            manager::Feature::SetTfPower,
            manager::Feature::SetMasteringDisplayPrimaries,
            manager::Feature::ExtendedTargetVolume,
        ] {
            object.supported_feature(f);
        }
        for p in [
            manager::Primaries::Srgb,
            manager::Primaries::DisplayP3,
            manager::Primaries::DciP3,
            manager::Primaries::Bt2020,
            manager::Primaries::PalM,
            manager::Primaries::Pal,
            manager::Primaries::Ntsc,
            manager::Primaries::GenericFilm,
            manager::Primaries::Cie1931Xyz,
            manager::Primaries::AdobeRgb,
        ] {
            object.supported_primaries_named(p);
        }
        for t in [
            manager::TransferFunction::Srgb,
            manager::TransferFunction::Gamma22,
            manager::TransferFunction::St428,
            manager::TransferFunction::ExtLinear,
            manager::TransferFunction::St2084Pq,
            manager::TransferFunction::Hlg,
            manager::TransferFunction::Bt1886,
            manager::TransferFunction::Gamma28,
            manager::TransferFunction::St240,
            manager::TransferFunction::Log100,
            manager::TransferFunction::Log316,
            manager::TransferFunction::Xvycc,
            manager::TransferFunction::ExtSrgb,
        ] {
            object.supported_tf_named(t);
        }
        if object.version() >= 2 {
            object.supported_tf_named(manager::TransferFunction::CompoundPower24);
        }
        object.done();
    }
}

impl Dispatch<WpColorManagerV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        object: &WpColorManagerV1,
        request: manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            manager::Request::GetOutput { id, output } => {
                init.init(id, output);
            }
            manager::Request::GetSurface { id, surface } => {
                let entry = state.surfaces.get_mut(&surface.id()).unwrap();
                if entry.color_surface.is_some() {
                    object.post_error(
                        manager::Error::SurfaceExists,
                        "surface already has color management",
                    );
                    return;
                }
                entry.color_surface = Some(init.init(id, surface));
            }
            manager::Request::GetSurfaceFeedback { id, surface } => {
                init.init(id, surface).preferred_changed(1);
            }
            manager::Request::CreateIccCreator { obj } => {
                init.init(obj, Mutex::new(IccParams::default()));
            }
            manager::Request::CreateParametricCreator { obj } => {
                init.init(obj, Mutex::new(Params::default()));
            }
            manager::Request::CreateWindowsScrgb { image_description } => {
                image(
                    init,
                    image_description,
                    Some(ImageDescription {
                        transfer: Transfer::WindowsScrgb,
                        reference_nits: 203.0,
                        peak_nits: 10000.0,
                        min_nits: 0.0,
                        target_peak_nits: None,
                        primaries: Primaries::Srgb,
                        intent: Intent::Perceptual,
                        lut: None,
                    }),
                    false,
                );
            }
            manager::Request::Destroy => {}
            _ => object.post_error(
                manager::Error::UnsupportedFeature,
                "unsupported color-management feature",
            ),
        }
    }
}

impl Dispatch<WpColorManagementOutputV1, WlOutput> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpColorManagementOutputV1,
        request: output::Request,
        output: &WlOutput,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let output::Request::GetImageDescription { image_description } = request {
            if output.is_alive() {
                image(init, image_description, Some(ImageDescription::HDR), true);
            } else {
                init.init(
                    image_description,
                    Description {
                        color: None,
                        information: false,
                        icc: None,
                    },
                )
                .failed(description::Cause::NoOutput, "output was destroyed".into());
            }
        }
    }
}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, WlSurface> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        object: &WpColorManagementSurfaceFeedbackV1,
        request: feedback::Request,
        surface: &WlSurface,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            feedback::Request::GetPreferred { image_description }
            | feedback::Request::GetPreferredParametric { image_description } => {
                if !state.surfaces.contains_key(&surface.id()) {
                    object.post_error(feedback::Error::Inert, "surface was destroyed");
                    return;
                }
                image(init, image_description, Some(ImageDescription::HDR), true);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagementSurfaceV1, WlSurface> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        object: &WpColorManagementSurfaceV1,
        request: surface::Request,
        surface: &WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let Some(entry) = state.surfaces.get_mut(&surface.id()) else {
            if !matches!(request, surface::Request::Destroy) {
                object.post_error(surface::Error::Inert, "surface was destroyed");
            }
            return;
        };
        match request {
            surface::Request::SetImageDescription {
                image_description,
                render_intent,
            } => {
                let intent = match render_intent.into_result().ok() {
                    Some(manager::RenderIntent::Perceptual) => Intent::Perceptual,
                    Some(manager::RenderIntent::Relative) => Intent::Relative,
                    Some(manager::RenderIntent::Absolute) => Intent::Absolute,
                    Some(manager::RenderIntent::RelativeBpc) => Intent::RelativeBpc,
                    Some(manager::RenderIntent::Saturation) => Intent::Saturation,
                    _ => {
                        object
                            .post_error(surface::Error::RenderIntent, "unsupported render intent");
                        return;
                    }
                };
                let Some(mut color) = image_description.data::<Description>().and_then(|d| {
                    if let Some(profile) = &d.icc {
                        profile.description(intent)
                    } else {
                        d.color.clone()
                    }
                }) else {
                    object.post_error(
                        surface::Error::ImageDescription,
                        "image description is not ready",
                    );
                    return;
                };
                color.intent = intent;
                entry.pending_color_description = Some(color);
            }
            surface::Request::UnsetImageDescription => {
                entry.pending_color_description = Some(ImageDescription::default())
            }
            surface::Request::Destroy => {
                entry.color_surface = None;
                entry.pending_color_description = Some(ImageDescription::default());
            }
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionCreatorParamsV1, Mutex<Params>> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        object: &WpImageDescriptionCreatorParamsV1,
        request: params::Request,
        data: &Mutex<Params>,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let mut p = data.lock().unwrap();
        let duplicate = || object.post_error(params::Error::AlreadySet, "property already set");
        match request {
            params::Request::SetPrimariesNamed { primaries: value } => {
                if p.primaries.is_some() {
                    duplicate();
                    return;
                }
                p.primaries = value.into_result().ok().and_then(primaries);
                if p.primaries.is_none() {
                    object.post_error(
                        params::Error::InvalidPrimariesNamed,
                        "unsupported primaries",
                    );
                }
            }
            params::Request::SetTfNamed { tf } => {
                if p.transfer.is_some() {
                    duplicate();
                    return;
                }
                p.transfer = tf.into_result().ok().and_then(transfer);
                if p.transfer.is_none() {
                    object.post_error(params::Error::InvalidTf, "unsupported transfer function");
                }
            }
            params::Request::SetTfPower { eexp } => {
                if p.transfer.is_some() {
                    duplicate();
                    return;
                }
                if !(10000..=100000).contains(&eexp) {
                    object.post_error(
                        params::Error::InvalidTf,
                        "power exponent must be between 1 and 10",
                    );
                    return;
                }
                p.transfer = Some(Transfer::Power(eexp as f32 / 10000.0));
            }
            params::Request::SetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                if p.primaries.is_some() {
                    duplicate();
                    return;
                }
                let primary = Primaries::from_xy(
                    [r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y].map(|v| f64::from(v) / 1_000_000.0),
                );
                p.invalid |= primary.is_none();
                p.primaries = Some(primary.unwrap_or_default());
            }
            params::Request::SetMasteringDisplayPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                if p.mastering_primaries.is_some() {
                    duplicate();
                    return;
                }
                let primary = Primaries::from_xy(
                    [r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y].map(|v| f64::from(v) / 1_000_000.0),
                );
                p.invalid |= primary.is_none();
                p.mastering_primaries = Some(primary.unwrap_or_default());
            }
            params::Request::SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                if p.luminances.is_some() {
                    duplicate();
                    return;
                }
                if u64::from(max_lum) * 10000 <= u64::from(min_lum)
                    || u64::from(reference_lum) * 10000 <= u64::from(min_lum)
                {
                    object.post_error(params::Error::InvalidLuminance, "invalid luminance range");
                    return;
                }
                p.luminances = Some((min_lum, max_lum, reference_lum));
            }
            params::Request::SetMasteringLuminance { min_lum, max_lum } => {
                if p.mastering_luminance.is_some() {
                    duplicate();
                    return;
                }
                if u64::from(max_lum) * 10000 <= u64::from(min_lum) {
                    object.post_error(params::Error::InvalidLuminance, "invalid mastering range");
                    return;
                }
                p.mastering_luminance = Some((min_lum, max_lum));
            }
            params::Request::SetMaxCll { max_cll } => {
                if p.max_cll.replace(max_cll).is_some() {
                    duplicate();
                }
            }
            params::Request::SetMaxFall { max_fall } => {
                if p.max_fall.replace(max_fall).is_some() {
                    duplicate();
                }
            }
            params::Request::Create { image_description } => {
                let (Some(primaries), Some(transfer)) = (p.primaries, p.transfer) else {
                    object.post_error(
                        params::Error::IncompleteSet,
                        "primaries and transfer are required",
                    );
                    return;
                };
                let (min, mut peak, reference) = p.luminances.unwrap_or(match transfer {
                    Transfer::Pq => (50, 10000, 203),
                    Transfer::Hlg => (50, 1000, 203),
                    Transfer::Bt1886 => (100, 100, 100),
                    _ => (2000, 80, 80),
                });
                if transfer == Transfer::Pq {
                    peak = 10000 + min / 10000;
                }
                let (master_min, master_max) = p.mastering_luminance.unwrap_or((min, peak));
                if p.max_fall.zip(p.max_cll).is_some_and(|(f, c)| f > c)
                    || [p.max_cll, p.max_fall].iter().flatten().any(|&v| {
                        object.version() < 2
                            && (u64::from(v) * 10000 <= u64::from(master_min) || v > master_max)
                    })
                {
                    object.post_error(
                        params::Error::InvalidLuminance,
                        "content light levels exceed mastering range",
                    );
                    return;
                }
                // Bounded, representable virtual output. Unsupported valid descriptions
                // fail as image descriptions rather than terminating the client.
                let supported = !p.invalid
                    && reference > 0
                    && reference <= 10000
                    && peak <= 10001
                    && min <= 10000;
                image(
                    init,
                    image_description,
                    supported.then_some(ImageDescription {
                        intent: Intent::Perceptual,
                        lut: None,
                        primaries,
                        transfer,
                        reference_nits: reference as f32,
                        peak_nits: peak as f32,
                        min_nits: min as f32 / 10000.0,
                        target_peak_nits: p
                            .max_cll
                            .map(|max| max as f32)
                            .or(p.mastering_luminance.map(|(_, max)| max as f32)),
                    }),
                    false,
                );
            }
            _ => object.post_error(
                params::Error::UnsupportedFeature,
                "unsupported parametric feature",
            ),
        }
    }
}

impl Dispatch<WpImageDescriptionV1, Description> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        object: &WpImageDescriptionV1,
        request: description::Request,
        data: &Description,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let description::Request::GetInformation { information } = request {
            if data.color.is_none() {
                object.post_error(description::Error::NotReady, "description is not ready");
                return;
            }
            if !data.information {
                object.post_error(
                    description::Error::NoInformation,
                    "client-created description has no information",
                );
                return;
            }
            let info = init.init(information, ());
            info.primaries(
                708000, 292000, 170000, 797000, 131000, 46000, 312700, 329000,
            );
            info.primaries_named(manager::Primaries::Bt2020);
            info.tf_named(manager::TransferFunction::St2084Pq);
            info.luminances(50, 10000, 203);
            info.target_primaries(
                708000, 292000, 170000, 797000, 131000, 46000, 312700, 329000,
            );
            info.target_luminance(50, 10000);
            // `done` destroys the new object. The backend installs its data
            // after this callback returns, so defer destruction until the
            // request dispatch is complete.
            state.pending_color_info_done.push(info);
        }
    }
}
impl Dispatch<WpImageDescriptionInfoV1, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpImageDescriptionInfoV1,
        _: <WpImageDescriptionInfoV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
