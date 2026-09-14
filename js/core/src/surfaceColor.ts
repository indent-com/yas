import { SURFACE_HDR_PROBE } from "./surfaceHdrProbe";
import type { YasSurfaceColorSpace } from "./yas/packed";
import type { YasExtension } from "./yas/wire";
import {
  YAS_SURFACE_COLOR_CAP_DISPLAY_P3,
  YAS_SURFACE_COLOR_CAP_HDR10_AV1,
  YAS_SURFACE_COLOR_CAP_HDR10_AV1_444,
  YAS_SURFACE_COLOR_CAP_AV1_444,
  YAS_SURFACE_COLOR_CAP_H264_444,
  YAS_SURFACE_VIEW_COLOR_CAPABILITIES_EXTENSION,
} from "./yas/generated";

export const SDR_COLOR: VideoColorSpaceInit = {
  primaries: "bt709",
  transfer: "iec61966-2-1",
  matrix: "smpte170m",
  fullRange: false,
};

export function videoColorSpace(
  color?: YasSurfaceColorSpace,
): VideoColorSpaceInit {
  if (!color) return SDR_COLOR;
  const primaries = ({ 1: "bt709", 9: "bt2020", 12: "smpte432" } as const)[
    color.primaries as 1 | 9 | 12
  ];
  const transfer = (
    { 1: "bt709", 13: "iec61966-2-1", 16: "pq", 18: "hlg" } as const
  )[color.transfer as 1 | 13 | 16 | 18];
  const matrix = ({ 1: "bt709", 6: "smpte170m", 9: "bt2020-ncl" } as const)[
    color.matrix as 1 | 6 | 9
  ];
  if (!primaries || !transfer || !matrix || color.range > 1)
    throw new Error("Unsupported YAS Surface color description");
  // lib.dom still lists the original SDR-only WebCodecs enums.
  return {
    primaries,
    transfer,
    matrix,
    fullRange: color.range === 1,
  } as unknown as VideoColorSpaceInit;
}

export function surface2DContext(
  canvas: HTMLCanvasElement,
): CanvasRenderingContext2D | null {
  try {
    return canvas.getContext("2d", { colorSpace: "display-p3" });
  } catch {
    return canvas.getContext("2d");
  }
}

let capabilities = 0;
let probe: Promise<number> | undefined;
let gpu:
  | {
      device: GPUDevice;
      pipeline: GPURenderPipeline;
      texturePipeline: GPURenderPipeline;
      sampler: GPUSampler;
    }
  | undefined;
const listeners = new Set<() => void>();
let watching = false;

export function onSurfaceColorChange(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}
export function surfaceColorExtensions(): YasExtension[] {
  return capabilities
    ? [
        {
          tag: YAS_SURFACE_VIEW_COLOR_CAPABILITIES_EXTENSION,
          required: false,
          value: new Uint8Array([capabilities]),
        },
      ]
    : [];
}

const shader = `
struct Vertex { @builtin(position) position: vec4f, @location(0) uv: vec2f }
@vertex fn vs(@builtin(vertex_index) index: u32) -> Vertex {
  let uv = vec2f(f32((index << 1u) & 2u), f32(index & 2u));
  return Vertex(vec4f(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
}
@group(0) @binding(0) var frame: texture_external;
@group(0) @binding(1) var frameSampler: sampler;
@fragment fn fs(in: Vertex) -> @location(0) vec4f {
  return textureSampleBaseClampToEdge(frame, frameSampler, in.uv);
}`;

const textureShader = shader
  .replace("texture_external", "texture_2d<f32>")
  .replace(
    "textureSampleBaseClampToEdge(frame, frameSampler, in.uv)",
    "textureSample(frame, frameSampler, in.uv)",
  );

async function decodeHdrProbe(): Promise<VideoFrame | null> {
  return new Promise((resolve) => {
    let settled = false;
    let decoder: VideoDecoder | undefined;
    const finish = (frame: VideoFrame | null) => {
      if (settled) {
        frame?.close();
        return;
      }
      settled = true;
      clearTimeout(timer);
      if (decoder?.state !== "closed") decoder?.close();
      resolve(frame);
    };
    const timer = setTimeout(() => finish(null), 2000);
    try {
      decoder = new VideoDecoder({ output: finish, error: () => finish(null) });
      decoder.configure({
        codec: "av01.0.08M.10",
        codedWidth: 64,
        codedHeight: 64,
        colorSpace: videoColorSpace({
          primaries: 9,
          transfer: 16,
          matrix: 9,
          range: 0,
        }),
      });
      decoder.decode(
        new EncodedVideoChunk({
          type: "key",
          timestamp: 0,
          data: SURFACE_HDR_PROBE,
        }),
      );
      void decoder.flush().catch(() => finish(null));
    } catch {
      finish(null);
    }
  });
}

