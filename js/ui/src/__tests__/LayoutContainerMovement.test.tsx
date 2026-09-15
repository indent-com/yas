import { PALETTES, type YasWorkspace } from "@yas-run/core";
import { enumeratePanes, type WorkspaceLayout } from "@yas-run/core/layout";
import { YasWorkspaceProvider } from "@yas-run/solid";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { handlePrefixKey, disarmPrefix } from "../keyPrefix";
import { LayoutContainer } from "../layout/LayoutContainer";
import { surfaceWorkspaceRef, type LayoutAssignments } from "../layout/store";

const { workspace, frames, frameListeners } = vi.hoisted(() => {
  const frames = new Map<bigint, HTMLCanvasElement>();
  const frameListeners = new Set<(id: bigint) => void>();
  const mounts = new Map<bigint, Set<string>>();
  let nextView = 0;
  const connection = {
    surfaceStore: {
      getSurface: () => ({
        width: 900,
        height: 300,
        logicalWidth: 900,
        logicalHeight: 300,
      }),
      getCanvas: (id: bigint) => frames.get(id) ?? null,
      getSurfaces: () => new Map(),
      getCursor: () => "default",
      canDecodeVideo: true,
      generation: 0,
      onChange: () => () => {},
      onCursor: () => () => {},
      onFrame: (listener: (id: bigint) => void) => {
        frameListeners.add(listener);
        return () => frameListeners.delete(listener);
      },
    },
    allocSurfaceViewId: () => String(++nextView),
    offerSurfaceViewSize: () => true,
    withdrawSurfaceViewSize: () => {},
    sendSurfaceFocus: () => {},
    sendSurfaceSubscribe: (id: bigint, view: string) => {
      if (!mounts.has(id)) mounts.set(id, new Set());
      mounts.get(id)!.add(view);
    },
    sendSurfaceUnsubscribe: (id: bigint, view: string) => {
      const owners = mounts.get(id);
      owners?.delete(view);
      // The native connection releases the backing frame with the last view.
      if (owners?.size === 0) frames.delete(id);
    },
  };
  return {
    frames,
    frameListeners,
    workspace: {
      getConnection: () => connection,
      setVisibleSessions: () => {},
      subscribe: () => () => {},
    },
  };
});

vi.mock("@yas-run/core", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@yas-run/core")>()),
  detectCodecSupport: () => {},
}));

vi.mock("@yas-run/solid", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@yas-run/solid")>()),
  createYasWorkspace: () => workspace,
  createYasWorkspaceState: () => () => ({
    sessions: [],
    connections: [{ id: "dev", status: "connected", ready: true }],
    focusedSessionId: null,
  }),
  createYasSessions: () => () => [],
}));

let dispose: (() => void) | undefined;
beforeEach(() => {
  vi.useFakeTimers();
  vi.stubGlobal("devicePixelRatio", 1);
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  // Track pixel provenance while keeping the real canvas and Solid binding.
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation(
    function (this: HTMLCanvasElement) {
      const canvas = this;
      return {
        drawImage(source: HTMLCanvasElement) {
          canvas.dataset.pixels = source.dataset.pixels;
        },
        clearRect() {
          delete canvas.dataset.pixels;
        },
      } as unknown as CanvasRenderingContext2D;
    },
  );
});
afterEach(() => {
  dispose?.();
  disarmPrefix();
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  frames.clear();
  frameListeners.clear();
  localStorage.clear();
  document.body.replaceChildren();
});

function presentFrame(id: bigint) {
  const frame = document.createElement("canvas");
  frame.width = 900;
  frame.height = 300;
  frame.dataset.pixels = `surface:dev:${id}`;
  frames.set(id, frame);
  for (const listener of frameListeners) listener(id);
}

