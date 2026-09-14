# Surface color in YAS

YAS supports wide-gamut SDR and HDR for Wayland surfaces, including Display-P3,
DCI-P3, BT.2020/PQ, HLG, and Windows-scRGB. DCI-P3 inputs are adapted from DCI
white to D65; browser P3 output uses Display-P3. Untagged applications are
sRGB. Applications describe their buffers with `wp_color_manager_v1`.

## Input and composition

The Vulkan compositor advertises color-management protocol version 2, including
parametric descriptions, custom primaries and power curves, luminances,
mastering primaries/luminances, extended target volumes, and Windows-scRGB.
All named primaries and transfer functions in version 2 are accepted, including
Adobe RGB, CIE XYZ, PAL/NTSC, BT.1886, and extended sRGB. Custom whites are
Bradford-adapted to D65. Invalid or unrepresentable descriptions fail through
the protocol. ICC v2/v4 display and color-space profiles are parsed with
moxcms under the protocol's 32 MiB limit. RGB/XYZ/Lab profiles whose transforms
can be built use cached 65³ float lookup tables sampled on the GPU. Perceptual,
relative, relative with black-point compensation, absolute, and saturation
intents are advertised. Absolute intent preserves the source white and its
luminance; other parametric intents adapt white to D65. Unsupported ICC
transforms return the protocol's `unsupported` failure.

Descriptions apply at the surface commit boundary, including unset and
destruction of the color-management surface object. The virtual output and
preferred description are BT.2020/PQ with 203-nit reference white. Mastering
and content-light metadata identify HDR content and tune HDR-to-SDR conversion.
A declared content-light peak takes precedence over the mastering peak. The
compositor carries this metadata through float readbacks, scaling, software
encoders, GPU conversion, and PipeWire SDR/P3 output.

SHM supports ARGB/XRGB/ABGR/XBGR in 8-bit, 2:10:10:10, 16-bit integer, and
16-bit float formats. DMA-BUF supports the same RGB formats subject to the
Vulkan driver's advertised format/modifier support. X formats force opaque
alpha. Sixteen-bit channel order and upload strides preserve source precision;
the fallback for linear DMA-BUFs also retains the original sample format.

A tree containing managed color is composited into linear BT.2020 RGBA16F.
The common reference white is 203 cd/m². Parametric source whites are anchored
to it; Windows-scRGB retains its absolute scale of 80 cd/m² per unit. Blending
and per-view downscaling preserve values above 1.0. Untagged SDR trees retain
the existing BGRA8 compositor and direct hardware paths.

## Per-view output

| Viewer capability                               | Managed stream                             | CICP (primaries, transfer, matrix, range) |
| ----------------------------------------------- | ------------------------------------------ | ----------------------------------------- |
| SDR or no color extension                       | Tone/gamut mapped sRGB, 8-bit H.264 or AV1 | 1, 13, 6, 0                               |
| Display-P3                                      | Display-P3 SDR, 8-bit H.264 or AV1         | 12, 13, 1, 0                              |
| HDR display, 10-bit AV1, extended WebGPU canvas | BT.2020/PQ HDR, 10-bit AV1                 | 9, 16, 9, 0                               |

Each view independently chooses color output, backend, and supported chroma
sampling. SDR, P3, and HDR viewers can watch the same surface simultaneously.
`YAS_CHROMA=444` (the default) requests full-resolution chroma; YAS falls back
to 4:2:0 when the encoder or viewer cannot support it. HDR 4:4:4 additionally
requires the viewer to advertise 10-bit AV1 High profile support.

| Backend                                 | Managed SDR / P3 | HDR                       | Chroma                           |
| --------------------------------------- | ---------------- | ------------------------- | -------------------------------- |
| Software H.264 (OpenH264 or x264 build) | Yes              | No                        | Existing backend capabilities    |
| Software AV1 (rav1e)                    | Yes              | 10-bit                    | 4:2:0 / 4:4:4                    |
| NVIDIA NVENC H.264                      | Yes              | No                        | 4:2:0 / 4:4:4, subject to GPU    |
| NVIDIA NVENC AV1                        | Yes              | 10-bit, subject to GPU    | 4:2:0 with the supported SDK     |
| VA-API H.264                            | Yes              | No                        | 4:2:0                            |
| VA-API AV1                              | Yes              | 10-bit, subject to driver | 4:2:0 / 4:4:4, subject to driver |
| Vulkan Video H.264                      | Yes              | No                        | 4:2:0 / 4:4:4, subject to driver |
| Vulkan Video AV1                        | Yes              | 10-bit, subject to driver | 4:2:0 / 4:4:4, subject to driver |

Managed encoding follows the configured encoder preference order and falls
back to software within the view's negotiated codec family. Hardware profile
and upload capabilities are checked at initialization. Vulkan sessions with
matching profiles can share their converted image. Different codec, chroma,
or color profiles get separate GPU images, including at the same target size.