/** Prove decoding AND external-texture conversion retain highlights. Some
 * implementations accept extended canvases but clamp video imports to SDR. */
async function verifyHdrPipeline(
  state: NonNullable<typeof gpu>,
): Promise<boolean> {
  const frame = await decodeHdrProbe();
  if (!frame) return false;
  const { device, pipeline, sampler } = state;
  let target: GPUTexture | undefined;
  let readback: GPUBuffer | undefined;
  try {
    target = device.createTexture({
      size: [1, 1],
      format: "rgba16float",
      usage: 0x10 | 0x01 /* RENDER_ATTACHMENT | COPY_SRC */,
    });
    readback = device.createBuffer({
      size: 256,
      usage: 0x08 | 0x01 /* COPY_DST | MAP_READ */,
    });
    const texture = device.importExternalTexture({
      source: frame,
      colorSpace: "display-p3",
    });
    const bindings = device.createBindGroup({
      layout: pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: texture },
        { binding: 1, resource: sampler },
      ],
    });
    const encoder = device.createCommandEncoder();
    const pass = encoder.beginRenderPass({
      colorAttachments: [
        { view: target.createView(), loadOp: "clear", storeOp: "store" },
      ],
    });
    pass.setPipeline(pipeline);
    pass.setBindGroup(0, bindings);
    pass.draw(3);
    pass.end();
    encoder.copyTextureToBuffer(
      { texture: target },
      { buffer: readback, bytesPerRow: 256 },
      [1, 1],
    );
    device.queue.submit([encoder.finish()]);
    await readback.mapAsync(0x01 /* READ */);
    const rgb = new Uint16Array(readback.getMappedRange(), 0, 3);
    // Positive finite half floats above 1.0 (0x3c00). The exact headroom
    // depends on the browser's SDR white, so only test that it survives.
    return [...rgb].every((v) => v > 0x3c00 && v < 0x7c00);
  } catch {
    return false;
  } finally {
    readback?.destroy();
    target?.destroy();
    frame.close();
  }
}

function disableSurfaceHdr(device: GPUDevice): void {
  if (gpu?.device !== device) return;
  gpu = undefined;
  device.destroy();
  capabilities &= ~(
    YAS_SURFACE_COLOR_CAP_HDR10_AV1 | YAS_SURFACE_COLOR_CAP_HDR10_AV1_444
  );
  probe = Promise.resolve(capabilities);
  for (const listener of listeners) listener();
}

/** A real stream can fail despite isConfigSupported(). Retire that profile
 * after repeated decoder failures and reopen views with a compatible one. */
export function rejectSurfaceColorProfile(
  codec: string | undefined,
  hdr: boolean,
): void {
  const highAv1 = codec?.startsWith("av01.1.");
  const mask = highAv1
    ? hdr
      ? YAS_SURFACE_COLOR_CAP_HDR10_AV1_444
      : YAS_SURFACE_COLOR_CAP_AV1_444
    : codec?.toLowerCase().startsWith("avc1.f4")
      ? YAS_SURFACE_COLOR_CAP_H264_444
      : hdr
        ? YAS_SURFACE_COLOR_CAP_HDR10_AV1 | YAS_SURFACE_COLOR_CAP_HDR10_AV1_444
        : 0;
  if (!(capabilities & mask)) return;
  if (mask & YAS_SURFACE_COLOR_CAP_HDR10_AV1 && gpu) {
    disableSurfaceHdr(gpu.device);
    return;
  }
  capabilities &= ~mask;
  probe = Promise.resolve(capabilities);
  for (const listener of listeners) listener();
}