it("never displays another window's pixels after C-b Shift-Up swaps tiled surfaces", async () => {
  const [layout, setLayout] = createSignal<WorkspaceLayout>({
    name: "Movement regression",
    root: {
      type: "split",
      direction: "vertical",
      children: [
        { node: { type: "leaf" }, weight: 1 },
        { node: { type: "leaf" }, weight: 1 },
      ],
    },
  });
  let assignments: LayoutAssignments | undefined;
  let refs: Readonly<Record<string, string>> = {};
  let focusedPaneId: string | null = null;
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
    function (this: HTMLElement) {
      const id =
        this.closest("[data-yas-pane-id]")?.getAttribute("data-yas-pane-id");
      return new DOMRect(0, id === "1" ? 300 : 0, 900, id == null ? 600 : 300);
    },
  );
  presentFrame(7n);
  presentFrame(9n);
  dispose = render(
    () => (
      <YasWorkspaceProvider workspace={workspace as unknown as YasWorkspace}>
        <LayoutContainer
          layout={layout()}
          onLayoutChange={(next) => next && setLayout(next)}
          connectionId="dev"
          palette={PALETTES[0]}
          fontFamily="monospace"
          fontSize={14}
          focusedSessionId={null}
          lruSessionIds={[]}
          liveSurfaceKeys={["dev:7", "dev:9"]}
          storedAssignments={{
            "0": surfaceWorkspaceRef("dev", 7n),
            "1": surfaceWorkspaceRef("dev", 9n),
          }}
          storedFocusedPaneId="1"
          onAssignmentsChange={(value) => {
            assignments = value;
          }}
          onUnresolvedAssignmentsChange={(value) => {
            refs = value;
          }}
          onFocusedPaneChange={(value) => {
            focusedPaneId = value;
          }}
          onFocusSession={() => {}}
        />
      </YasWorkspaceProvider>
    ),
    document.body,
  );
  await Promise.resolve();
  vi.advanceTimersByTime(50);

  const check = (waitingForFrame = false) => {
    expect(Object.values(assignments!.assignments).sort()).toEqual([
      "surface:dev:7",
      "surface:dev:9",
    ]);
    expect(
      enumeratePanes(layout().root)
        .map(({ id }) => id)
        .sort(),
    ).toEqual(Object.keys(assignments!.assignments).sort());
    expect(refs).toEqual(assignments!.assignments);
    expect(assignments!.assignments[focusedPaneId!]).toBe("surface:dev:9");
    const canvases = document.querySelectorAll<HTMLCanvasElement>("canvas");
    expect(canvases).toHaveLength(2);
    for (const canvas of canvases) {
      const paneId = canvas
        .closest("[data-yas-pane-id]")!
        .getAttribute("data-yas-pane-id")!;
      if (waitingForFrame && !canvas.dataset.pixels) continue;
      expect(canvas.dataset.pixels, `pixels in pane ${paneId}`).toBe(
        assignments!.assignments[paneId],
      );
    }
  };
  check();
  for (const key of ["ArrowUp", "ArrowDown", "ArrowUp"]) {
    handlePrefixKey(new KeyboardEvent("keydown", { key: "b", ctrlKey: true }));
    handlePrefixKey(new KeyboardEvent("keydown", { key, shiftKey: true }));
    await Promise.resolve();
    check(true);
    presentFrame(7n);
    presentFrame(9n);
    check();
  }
});

it("moves through the tiled tree without geometry and leaves floating frames untouched", async () => {
  const frame = {
    node: { type: "leaf" as const },
    weight: 1,
    rect: { x: 20, y: 10, width: 40, height: 50 },
  };
  const [layout, setLayout] = createSignal<WorkspaceLayout>({
    name: "Mixed movement",
    root: {
      type: "split",
      direction: "workspace",
      children: [
        frame,
        {
          weight: 1,
          node: {
            type: "split",
            direction: "horizontal",
            children: [
              { node: { type: "leaf" }, weight: 1 },
              {
                node: {
                  type: "split",
                  direction: "vertical",
                  children: [
                    { node: { type: "leaf" }, weight: 1 },
                    { node: { type: "leaf" }, weight: 1 },
                  ],
                },
                weight: 1,
              },
            ],
          },
        },
      ],
    },
  });
  let assignments: LayoutAssignments | undefined;
  let refs: Readonly<Record<string, string>> = {};
  let focus: string | null = null;
  let focusPane: (id: string) => void;
  dispose = render(
    () => (
      <YasWorkspaceProvider workspace={workspace as unknown as YasWorkspace}>
        <LayoutContainer
          layout={layout()}
          onLayoutChange={(next) => next && setLayout(next)}
          connectionId="dev"
          palette={PALETTES[0]}
          fontFamily="monospace"
          fontSize={14}
          focusedSessionId={null}
          lruSessionIds={[]}
          liveSurfaceKeys={["dev:7", "dev:9", "dev:11", "dev:13"]}
          storedAssignments={{
            "0": surfaceWorkspaceRef("dev", 7n),
            "1.0": surfaceWorkspaceRef("dev", 9n),
            "1.1.0": surfaceWorkspaceRef("dev", 11n),
            "1.1.1": surfaceWorkspaceRef("dev", 13n),
          }}
          storedFocusedPaneId="1.1.1"
          onAssignmentsChange={(value) => {
            assignments = value;
          }}
          onUnresolvedAssignmentsChange={(value) => {
            refs = value;
          }}
          onFocusedPaneChange={(value) => {
            focus = value;
          }}
          onFocusPane={(fn) => {
            focusPane = fn;
          }}
          onFocusSession={() => {}}
        />
      </YasWorkspaceProvider>
    ),
    document.body,
  );
  await Promise.resolve();
  const moveLeft = () => {
    handlePrefixKey(new KeyboardEvent("keydown", { key: "b", ctrlKey: true }));
    handlePrefixKey(
      new KeyboardEvent("keydown", { key: "ArrowLeft", shiftKey: true }),
    );
  };
  moveLeft();
  expect(assignments!.assignments).toEqual({
    "0": "surface:dev:7",
    "1.0": "surface:dev:9",
    "1.1": "surface:dev:13",
    "1.2": "surface:dev:11",
  });
  expect(refs).toEqual(assignments!.assignments);
  expect(focus).toBe("1.1");
  const root = layout().root;
  if (root.type !== "split") throw new Error("Missing mixed workspace");
  expect(root.direction).toBe("workspace");
  expect(root.children[0]).toBe(frame);
  moveLeft();
  expect(focus).toBe("1.0");
  expect(assignments!.assignments[focus!]).toBe("surface:dev:13");
  expect(refs).toEqual(assignments!.assignments);
  const edge = layout();
  moveLeft();
  expect(layout()).toBe(edge);
  // A floating move with no usable viewport is a no-op, never a tiled edit.
  focusPane!("0");
  moveLeft();
  expect(layout()).toBe(edge);
  expect(assignments!.assignments[focus!]).toBe("surface:dev:7");
});
