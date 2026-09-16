import type { YasSession, YasSurface, YasWasmModule } from "@yas-run/core";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { Workspace } from "../Workspace";
import { disarmPrefix } from "../keyPrefix";

const state = vi.hoisted(() => {
  const surfaces = new Map<bigint, YasSurface>();
  const changes = new Set<() => void>();
  const activations = new Set<(id: bigint) => void>();
  const connection = {
    surfaceStore: {
      getSurfaces: () => surfaces,
      onChange: (fn: () => void) => {
        changes.add(fn);
        return () => changes.delete(fn);
      },
      onActivated: (fn: (id: bigint) => void) => {
        activations.add(fn);
        return () => activations.delete(fn);
      },
    },
    setFontSize: () => {},
    setFontFamily: () => {},
  };
  const snapshot = { connections: [], sessions: [], focusedSessionId: null };
  return {
    surfaces,
    changes,
    activations,
    place: vi.fn(),
    createSession: vi.fn(),
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
      focusSession: vi.fn(),
      search: async () => [],
    },
  };
});

vi.mock("@yas-run/core", async (original) => ({
  ...(await original<typeof import("@yas-run/core")>()),
  measureCell: () => ({ w: 8, h: 16, pw: 8, ph: 16 }),
  YasWorkspace: class {
    constructor() {
      return { ...state.workspace, createSession: state.createSession };
    }
  },
}));

// Exercise the actual workspace, menus, keyboard handlers, and surface event
// subscriptions. Rendering/placement and the server are the test boundaries.
vi.mock("../layout/LayoutContainer", () => ({
  LayoutContainer: (props: {
    onAddManagedWindow: (fn: (assignment: string) => void) => void;
    onFocusedPaneChange: (paneId: string | null) => void;
  }) => {
    props.onAddManagedWindow(state.place);
    props.onFocusedPaneChange(null);
    return null;
  },
  EmptyPane: () => null,
}));
vi.mock("@yas-run/solid", async (original) => ({
  ...(await original<typeof import("@yas-run/solid")>()),
  YasSurfaceView: () => null,
}));
vi.mock("../xdgDesktopCatalogs", () => ({
  xdgDesktopCatalogs: () => [
    {
      connectionId: "dev",
      apps: [],
      catalog: [{ id: "test.desktop", name: "Test application" }],
    },
  ],
  applicationIcon: () => undefined,
  requestApplicationIcons: () => {},
  startApplication: () => true,
}));

let dispose: (() => void) | undefined;
beforeEach(() => {
  vi.useFakeTimers();
  vi.stubGlobal("matchMedia", () => ({
    matches: false,
    addEventListener: () => {},
    removeEventListener: () => {},
  }));
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  vi.spyOn(document, "hasFocus").mockReturnValue(true);
  vi.spyOn(HTMLElement.prototype, "getClientRects").mockReturnValue([
    {} as DOMRect,
  ] as unknown as DOMRectList);
});
afterEach(() => {
  dispose?.();
  dispose = undefined;
  disarmPrefix();
  state.surfaces.clear();
  state.changes.clear();
  state.activations.clear();
  document.body.replaceChildren();
  localStorage.clear();
  vi.restoreAllMocks();
  vi.clearAllMocks();
  state.createSession.mockReset();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

async function mount() {
  dispose = render(
    () => (
      <Workspace
        connections={[{ id: "dev", label: "Development" }]}
        wasm={{} as YasWasmModule}
        onAuthError={() => {}}
      />
    ),
    document.body,
  );
  await Promise.resolve();
}

function key(key: string, ctrlKey = false, shiftKey = false) {
  (document.activeElement ?? document.body).dispatchEvent(
    new KeyboardEvent("keydown", {
      key,
      ctrlKey,
      shiftKey,
      bubbles: true,
      cancelable: true,
    }),
  );
}

async function openMenu() {
  key("b", true);
  key("k");
  await Promise.resolve();
  return document.querySelector<HTMLInputElement>('[role="dialog"] input')!;
}

function arrive(title = "Test application") {
  state.surfaces.set(7n, {
    connectionId: "dev",
    surfaceId: 7n,
    parentId: 0n,
    appId: "test.desktop",
    title,
    width: 800,
    height: 600,
  } as YasSurface);
  for (const changed of state.changes) changed();
  for (const activated of state.activations) activated(7n);
}

function clickRow(label: string) {
  const row = [...document.querySelectorAll<HTMLElement>("section div")].find(
    (el) => el.style.cursor === "pointer" && el.textContent?.includes(label),
  );
  expect(row, `menu row: ${label}`).toBeDefined();
  row!.click();
}

it("keeps a reopened menu and its query when a launched surface arrives", async () => {
  await mount();
  await openMenu();
  clickRow("Test application");
  expect(document.querySelector('[role="dialog"]')).toBeNull();

  const search = await openMenu();
  search.value = "unfinished query";
  search.dispatchEvent(new InputEvent("input", { bubbles: true }));
  arrive();
  await Promise.resolve();
  vi.advanceTimersByTime(50);

  expect(state.place).toHaveBeenCalled();
  expect(search.isConnected).toBe(true);
  expect(search.value).toBe("unfinished query");
  expect(document.activeElement).toBe(search);
  key("Escape");
  expect(document.querySelector('[role="dialog"]')).toBeNull();
});

it.each([false, true])(
  "keeps a reopened menu while terminal creation finishes (beside: %s)",
  async (beside) => {
    const session = {
      id: "dev:1",
      connectionId: "dev",
      ptyId: 1n,
    } as YasSession;
    let finish!: (session: YasSession) => void;
    state.createSession.mockReturnValueOnce(
      new Promise<YasSession>((resolve) => {
        finish = resolve;
      }),
    );
    await mount();
    await openMenu();
    key("b", true);
    key("Enter", false, beside);
    await vi.advanceTimersByTimeAsync(50);
    expect(state.createSession).toHaveBeenCalledOnce();
    expect(document.querySelector('[role="dialog"]')).toBeNull();

    const search = await openMenu();
    finish(session);
    await vi.advanceTimersByTimeAsync(50);
    expect(state.workspace.focusSession).toHaveBeenCalledWith(session.id);
    expect(search.isConnected).toBe(true);
    expect(document.activeElement).toBe(search);
  },
);

it("dismisses the menu when an existing surface is explicitly selected", async () => {
  await mount();
  arrive("Existing window");
  await openMenu();
  clickRow("Existing window");
  expect(document.querySelector('[role="dialog"]')).toBeNull();
});

it("keeps the prefix menu through surface events and temporary window blur", async () => {
  await mount();
  key("b", true);
  const menu = document.querySelector('[aria-label="Keys behind Ctrl+B"]');
  expect(menu).not.toBeNull();
  arrive();
  window.dispatchEvent(new Event("blur"));
  window.dispatchEvent(new Event("focus"));
  vi.advanceTimersByTime(50);
  expect(menu!.isConnected).toBe(true);
  key("Escape");
  expect(
    document.querySelector('[aria-label="Keys behind Ctrl+B"]'),
  ).toBeNull();
});
