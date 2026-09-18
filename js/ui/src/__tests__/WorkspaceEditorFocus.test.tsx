import {
  PALETTES,
  type YasSession,
  type YasWasmModule,
  type YasWorkspace,
} from "@yas-run/core";
import { YasWorkspaceProvider } from "@yas-run/solid";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { defaultKeymap } from "@codemirror/commands";
import { openSearchPanel, search } from "@codemirror/search";
import { createEffect, createSignal, onCleanup } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { Workspace } from "../Workspace";
import { disarmPrefix } from "../keyPrefix";
import {
  saveActiveLayoutState,
  tabWorkspaceRef,
  terminalWorkspaceRef,
} from "../layout/store";
import { tabId } from "../ide/tabRegistry";
import { LayoutContainer } from "../layout/LayoutContainer";

const state = vi.hoisted(() => {
  const listeners = new Set<() => void>();
  const surfaceListeners = new Set<() => void>();
  const session = {
    id: "dev:1",
    connectionId: "dev",
    ptyId: 1n,
    state: "active",
    title: "Terminal",
  } as YasSession;
  let snapshot = {
    connections: [
      {
        id: "dev",
        status: "connected",
        ready: true,
        supportsKv: true,
        generation: 1,
      },
    ],
    sessions: [session],
    focusedSessionId: session.id,
  };
  const connection = {
    surfaceStore: {
      getSurfaces: () => new Map(),
      onChange: (fn: () => void) => {
        surfaceListeners.add(fn);
        return () => surfaceListeners.delete(fn);
      },
      onActivated: () => () => {},
      setPresentationSmoothingEnabled: () => {},
    },
    setFontSize: () => {},
    setFontFamily: () => {},
    setSurfaceMaxFpsCap: () => {},
  };
  return {
    listeners,
    surfaceListeners,
    publish: () => {
      snapshot = {
        ...snapshot,
        connections: snapshot.connections.map((connection) => ({
          ...connection,
        })),
        sessions: snapshot.sessions.map((session) => ({ ...session })),
      };
      for (const listener of listeners) listener();
    },
    workspace: {
      activities: { getSnapshot: () => [], subscribe: () => () => {} },
      getSnapshot: () => snapshot,
      getConnection: () => connection,
      addConnection: () => {},
      removeConnection: () => {},
      subscribe: (fn: () => void) => {
        listeners.add(fn);
        return () => listeners.delete(fn);
      },
      dispose: () => {},
      setVisibleSessions: () => {},
      setSurfaceDiagnosticsEnabled: () => {},
      getConnectionDebugStats: () => null,
      focusSession: vi.fn(),
      search: async () => [],
      sessionCwd: async () => null,
      kvFetch: async (_connectionId: string, key: string) =>
        key.startsWith("tabs/")
          ? { value: new TextEncoder().encode("editor:/src/example.ts") }
          : null,
      kvPut: async () => {},
      watchKv: async () => ({ mirror: { live: new Map() }, close: () => {} }),
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

// Keep the workspace, pane tree, and focus management real. Only the server
// and rendered terminal/editor internals sit outside this regression.
vi.mock("@yas-run/solid", async (original) => ({
  ...(await original<typeof import("@yas-run/solid")>()),
  YasTerminal: (props: {
    sessionId: string;
    focus?: boolean;
    readOnly?: boolean;
  }) => {
    const input = document.createElement("textarea");
    input.dataset.terminal = props.sessionId;
    createEffect(() => {
      if (props.focus && !props.readOnly) input.focus();
    });
    return input;
  },
  YasSurfaceView: () => null,
}));

vi.mock("../ide/YasTile", () => ({
  YasTile: () => {
    const host = document.createElement("div");
    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: "const answer = 42;",
        extensions: [keymap.of(defaultKeymap), search()],
      }),
    });
    view.contentDOM.dataset.editor = "";
    openSearchPanel(view);
    host.querySelector<HTMLInputElement>(
      'input[name="search"]',
    )!.dataset.editorSearch = "";
    onCleanup(() => view.destroy());
    return host;
  },
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
  state.listeners.clear();
  state.surfaceListeners.clear();
  document.body.replaceChildren();
  localStorage.clear();
  vi.restoreAllMocks();
  vi.clearAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

function key(key: string, ctrlKey = false) {
  document.activeElement!.dispatchEvent(
    new KeyboardEvent("keydown", {
      key,
      ctrlKey,
      bubbles: true,
      cancelable: true,
    }),
  );
}

it.each(["[data-editor]", "[data-editor-search]"])(
  "preserves %s focus through prefix arm/cancel and workspace updates",
  async (selector) => {
    saveActiveLayoutState(
      {
        name: "Editor and terminal",
        root: {
          type: "split",
          direction: "horizontal",
          children: [
            { node: { type: "leaf" }, weight: 1 },
            { node: { type: "leaf" }, weight: 1 },
          ],
        },
      },
      {
        "0": tabWorkspaceRef("dev", tabId("editor:/src/example.ts")),
        "1": terminalWorkspaceRef("dev", 1n),
      },
      "0",
    );
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
    await vi.advanceTimersByTimeAsync(50);
    const editor = document.querySelector<HTMLElement>(selector)!;
    expect(editor).not.toBeNull();
    expect(document.querySelector('[data-terminal="dev:1"]')).not.toBeNull();
    const view = EditorView.findFromDOM(editor.closest(".cm-editor")!)!;
    view.dispatch({ selection: { anchor: 5 } });
    if (editor instanceof HTMLInputElement) {
      editor.value = "unfinished query";
      editor.setSelectionRange(3, 7);
    }
    editor.focus();
    await vi.advanceTimersByTimeAsync(50);
    expect(document.activeElement).toBe(editor);

    const check = () => {
      expect(document.querySelector(selector)).toBe(editor);
      expect(document.activeElement).toBe(editor);
      expect(view.state.selection.main.anchor).toBe(5);
      if (editor instanceof HTMLInputElement) {
        expect(editor.value).toBe("unfinished query");
        expect([editor.selectionStart, editor.selectionEnd]).toEqual([3, 7]);
      }
      expect(
        editor
          .closest("[data-yas-pane-focused]")
          ?.getAttribute("data-yas-pane-id"),
      ).toBe("0");
    };
    key("b", true);
    expect(
      document.querySelector('[aria-label="Keys behind Ctrl+B"]'),
    ).not.toBeNull();
    check();
    state.publish();
    await vi.advanceTimersByTimeAsync(50);
    check();
    key("Escape");
    expect(
      document.querySelector('[aria-label="Keys behind Ctrl+B"]'),
    ).toBeNull();
    check();
    state.publish();
    await vi.advanceTimersByTimeAsync(50);
    check();

    // Selecting a different pane remains an explicit focus handoff.
    key("b", true);
    key("2");
    await vi.advanceTimersByTimeAsync(50);
    expect(document.activeElement).toBe(
      document.querySelector('[data-yas-pane-id="1"] [data-terminal="dev:1"]'),
    );
    key("b", true);
    key("1");
    await vi.advanceTimersByTimeAsync(50);
    expect(document.activeElement).toBe(view.contentDOM);
    expect(view.state.selection.main.anchor).toBe(5);
  },
);

it("does not autofocus an editor while its layout is hidden", async () => {
  const [visible, setVisible] = createSignal(true);
  dispose = render(
    () => (
      <YasWorkspaceProvider
        workspace={state.workspace as unknown as YasWorkspace}
      >
        <LayoutContainer
          layout={{ name: "Editor", root: { type: "leaf" } }}
          onLayoutChange={() => {}}
          connectionId="dev"
          palette={PALETTES[0]}
          fontFamily="monospace"
          fontSize={14}
          focusedSessionId={null}
          lruSessionIds={[]}
          manageVisibility={visible()}
          storedAssignments={{
            "0": tabWorkspaceRef("dev", tabId("editor:/src/example.ts")),
          }}
          storedFocusedPaneId="0"
          onFocusSession={() => {}}
        />
      </YasWorkspaceProvider>
    ),
    document.body,
  );
  await vi.advanceTimersByTimeAsync(50);
  const editor = document.querySelector<HTMLElement>("[data-editor]")!;
  editor.focus();
  const focus = vi.spyOn(editor, "focus");
  setVisible(false);
  await vi.advanceTimersByTimeAsync(50);
  expect(document.activeElement).toBe(editor);
  expect(focus).not.toHaveBeenCalled();

  // Hiding also must not reclaim focus that became unowned just beforehand.
  setVisible(true);
  editor.blur();
  setVisible(false);
  state.publish();
  await vi.advanceTimersByTimeAsync(50);
  expect(document.activeElement).toBe(document.body);
  expect(focus).not.toHaveBeenCalled();
  expect(document.querySelector("[data-editor]")).toBe(editor);
});
