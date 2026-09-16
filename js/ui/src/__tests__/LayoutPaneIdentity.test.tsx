import { PALETTES } from "@yas-run/core";
import type { WorkspaceLayout } from "@yas-run/core/layout";
import { createEffect, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { LayoutContainer } from "../layout/LayoutContainer";
import {
  surfaceAssignment,
  surfaceWorkspaceRef,
  terminalWorkspaceRef,
} from "../layout/store";

const bindings = vi.hoisted(() => new Map<HTMLCanvasElement, Set<string>>());

vi.mock("@yas-run/solid", () => {
  const canvasFor = (resource: () => string) => {
    const canvas = document.createElement("canvas");
    const seen = new Set<string>();
    bindings.set(canvas, seen);
    createEffect(() => {
      const id = resource();
      seen.add(id);
      canvas.dataset.resource = id;
    });
    return canvas;
  };
  const snapshot = {
    sessions: [1n, 2n, 3n].map((ptyId) => ({
      id: `terminal:${ptyId}`,
      connectionId: "dev",
      ptyId,
      state: "active",
    })),
    connections: [{ id: "dev", status: "connected", ready: true }],
    focusedSessionId: null,
  };
  return {
    createYasWorkspace: () => ({
      getConnection: () => null,
      setVisibleSessions: () => {},
    }),
    createYasWorkspaceState: () => () => snapshot,
    createYasSessions: () => () => snapshot.sessions,
    YasTerminal: (props: { sessionId: string }) =>
      canvasFor(() => props.sessionId),
    YasSurfaceView: (props: { surfaceId: bigint }) =>
      canvasFor(() => `surface:${props.surfaceId}`),
  };
});

let dispose: (() => void) | undefined;
beforeEach(() => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
});
afterEach(() => {
  dispose?.();
  dispose = undefined;
  bindings.clear();
  vi.unstubAllGlobals();
  localStorage.clear();
  document.body.replaceChildren();
});

it.each([
  { direction: "horizontal", nested: false },
  { direction: "vertical", nested: false },
  { direction: "horizontal", nested: true },
  { direction: "vertical", nested: true },
] as const)(
  "keeps terminal and surface canvases through insertion and removal ($direction, nested: $nested)",
  async ({ direction, nested }) => {
    const prefix = nested ? "0." : "";
    const children = Array.from({ length: 4 }, () => ({
      node: { type: "leaf" as const },
      weight: 1,
    }));
    const [layout, setLayout] = createSignal<WorkspaceLayout>({
      name: "Pane continuity",
      root: nested
        ? {
            type: "split",
            direction: direction === "horizontal" ? "vertical" : "horizontal",
            children: [
              { node: { type: "split", direction, children }, weight: 1 },
              { node: { type: "leaf" }, weight: 1 },
            ],
          }
        : { type: "split", direction, children },
    });
    let remove!: (paneId: string) => void;
    let open!: (assignment: string, paneId: string) => boolean;
    dispose = render(
      () => (
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
            [`${prefix}0`]: terminalWorkspaceRef("dev", 1n),
            [`${prefix}1`]: surfaceWorkspaceRef("dev", 7n),
            [`${prefix}2`]: terminalWorkspaceRef("dev", 2n),
            [`${prefix}3`]: surfaceWorkspaceRef("dev", 9n),
            ...(nested ? { "1": surfaceWorkspaceRef("dev", 13n) } : {}),
          }}
          onFocusSession={() => {}}
          onClearPaneAssignment={(fn) => {
            remove = fn;
          }}
          onOpenInContainer={(fn) => {
            open = fn;
          }}
        />
      ),
      document.body,
    );
    await Promise.resolve();
    const originals = [...document.querySelectorAll("canvas")];
    expect(originals).toHaveLength(nested ? 5 : 4);
    const resources = originals.map((canvas) =>
      canvas.getAttribute("data-resource"),
    );
    const check = () => {
      for (let index = 0; index < originals.length; index++) {
        expect(
          document.querySelector(`canvas[data-resource="${resources[index]}"]`),
        ).toBe(originals[index]);
        expect([...bindings.get(originals[index])!]).toEqual([
          resources[index],
        ]);
      }
    };
    for (const added of ["terminal:3", surfaceAssignment("dev", 11n)]) {
      expect(open(added, `${prefix}0`)).toBe(true);
      await Promise.resolve();
      check();
    }
    // Resize edits replace the split wrappers while retaining their leaves.
    const reweight = (
      node: WorkspaceLayout["root"],
    ): WorkspaceLayout["root"] =>
      node.type === "leaf"
        ? node
        : {
            ...node,
            children: node.children.map((child, index) => ({
              ...child,
              weight: index + 2,
              node: reweight(child.node),
            })),
          };
    setLayout({ ...layout(), root: reweight(layout().root) });
    await Promise.resolve();
    check();
    for (let count = 0; count < 2; count++) {
      remove(`${prefix}1`);
      await Promise.resolve();
      check();
    }
  },
);
