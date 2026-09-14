import { afterEach, describe, expect, it, vi } from "vitest";
import { SDR_COLOR, surface2DContext, videoColorSpace } from "../surfaceColor";
import {
  surfaceColorCapabilities,
  encodeSurfaceOpenView,
} from "../yas/surface";
import { YAS_SURFACE_VIEW_COLOR_CAPABILITIES_EXTENSION as TAG } from "../yas/generated";

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("Surface color negotiation", () => {
  it("advertises P3 for an H.264-only viewer without WebGPU", async () => {
    vi.resetModules();
    vi.stubGlobal(
      "VideoDecoder",
      class {
        static isConfigSupported = vi.fn(async () => ({ supported: false }));
      },
    );
    vi.stubGlobal("matchMedia", (query: string) => ({
      matches: query === "(color-gamut: p3)",
      addEventListener: vi.fn(),
    }));
    vi.stubGlobal("navigator", {});
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
      getContextAttributes: () => ({ colorSpace: "display-p3" }),
    } as never);
    const color = await import("../surfaceColor");
    expect(await color.detectSurfaceColorCapabilities()).toBe(1);
  });
  it("keeps legacy SDR metadata and maps P3/HDR CICP explicitly", () => {
    expect(videoColorSpace()).toEqual(SDR_COLOR);
    expect(
      videoColorSpace({ primaries: 12, transfer: 13, matrix: 1, range: 0 }),
    ).toEqual({
      primaries: "smpte432",
      transfer: "iec61966-2-1",
      matrix: "bt709",
      fullRange: false,
    });
    expect(
      videoColorSpace({ primaries: 9, transfer: 16, matrix: 9, range: 0 }),
    ).toEqual({
      primaries: "bt2020",
      transfer: "pq",
      matrix: "bt2020-ncl",
      fullRange: false,
    });
    expect(() =>
      videoColorSpace({ primaries: 255, transfer: 16, matrix: 9, range: 0 }),
    ).toThrow();
  });
  it("preserves P3 on every 2D intermediate and falls back when unsupported", () => {
    const context = {};
    const getContext = vi.fn().mockReturnValue(context);
    expect(
      surface2DContext({ getContext } as unknown as HTMLCanvasElement),
    ).toBe(context);
    expect(getContext).toHaveBeenCalledWith("2d", { colorSpace: "display-p3" });
    getContext.mockImplementationOnce(() => {
      throw new Error("unsupported color space");
    });
    expect(
      surface2DContext({ getContext } as unknown as HTMLCanvasElement),
    ).toBe(context);
    expect(getContext).toHaveBeenLastCalledWith("2d");
    getContext.mockReturnValueOnce(null);
    expect(
      surface2DContext({ getContext } as unknown as HTMLCanvasElement),
    ).toBe(context);
    getContext.mockReturnValue(null);
    expect(
      surface2DContext({ getContext } as unknown as HTMLCanvasElement),
    ).toBeNull();
  });
  it("validates masks without changing legacy OPEN_VIEW bytes", () => {
    const ext = (value: number[]) => ({
      tag: TAG,
      required: false,
      value: new Uint8Array(value),
    });
    expect(surfaceColorCapabilities()).toBe(0);
    for (let mask = 0; mask < 32; mask++)
      expect(surfaceColorCapabilities([ext([mask])])).toBe(mask);
    for (const invalid of [
      [ext([])],
      [ext([32])],
      [ext([1, 2])],
      [ext([1]), ext([2])],
    ])
      expect(() => surfaceColorCapabilities(invalid)).toThrow();
    const base = {
      surfaceHandle: 1n,
      width: 1920,
      height: 1080,
      maxFps: 60,
      decoderCapacity: 3,
      codecVersions: [2],
    };
    expect(encodeSurfaceOpenView(base)).toEqual(
      encodeSurfaceOpenView({ ...base, extensions: [] }),
    );
    expect(() =>
      encodeSurfaceOpenView({ ...base, extensions: [ext([32])] }),
    ).toThrow();
  });
});