Vulkan Video scales and converts the linear composite on the GPU into 8-bit
NV12/NV24 or 10-bit two-plane video. GPU-only streams avoid CPU readback;
capture, PipeWire, and CPU consumers request it as needed. NVENC uses GPU
conversion into NV12, P010, or planar 4:4:4 OPAQUE_FD buffers imported by CUDA.
VA-API exports its encoder surfaces for Vulkan to fill directly, preserving
the driver's plane offsets, pitches, and modifier. Supported output layouts
are NV12/P010, planar 4:4:4 (444P/Q416), and packed AYUV/XYUV, Y410, and Y416.
Managed VA-API streams need no VPP engine or intermediate GBM RGB pool.
Conversion includes linear scaling, gamut conversion, and HDR-to-SDR tone mapping.
GPU buffers carry their output color; NVENC also checks precision and layout.
VA-API consumers must own the exported surface.

Equal-sized viewers have independent color/chroma pools: matching NVENC profiles
share conversion, and each VA-API encoder keeps its own surfaces. Reconfiguring
or removing a viewer preserves the other pools. Readbacks are requested only by
CPU consumers or when GPU allocation/import fails; these failures retain the
float conversion fallback. Software AV1 retains its approximately 4K-pixel budget
and adaptive pacing; hardware uses the existing backend limits.

HDR-to-SDR conversion preserves shadows and adds a luminance shoulder above
75% of diffuse white, followed by neutral-axis gamut compression and sRGB
encoding. With mastering/content-light metadata, a continuous rational curve
maps the declared source peak to SDR white while retaining highlight detail.
Without that metadata, YAS uses its exponential shoulder. CPU and GPU paths
use the same curve. P3-only viewers receive the mapped highlights in their
wider gamut. HDR display adaptation stays with the browser's extended-range
presentation; browsers do not expose a reliable physical peak-luminance value.

