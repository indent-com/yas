import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import {
  createDefaultStoredWorkspaceSession,
  type YasWasmModule,
  type WorkspaceSessionPatch,
} from "@yas-run/core";
import type { WorkspaceLayout } from "@yas-run/core/layout";
import { Workspace } from "../Workspace";
import { togglePaneFloating } from "../layout/floatingWindow";
import { manageAssignment, tabWorkspaceRef } from "../layout/store";
import { tabId } from "../ide/tabRegistry";
import type { WorkspaceSessionBinding } from "../workspaceSession";

const state = vi.hoisted(() => {
  const snapshot = { connections: [], sessions: [], focusedSessionId: null };
  const connection = {
    surfaceStore: {
      getSurfaces: () => new Map(),
      onChange: () => () => {},
      onActivated: () => () => {},
    },
    setFontSize: () => {},
    setFontFamily: () => {},
  };
  return {
    layoutProps: null as {
      layout: WorkspaceLayout;
      onLayoutChange: (layout: WorkspaceLayout) => void;
    } | null,
    workspace: {
      activities: { getSnapshot: () => [], subscribe: () => () => {} },
      getSnapshot: () => snapshot,
      getConnection: () => connection,
      addConnection: () => {},
      subscribe: () => () => {},
      dispose: () => {},
      setVisibleSessions: () => {},
      setSurfaceDiagnosticsEnabled: () => {},
      getConnectionDebugStats: () => null,
      focusSession: () => {},
      search: async () => [],
    },
  };
});
vi.mock("@yas-run/core", async (original) => ({
  ...(await original<typeof import("@yas-run/core")>()),
  measureCell: () => ({ w: 8, h: 16, pw: 8, ph: 16 }),
  YasWorkspace: class {
    constructor() {
      return state.workspace;
    }
  },
}));
// Exercise Workspace's actual hydration and persistence effects. Rendering
// and server transport are the boundaries, rather than the persistence queue.
vi.mock("../layout/LayoutContainer", () => ({
  LayoutContainer: (
    props: NonNullable<typeof state.layoutProps> & {
      onAssignmentsResolved: (resolved: boolean) => void;
    },
  ) => {
    state.layoutProps = props;
    props.onAssignmentsResolved(true);
    return null;
  },
  EmptyPane: () => null,
}));

let dispose: (() => void) | undefined;
beforeEach(() => {
  vi.useFakeTimers();
  vi.stubGlobal("matchMedia", () => ({
    matches: false,
    addEventListener() {},
    removeEventListener() {},
  }));
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
  document.body.replaceChildren();
  localStorage.clear();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

it.each([
  { delay: 0, flush: "timer" },
  { delay: 300, flush: "timer" },
  { delay: 0, flush: "pagehide" },
  { delay: 300, flush: "hidden" },
])(
  "remembers tiling Manage after refresh (edit after $delay ms, flush: $flush)",
  async ({ delay, flush }) => {
    let record = createDefaultStoredWorkspaceSession({
      id: "00000000-0000-4000-8000-000000000001",
    });
    record = {
      ...record,
      workspace: {
        ...record.workspace,
        layout: {
          name: "Manage",
          root: {
            type: "split",
            direction: "workspace",
            children: [
              {
                node: { type: "leaf" },
                weight: 1,
                rect: { x: 10, y: 10, width: 60, height: 60 },
              },
            ],
          },
        },
        assignments: { "0": tabWorkspaceRef("dev", tabId("manage:")) },
      },
    };
    const mount = () => {
      const [restoring, setRestoring] = createSignal(true);
      const binding: WorkspaceSessionBinding = {
        id: record.id,
        current: () => record,
        restoring,
        finishRestoring: () => setRestoring(false),
        patch: vi.fn(async (patch: WorkspaceSessionPatch) => {
          record = {
            ...record,
            workspace: {
              ...record.workspace,
              ...patch.workspace,
              panels: {
                ...record.workspace.panels,
                ...patch.workspace?.panels,
                expandedSections: [
                  ...(patch.workspace?.panels?.expandedSections ??
                    record.workspace.panels.expandedSections),
                ],
              },
            },
          };
        }),
        setRemoteActive: async () => {},
      };
      return render(
        () => (
          <Workspace
            connections={[{ id: "dev", label: "Development" }]}
            wasm={{} as YasWasmModule}
            workspaceSession={binding}
            onAuthError={() => {}}
          />
        ),
        document.body,
      );
    };
    dispose = mount();
    await vi.advanceTimersByTimeAsync(delay);
    const props = state.layoutProps!;
    const mutation = togglePaneFloating(
      props.layout.root,
      { "0": manageAssignment("dev") },
      "0",
      { x: 10, y: 10, width: 60, height: 60 },
    )!;
    expect(mutation.root).toEqual({ type: "leaf" });
    props.onLayoutChange({ ...props.layout, root: mutation.root });
    if (flush === "pagehide") window.dispatchEvent(new Event("pagehide"));
    else if (flush === "hidden") {
      vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
      document.dispatchEvent(new Event("visibilitychange"));
    } else await vi.advanceTimersByTimeAsync(300);
    await Promise.resolve();
    expect(record.workspace.layout?.root).toEqual({ type: "leaf" });
    dispose();
    // Reload from the server document, not this page's live layout objects.
    record = JSON.parse(JSON.stringify(record));
    dispose = mount();
    expect(state.layoutProps!.layout.root).toEqual({ type: "leaf" });
  },
);
