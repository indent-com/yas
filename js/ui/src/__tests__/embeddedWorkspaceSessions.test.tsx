import { afterEach, expect, it, vi } from "vitest";
import type { YasTransport, YasWasmModule } from "@yas-run/core";
import { mountYasWorkspace } from "../embed";
import { shellCapabilities, setShellCapabilities } from "../shellCapabilities";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.replaceChildren();
  setShellCapabilities({ remotes: true, previews: true });
});

it("opens the regular workspace manager over an embedded home transport", () => {
  // Hold the transport before its handshake: session controls must be usable
  // even while loading, without creating an unbound workspace or a renderer.
  const listeners = new Map<string, Set<Function>>();
  const transport: YasTransport = {
    status: "connecting",
    authRejected: false,
    lastError: null,
    connect: vi.fn(),
    send: vi.fn(),
    close: vi.fn(),
    addEventListener(type, listener) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type)!.add(listener);
    },
    removeEventListener(type, listener) {
      listeners.get(type)?.delete(listener);
    },
  };
  const onAuthError = vi.fn();
  dispose = mountYasWorkspace(document.body, {
    wasm: {} as YasWasmModule,
    home: {
      transport,
      workspaceSessionDeviceId: "123e4567-e89b-42d3-a456-426614174000",
    },
    onAuthError,
  });

  const manager = document.querySelector<HTMLButtonElement>(
    'button[aria-label="Open workspace manager"]',
  );
  expect(manager).not.toBeNull();
  manager!.click();
  expect(document.querySelector('[role="dialog"]')).not.toBeNull();
  expect(shellCapabilities()).toEqual({ remotes: true, previews: false });

  Object.assign(transport, { authRejected: true });
  for (const listener of listeners.get("statuschange") ?? []) listener("error");
  expect(onAuthError).toHaveBeenCalledOnce();

  dispose();
  dispose = undefined;
  expect(transport.close).toHaveBeenCalled();
  expect([...listeners.values()].every((set) => set.size === 0)).toBe(true);
});