export function detectSurfaceColorCapabilities(): Promise<number> {
  return (probe ??= (async () => {
    if (typeof document === "undefined" || typeof VideoDecoder === "undefined")
      return 0;
    let next = 0;
    try {
      const canvas = document.createElement("canvas");
      const context = surface2DContext(canvas);
      if (
        context?.getContextAttributes?.().colorSpace === "display-p3" &&
        typeof matchMedia === "function" &&
        matchMedia("(color-gamut: p3)").matches
      ) {
        next |= YAS_SURFACE_COLOR_CAP_DISPLAY_P3;
      }
      if (typeof VideoDecoder !== "undefined") {
        for (const [codec, flag] of [
          ["av01.1.08M.08", YAS_SURFACE_COLOR_CAP_AV1_444],
          ["avc1.f40033", YAS_SURFACE_COLOR_CAP_H264_444],
        ] as const) {
          try {
            if (
              (
                await VideoDecoder.isConfigSupported({
                  codec,
                  codedWidth: 1920,
                  codedHeight: 1080,
                })
              ).supported
            )
              next |= flag;
          } catch {
            /* Keep independently proven capabilities. */
          }
        }
      }
      if (typeof matchMedia === "function" && !watching) {
        watching = true;
        for (const query of ["(dynamic-range: high)", "(color-gamut: p3)"]) {
          matchMedia(query).addEventListener("change", () => {
            probe = undefined;
            void detectSurfaceColorCapabilities().then(() => {
              for (const listener of listeners) listener();
            });
          });
        }
      }
      if (
        (next & YAS_SURFACE_COLOR_CAP_DISPLAY_P3) !== 0 &&
        typeof matchMedia === "function" &&
        matchMedia("(dynamic-range: high)").matches &&
        navigator.gpu &&
        (
          await VideoDecoder.isConfigSupported({
            codec: "av01.0.08M.10",
            codedWidth: 1920,
            codedHeight: 1080,
            colorSpace: videoColorSpace({
              primaries: 9,
              transfer: 16,
              matrix: 9,
              range: 0,
            }),
          })
        ).supported
      ) {
        if (!gpu) {
          const adapter = await navigator.gpu.requestAdapter();
          const device = await adapter?.requestDevice();
          if (device) {
            let accepted = false;
            const probeCanvas = document.createElement("canvas");
            const context = probeCanvas.getContext(
              "webgpu",
            ) as GPUCanvasContext | null;
            device.pushErrorScope("validation");
            try {
              context?.configure({
                device,
                format: "rgba16float",
                colorSpace: "display-p3",
                toneMapping: { mode: "extended" },
                alphaMode: "opaque",
              });
              const configured =
                context?.getConfiguration()?.toneMapping?.mode === "extended";
              const module = device.createShaderModule({ code: shader });
              const pipeline = await device.createRenderPipelineAsync({
                layout: "auto",
                vertex: { module, entryPoint: "vs" },
                fragment: {
                  module,
                  entryPoint: "fs",
                  targets: [{ format: "rgba16float" }],
                },
                primitive: { topology: "triangle-list" },
              });
              const textureModule = device.createShaderModule({
                code: textureShader,
              });
              const texturePipeline = await device.createRenderPipelineAsync({
                layout: "auto",
                vertex: { module: textureModule, entryPoint: "vs" },
                fragment: {
                  module: textureModule,
                  entryPoint: "fs",
                  targets: [{ format: "rgba16float" }],
                },
                primitive: { topology: "triangle-list" },
              });
              const candidate = {
                device,
                pipeline,
                texturePipeline,
                sampler: device.createSampler({
                  minFilter: "linear",
                  magFilter: "linear",
                }),
              };
              if (configured && (await verifyHdrPipeline(candidate))) {
                gpu = candidate;
                accepted = true;
              }
            } finally {
              const error = await device.popErrorScope();
              context?.unconfigure();
              if (error || !accepted) {
                if (gpu?.device === device) gpu = undefined;
                device.destroy();
              } else {
                void device.lost.then(() => disableSurfaceHdr(device));
                device.addEventListener("uncapturederror", () =>
                  disableSurfaceHdr(device),
                );
              }
            }
          }
        }
        if (gpu) {
          next |= YAS_SURFACE_COLOR_CAP_HDR10_AV1;
          if (
            (
              await VideoDecoder.isConfigSupported({
                codec: "av01.1.08M.10",
                codedWidth: 1920,
                codedHeight: 1080,
                colorSpace: videoColorSpace({
                  primaries: 9,
                  transfer: 16,
                  matrix: 9,
                  range: 0,
                }),
              })
            ).supported
          ) {
            next |= YAS_SURFACE_COLOR_CAP_HDR10_AV1_444;
          }
        }
      }
    } catch {
      /* Retain any proven SDR/P3 path. */
    }
    if (!(next & YAS_SURFACE_COLOR_CAP_HDR10_AV1) && gpu) {
      const device = gpu.device;
      gpu = undefined;
      device.destroy();
    }
    capabilities = next;
    return next;
  })());
}

