import { afterEach, describe, expect, it, vi } from "vitest";
import { SurfaceStore } from "../SurfaceStore";
import { YasSurfaceCanvas } from "../YasSurfaceCanvas";
import type { YasWorkspace } from "../YasWorkspace";
import type { YasSurface } from "../types";

const disposals: (() => void)[] = [];
afterEach(() => {
  for (const dispose of disposals.splice(0)) dispose();
  vi.restoreAllMocks();
});

function mount() {
  let info: YasSurface | undefined;
  let source: HTMLCanvasElement | null = null;
  let changed = () => {};
  let frame = (_: bigint) => {};
  const store = {
    generation: 0,
    canDecodeVideo: true,
    getSurface: () => info,
    getCanvas: () => source,
    getCursor: () => "default",
    onCursor: () => () => {},
    onChange: (cb: () => void) => {
      changed = cb;
      return () => {};
    },
    onFrame: (cb: (id: bigint) => void) => {
      frame = cb;
      return () => {};
    },
  };
  const connection = { surfaceStore: store };
  const view = new YasSurfaceCanvas({
    workspace: {
      getConnection: () => connection,
      subscribe: () => () => {},
    } as unknown as YasWorkspace,
    connectionId: "test",
    surfaceId: 1n,
    resizable: true,
  });
  view.attach(document.createElement("div"));
  disposals.push(() => view.dispose());
  const canvas = view.canvasElement!;
  return {
    view,
    canvas,
    announce() {
      info = { width: 8192, height: 4352 } as YasSurface;
      changed();
    },
    present() {
      source ??= document.createElement("canvas");
      source.width = 1494;
      source.height = 1788;
      frame(1n);
    },
  };
}

describe("surface canvas recovery", () => {
  it("retries a failed context allocation when a frame arrives", () => {
    let available = false;
    const drawImage = vi.fn();
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation(
      (() => (available ? { drawImage } : null)) as never,
    );
    const { canvas, present } = mount();
    present();
    expect(drawImage).not.toHaveBeenCalled();
    available = true;
    present();
    expect(drawImage).toHaveBeenCalledOnce();
    expect([canvas.width, canvas.height]).toEqual([1494, 1788]);
  });

  it("repaints a restored visible canvas even when the app is idle", () => {
    let lost = false;
    const drawImage = vi.fn();
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
      drawImage,
      isContextLost: () => lost,
    } as never);
    const { view, canvas, present } = mount();
    present();
    drawImage.mockClear();
    lost = true;
    canvas.dispatchEvent(new Event("contextlost"));
    present();
    expect(drawImage).not.toHaveBeenCalled();
    lost = false;
    canvas.dispatchEvent(new Event("contextrestored"));
    expect(drawImage).toHaveBeenCalledOnce();
    view.dispose();
    expect([canvas.width, canvas.height]).toEqual([0, 0]);
    canvas.dispatchEvent(new Event("contextrestored"));
    expect(drawImage).toHaveBeenCalledOnce();
  });

  it("does not allocate another viewer's native size on catalogue arrival", () => {
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
      drawImage: vi.fn(),
    } as never);
    const { canvas, announce, present } = mount();
    announce();
    expect(canvas.width * canvas.height).toBeLessThan(1494 * 1788);
    present();
    expect([canvas.width, canvas.height]).toEqual([1494, 1788]);
  });

  it.each(["reset", "handleDisconnect", "handleSurfaceDestroyed"] as const)(
    "%s releases backing memory and retires restoration callbacks",
    (operation) => {
      vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
        drawImage: vi.fn(),
      } as never);
      const store = new SurfaceStore();
      disposals.push(() => store.destroy());
      const keyframe = vi.fn();
      store.setKeyframeSender(keyframe);
      (store as any).ensureCanvas(1n, 3002, 3140);
      const canvas = store.getCanvas(1n)!;
      canvas.dispatchEvent(new Event("contextrestored"));
      expect(keyframe).toHaveBeenCalledWith(1n);
      keyframe.mockClear();
      if (operation === "handleSurfaceDestroyed") store[operation](1n);
      else store[operation]();
      expect([canvas.width, canvas.height]).toEqual([0, 0]);
      expect(store.getCanvas(1n)).toBeNull();
      canvas.dispatchEvent(new Event("contextrestored"));
      expect(keyframe).not.toHaveBeenCalled();
    },
  );
});