VA-API currently exposes no H.264 4:4:4 profile; YAS selects another encoder
or 4:2:0. NVENC's AV1 API still supports only 4:2:0, including in the
[upstream SDK headers](https://github.com/FFmpeg/nv-codec-headers/blob/master/include/ffnvcodec/nvEncodeAPI.h).
These are API limits, not pending YAS implementations. Actual HDR presentation
still needs physical monitor/browser/OS validation, and VA-API AV1 encoding
needs an AV1-capable GPU; this host validates its surface conversion but has
no Intel AV1 encoder.

## Browser presentation

P3 negotiation requires a Display-P3 canvas and a wide-gamut display; it works
with H.264-only viewers. HDR requires an HDR display, 10-bit AV1 decoding,
extended-range `rgba16float` WebGPU canvas support, and an actual 1,000-nit
AV1 decode/import probe whose float readback exceeds SDR white. Accepting a
decoder configuration alone does not enable HDR. A separate decoder probe
checks 10-bit AV1 4:4:4. HDR-capable views reserve AV1 when opened because the
protocol fixes the codec family for the view's lifetime.

Packed frame color metadata configures WebCodecs. P3 backing, intermediate,
and visible 2D canvases use Display-P3. HDR presentation imports the original
decoded VideoFrame into a WebGPU external texture and draws into an
extended-range canvas. The shared retained VideoFrame is closed on replacement
or surface teardown. Display changes and WebGPU device failure renegotiate
views; unsupported viewers receive server-side SDR conversion. Repeated
failures of a real stream retire its 4:4:4 or HDR capability and reopen views
with a compatible profile. Native AV1 views read their profile from keyframes.

Portal picker thumbnails use 16-bit color-tagged PNGs resized in linear light.
Built-in workspace thumbnails use the same HDR presenter. Repeated halving through
reusable float16 textures preserves highlights while filtering small previews.
Consumers of the backing 2D canvas receive SDR/P3; HDR consumers must use the
retained VideoFrame. Physical HDR appearance depends on the browser, OS, and
monitor; code and bitstream validation do not replace display measurements.

## PipeWire sharing

ScreenCast offers full-range BT.2020/PQ in RGBA float16 and either packed
10-bit RGB channel order, plus Display-P3 RGBA8 and sRGB RGBA8 fallbacks.
Float16 samples are PQ encoded, matching their negotiated transfer function.
SPA range, matrix, transfer, and primaries accompany each offered format.
Unconstrained consumers default to sRGB; color-aware consumers can request P3
or PQ explicitly. Selected formats determine buffer strides and conversion; renegotiation drops
queued frames from the old format. Cameras retain their existing RGBA8 format.

## Captures

CAPTURE preserves managed color in both formats: PNG uses 16-bit RGB with a
full-range RGB `cICP` chunk; AVIF uses 12-bit 4:4:4 AV1 with matching container
and bitstream color metadata. HDR captures use BT.2020/PQ, and managed SDR
captures use Display-P3. Untagged SDR captures keep their existing behavior.
Capture viewers must honor the color metadata; older PNG readers may ignore
`cICP`.

## Protocol

OPEN_VIEW and CONFIGURE_VIEW accept optional extension tag 8, containing one
capability byte: bit 0 enables Display-P3 SDR, bit 1 enables 10-bit AV1 PQ,
bit 2 additionally permits 10-bit AV1 4:4:4, bit 3 permits 8-bit AV1 4:4:4,
and bit 4 permits H.264 4:4:4. Absence means SDR. Unknown
bits, repeated tags, and lengths other than one are invalid. Existing packed
`COLOR_SPACE` metadata carries the four CICP bytes. A color change rebuilds
the encoder and starts a keyframe; receivers reconfigure at that boundary.
AV1 codec strings describe the actual profile and depth: Main for 4:2:0,
High for 8/10-bit 4:4:4, and 10-bit for HDR streaming. H.264 SPS VUI metadata
is updated for managed output across software and hardware encoders.

## Validation and shader rebuild

`cargo test -p yas-compositor --test color_management` exercises real Wayland
10/16-bit SHM buffers, P3/DCI-P3/PQ/scRGB transforms, custom primaries and
power curves, mastering metadata, ICC lookup sampling/lifetime, alpha, and return to SDR on a Vulkan device
(lavapipe is sufficient). Server tests independently decode AV1 P3/HDR in
4:2:0 and 4:4:4 using rav1d, and P3 H.264 using OpenH264. They also check
capture precision/metadata and odd-sized chroma conversion. Browser tests
cover color transitions, P3 intermediates, and rejection of an HDR path that
clips highlights.

The ignored `managed_hardware_bitstreams` test requires NVIDIA H.264/AV1
encoding (including H.264 4:4:4), an Intel VA-API H.264 encoder at `/dev/dri/renderD128`, and `ffprobe`.
It checks actual backend selection and bitstream metadata. On this development
host, NVENC P3 H.264, P3 AV1, HDR AV1, and Intel VA-API P3 H.264 have also been
independently decoded with FFmpeg to verify pixel levels. VA-API AV1 HDR needs
validation on a GPU with AV1 encoding support.

The ignored `vulkan_video_color_pixels_and_metadata` test selects a render
node through `YAS_COLOR_GPU` and decodes P3 H.264/AV1 and native/scaled HDR AV1
with FFmpeg. `YAS_COLOR_PROBE_DIRECTORY` optionally retains streams for
independent inspection. This host's NVIDIA GPU passes those paths under Vulkan
validation; its Vulkan H.264 4:4:4 encode fails and falls back. Set
`YAS_COLOR_VULKAN_H264_444=1` to also exercise that profile and its explicit
refusal path.

The ignored `direct_gpu_color_pixels_and_metadata` test exercises real Wayland
10-bit input, native/scaled GPU conversion, and three hardware-encoded frames.
Select `YAS_COLOR_DIRECT_BACKEND=nvenc`, `vaapi`, or `vaapi-av1`, and set `YAS_COLOR_GPU` to
the matching render node. FFmpeg checks pixel levels and color metadata;
`YAS_COLOR_PROBE_DIRECTORY` retains the streams. NVENC covers HDR/P3 AV1 and
P3 H.264 4:2:0/4:4:4; VA-API covers P3 H.264 or HDR/P3 AV1 4:2:0/4:4:4.
`direct_gpu_yuv_surfaces` additionally maps VA-API P010, 444P, AYUV, Y410, and
Y416 output and checks that CPU uploads use the same byte layout as GPU writes.
It runs even on GPUs without AV1 encoding. `direct_gpu_mixed_viewers` checks
simultaneous SDR/P3/HDR and 4:2:0/4:4:4 output, pool reuse, viewer removal,
and concurrent CPU readback. `direct_gpu_mixed_vulkan_viewers` independently
decodes simultaneous Vulkan H.264/AV1 SDR/P3/HDR streams. The
`direct_gpu_mastering_tone_mapping` test compares mastering-aware GPU output
with CPU conversion.

The ignored `screencast_delivers_negotiated_hdr_and_p3_pixels` test starts an
isolated PipeWire daemon and WirePlumber, then verifies byte-exact delivery of
all five formats to `crates/server/tests/fixtures/video_consumer.c`. Compile
that consumer with the PipeWire development flags and set
`YAS_PIPEWIRE_VIDEO_CONSUMER` to its executable path.

The compositor shader source and SPIR-V are checked in together. Rebuild all
shaders, including the nine managed video variants, after editing them:

```sh
direnv exec . ./bin/build-shaders
```

Use `./bin/build-shaders --check` to verify that the checked-in binaries match
the sources without changing them. The script uses `glslangValidator` from
PATH, or the repository's pinned Nix dependency when it is unavailable. All
variants compile successfully before any checked-in output is replaced.