/** A direct VideoFrame → extended-range canvas path. No SDR intermediate. */
export class SurfaceHdrPresenter {
  readonly canvas = document.createElement("canvas");
  private context: GPUCanvasContext;
  private state: NonNullable<typeof gpu>;
  private intermediates: GPUTexture[] = [];
  private intermediateKey = "";

  static create(): SurfaceHdrPresenter | null {
    if (!gpu || !(capabilities & YAS_SURFACE_COLOR_CAP_HDR10_AV1)) return null;
    try {
      return new SurfaceHdrPresenter(gpu);
    } catch {
      disableSurfaceHdr(gpu.device);
      return null;
    }
  }
  private constructor(state: NonNullable<typeof gpu>) {
    this.state = state;
    const context = this.canvas.getContext("webgpu") as GPUCanvasContext | null;
    if (!context) throw new Error("WebGPU canvas unavailable");
    this.context = context;
    context.configure({
      device: state.device,
      format: "rgba16float",
      colorSpace: "display-p3",
      toneMapping: { mode: "extended" },
      alphaMode: "opaque",
    });
    this.canvas.setAttribute("aria-hidden", "true");
    this.canvas.style.pointerEvents = "none";
  }
  draw(frame: VideoFrame, base: HTMLCanvasElement): boolean {
    if (gpu !== this.state || !(capabilities & YAS_SURFACE_COLOR_CAP_HDR10_AV1))
      return false;
    const { device, pipeline, sampler } = this.state;
    if (base.width < 1 || base.height < 1) return false;
    try {
      if (this.canvas.width !== base.width) this.canvas.width = base.width;
      if (this.canvas.height !== base.height) this.canvas.height = base.height;
      this.syncLayout(base);
      const texture = device.importExternalTexture({
        source: frame,
        colorSpace: "display-p3",
      });
      // Repeated halving keeps small workspace previews legible. Every
      // intermediate stays float16 so highlights survive all filtering passes.
      const sizes: [number, number][] = [];
      let w = frame.displayWidth || base.width;
      let h = frame.displayHeight || base.height;
      while (w > base.width * 2 || h > base.height * 2) {
        w = Math.max(base.width, Math.ceil(w / 2));
        h = Math.max(base.height, Math.ceil(h / 2));
        sizes.push([w, h]);
      }
      const key = JSON.stringify(sizes);
      if (key !== this.intermediateKey) {
        for (const target of this.intermediates) target.destroy();
        this.intermediates = [];
        this.intermediateKey = "";
        for (const size of sizes)
          this.intermediates.push(
            device.createTexture({
              size,
              format: "rgba16float",
              usage: 0x10 | 0x04 /* RENDER_ATTACHMENT | TEXTURE_BINDING */,
            }),
          );
        this.intermediateKey = key;
      }
      const encoder = device.createCommandEncoder();
      for (let i = 0; i <= this.intermediates.length; i++) {
        const activePipeline = i === 0 ? pipeline : this.state.texturePipeline;
        const resource =
          i === 0 ? texture : this.intermediates[i - 1]!.createView();
        const bindings = device.createBindGroup({
          layout: activePipeline.getBindGroupLayout(0),
          entries: [
            { binding: 0, resource },
            { binding: 1, resource: sampler },
          ],
        });
        const target =
          this.intermediates[i] ?? this.context.getCurrentTexture();
        const pass = encoder.beginRenderPass({
          colorAttachments: [
            {
              view: target.createView(),
              loadOp: "clear",
              storeOp: "store",
              clearValue: [0, 0, 0, 1],
            },
          ],
        });
        pass.setPipeline(activePipeline);
        pass.setBindGroup(0, bindings);
        pass.draw(3);
        pass.end();
      }
      device.queue.submit([encoder.finish()]);
      return true;
    } catch {
      disableSurfaceHdr(device);
      return false;
    }
  }
  syncLayout(base: HTMLCanvasElement): void {
    this.canvas.style.cssText = base.style.cssText;
    this.canvas.style.pointerEvents = "none";
    this.canvas.style.opacity = "1";
    this.canvas.style.position = "absolute";
    this.canvas.style.left ||= "0";
    this.canvas.style.top ||= "0";
  }
  dispose(): void {
    for (const target of this.intermediates) target.destroy();
    this.intermediates = [];
    this.context.unconfigure();
    this.canvas.remove();
  }
}