describe("HDR presentation capability probe", () => {
  it.each([true, false])(
    "advertises HDR only when actual decoded highlights survive (retained=%s)",
    async (retained) => {
      vi.resetModules();
      const frame = { close: vi.fn(), colorSpace: { transfer: "pq" } };
      vi.stubGlobal(
        "VideoDecoder",
        class {
          static isConfigSupported = vi.fn(async () => ({ supported: true }));
          state = "unconfigured";
          constructor(private init: { output: (frame: unknown) => void }) {}
          configure() {
            this.state = "configured";
          }
          decode() {
            queueMicrotask(() => this.init.output(frame));
          }
          flush() {
            return Promise.resolve();
          }
          close() {
            this.state = "closed";
          }
        },
      );
      vi.stubGlobal(
        "EncodedVideoChunk",
        class {
          constructor(_: unknown) {}
        },
      );
      vi.stubGlobal("matchMedia", () => ({
        matches: true,
        addEventListener: vi.fn(),
      }));
      const samples = new Uint16Array(128);
      samples.set([
        retained ? 0x4000 : 0x3c00,
        retained ? 0x4000 : 0x3c00,
        retained ? 0x4000 : 0x3c00,
      ]);
      const pass = {
        setPipeline: vi.fn(),
        setBindGroup: vi.fn(),
        draw: vi.fn(),
        end: vi.fn(),
      };
      const device = {
        pushErrorScope: vi.fn(),
        popErrorScope: vi.fn(async () => null),
        createShaderModule: vi.fn(() => ({})),
        createRenderPipelineAsync: vi.fn(async () => ({
          getBindGroupLayout: () => ({}),
        })),
        createSampler: vi.fn(() => ({})),
        createTexture: vi.fn((_: unknown) => ({
          createView: () => ({}),
          destroy: vi.fn(),
        })),
        createBuffer: vi.fn(() => ({
          mapAsync: async () => {},
          getMappedRange: () => samples.buffer,
          destroy: vi.fn(),
        })),
        importExternalTexture: vi.fn(() => ({})),
        createBindGroup: vi.fn(() => ({})),
        createCommandEncoder: vi.fn(() => ({
          beginRenderPass: () => pass,
          copyTextureToBuffer: vi.fn(),
          finish: () => ({}),
        })),
        queue: { submit: vi.fn() },
        destroy: vi.fn(),
        addEventListener: vi.fn(),
        lost: new Promise(() => {}),
      };
      const context = {
        configure: vi.fn(),
        getConfiguration: () => ({ toneMapping: { mode: "extended" } }),
        unconfigure: vi.fn(),
        getCurrentTexture: () => ({ createView: () => ({}) }),
      };
      vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation(((
        kind: string,
      ) =>
        kind === "webgpu"
          ? context
          : {
              getContextAttributes: () => ({ colorSpace: "display-p3" }),
            }) as never);
      vi.stubGlobal("navigator", {
        gpu: {
          requestAdapter: async () => ({ requestDevice: async () => device }),
        },
      });
      const color = await import("../surfaceColor");
      expect(await color.detectSurfaceColorCapabilities()).toBe(
        retained ? 31 : 25,
      );
      expect(frame.close).toHaveBeenCalledOnce();
      expect(device.importExternalTexture).toHaveBeenCalledWith({
        source: frame,
        colorSpace: "display-p3",
      });
      const presenter = color.SurfaceHdrPresenter.create();
      if (retained) {
        expect(presenter).not.toBeNull();
        const base = document.createElement("canvas");
        const liveFrame = {
          colorSpace: { transfer: "pq" },
        } as unknown as VideoFrame;
        expect(presenter!.draw(liveFrame, base)).toBe(true);
        expect(device.importExternalTexture).toHaveBeenLastCalledWith({
          source: liveFrame,
          colorSpace: "display-p3",
        });
        expect(presenter!.canvas.style.pointerEvents).toBe("none");
        base.width = base.height = 2;
        const thumbnailFrame = {
          displayWidth: 16,
          displayHeight: 16,
        } as VideoFrame;
        device.createTexture.mockClear();
        pass.draw.mockClear();
        expect(presenter!.draw(thumbnailFrame, base)).toBe(true);
        expect(
          device.createTexture.mock.calls.map(([options]) => options),
        ).toEqual([
          { size: [8, 8], format: "rgba16float", usage: 20 },
          { size: [4, 4], format: "rgba16float", usage: 20 },
        ]);
        expect(pass.draw).toHaveBeenCalledTimes(3);
        expect(presenter!.draw(thumbnailFrame, base)).toBe(true);
        expect(device.createTexture).toHaveBeenCalledTimes(2);
        const targets = device.createTexture.mock.results.map(
          (result) => result.value,
        );
        presenter!.dispose();
        for (const target of targets)
          expect(target.destroy).toHaveBeenCalledOnce();
        color.rejectSurfaceColorProfile("av01.1.08M.10", true);
        expect(await color.detectSurfaceColorCapabilities()).toBe(27);
        color.rejectSurfaceColorProfile("av01.0.08M.10", true);
        expect(await color.detectSurfaceColorCapabilities()).toBe(25);
        expect(color.SurfaceHdrPresenter.create()).toBeNull();
        expect(device.destroy).toHaveBeenCalledOnce();
      } else {
        expect(presenter).toBeNull();
        expect(device.destroy).toHaveBeenCalledOnce();
      }
    },
  );
});
