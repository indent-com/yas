import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  YAS_EXTENSION_PHASE_QUEUED,
  YAS_EXTENSION_PHASE_RUNNING,
  YAS_EXTENSION_PHASE_STOPPED,
  YAS_EXTENSION_PHASE_STOPPING,
  YAS_EXTENSION_CONTROL_DISABLE,
  YAS_EXTENSION_CONTROL_REMOVE,
  type YasExtensionRecord,
  type YasWorkspace,
} from "@yas-run/core";
import { PALETTES } from "@yas-run/core/palettes";
import { ExtensionsPanel } from "../ExtensionsPanel";
import type { ExtensionHost } from "../extensionRegistry";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

const record = (
  name: string,
  handle: bigint,
  phase: number = YAS_EXTENSION_PHASE_RUNNING,
) =>
  ({
    name,
    extensionHandle: handle,
    generation: 1n,
    definitionRevision: 1n,
    contentHash: new Uint8Array(32).fill(1),
    flags: 3,
    phase,
  }) as YasExtensionRecord;

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.replaceChildren();
  vi.unstubAllGlobals();
});

async function mount(initial: YasExtensionRecord[] = []) {
  let records = initial;
  let listener:
    | ((records: readonly YasExtensionRecord[] | null) => void)
    | undefined;
  const unsubscribe = vi.fn(() => {
    listener = undefined;
  });
  const host = {
    native: {},
    listExtensions: vi.fn(async () => records),
    subscribeExtensions: vi.fn((next: typeof listener) => {
      listener = next;
      return unsubscribe;
    }),
    installExtension: vi.fn<ExtensionHost["installExtension"]>(),
    controlExtension: vi.fn<ExtensionHost["controlExtension"]>(),
  };
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => ({
      ok: true,
      json: async () => ({
        extensions: ["alpha", "beta", "gamma"].map((name) => ({
          name,
          blake3: "01".repeat(32),
        })),
      }),
    })),
  );
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(
    () => (
      <ExtensionsPanel
        workspace={{ getConnection: () => host } as unknown as YasWorkspace}
        connectionId="test"
        palette={PALETTES[0]!}
        fontSize={13}
      />
    ),
    root,
  );
  const row = (name: string) =>
    root.querySelector<HTMLElement>(`[data-extension="${name}"]`)!;
  const button = (name: string, label: string) =>
    Array.from(row(name).querySelectorAll("button")).find(
      (button) => button.textContent === label,
    )!;
  await vi.waitFor(() => expect(row("gamma")).not.toBeNull());
  return {
    host,
    root,
    row,
    button,
    unsubscribe,
    publish(next: YasExtensionRecord[]) {
      records = next;
      listener?.(next);
    },
  };
}

describe("independent extension operations", () => {
  it("keeps removed rows installable when another removal advances the catalogue", async () => {
    let records = [record("alpha", 1n), record("beta", 2n)];
    const { host, row, button, publish } = await mount(records);
    host.controlExtension.mockImplementation(async (handle, action) => {
      if (action === YAS_EXTENSION_CONTROL_DISABLE) {
        // The server stops and then disables the definition. Both lifecycle
        // changes advance its generation without changing its handle.
        records = records.map((item) =>
          item.extensionHandle === handle
            ? {
                ...item,
                generation: item.generation + 2n,
                phase: YAS_EXTENSION_PHASE_STOPPED,
                flags: 1,
              }
            : item,
        );
      } else if (action === YAS_EXTENSION_CONTROL_REMOVE) {
        records = records.filter((item) => item.extensionHandle !== handle);
      }
      publish(records);
      return records.find((item) => item.extensionHandle === handle) ?? null;
    });

    for (const name of ["alpha", "beta"]) {
      button(name, "Remove").click();
      await vi.waitFor(() =>
        expect(row(name).textContent).toContain(`Removed ${name}`),
      );
      expect(button("alpha", "Install")).toBeDefined();
      expect(button("alpha", "Remove")).toBeUndefined();
      expect(button("gamma", "Install")).toBeDefined();
    }
    expect(button("beta", "Install")).toBeDefined();
    expect(records).toEqual([]);
    expect(host.installExtension).not.toHaveBeenCalled();
    expect(host.controlExtension.mock.calls).toEqual([
      [1n, YAS_EXTENSION_CONTROL_DISABLE],
      [1n, YAS_EXTENSION_CONTROL_REMOVE],
      [2n, YAS_EXTENSION_CONTROL_DISABLE],
      [2n, YAS_EXTENSION_CONTROL_REMOVE],
    ]);
  });

  it("does not let a slow reload undo a completed installation", async () => {
    const { host, root, row, button } = await mount();
    const install = deferred<YasExtensionRecord>();
    host.installExtension.mockReturnValue(install.promise);
    button("alpha", "Install").click();
    await vi.waitFor(() =>
      expect(host.installExtension).toHaveBeenCalledOnce(),
    );

    const reload = deferred<YasExtensionRecord[]>();
    host.listExtensions.mockReturnValueOnce(reload.promise);
    Array.from(root.querySelectorAll("button"))
      .find((button) => button.textContent === "Reload")!
      .click();
    install.resolve(record("alpha", 1n));
    await vi.waitFor(() =>
      expect(row("alpha").textContent).toContain("Installed alpha"),
    );
    reload.resolve([]);
    await reload.promise;
    expect(button("alpha", "Restart").disabled).toBe(false);
    expect(button("beta", "Install").disabled).toBe(false);
  });

  it("accepts rapid installs, retains per-row progress and errors, and allows retry", async () => {
    const { host, root, row, button, publish, unsubscribe } = await mount();
    const alpha = deferred<YasExtensionRecord>();
    const beta = deferred<YasExtensionRecord>();
    const gamma = deferred<YasExtensionRecord>();
    const pending = { alpha, beta, gamma };
    host.installExtension.mockImplementation((request) => {
      request.onProgress?.("downloading");
      return pending[request.name as keyof typeof pending].promise;
    });
    for (const name of ["alpha", "beta", "gamma"]) {
      expect(button(name, "Install").disabled).toBe(false);
      button(name, "Install").click();
      button(name, "Install").click();
    }
    await vi.waitFor(() =>
      expect(host.installExtension).toHaveBeenCalledTimes(3),
    );
    expect(row("alpha").textContent).toContain("Downloading module");
    expect(button("alpha", "Install").disabled).toBe(true);
    expect(
      Array.from(root.querySelectorAll("button")).find(
        (b) => b.textContent === "Reload",
      )!.disabled,
    ).toBe(false);

    // The server can publish a definition before its install has finished.
    const gammaButton = button("gamma", "Install");
    publish([record("alpha", 1n, YAS_EXTENSION_PHASE_QUEUED)]);
    expect(button("gamma", "Install")).toBe(gammaButton);
    expect(button("alpha", "Restart").disabled).toBe(true);
    expect(row("alpha").textContent).toContain("Downloading module");
    publish([
      record("alpha", 1n, YAS_EXTENSION_PHASE_QUEUED),
      record("beta", 2n),
    ]);
    beta.resolve(record("beta", 2n));
    gamma.reject(new Error("registry unavailable"));
    await vi.waitFor(() => {
      expect(row("beta").textContent).toContain("Installed beta");
      expect(row("gamma").textContent).toContain("registry unavailable");
    });
    expect(row("alpha").getAttribute("data-busy")).toBe("true");
    expect(button("gamma", "Install").disabled).toBe(false);
    expect(
      Array.from(root.querySelectorAll("[data-extension]")).map((r) =>
        r.getAttribute("data-extension"),
      ),
    ).toEqual(["alpha", "beta", "gamma"]);

    host.installExtension.mockResolvedValueOnce(record("gamma", 3n));
    button("gamma", "Install").click();
    alpha.resolve(record("alpha", 1n));
    await vi.waitFor(() => {
      expect(row("alpha").textContent).toContain("Installed alpha");
      expect(row("gamma").textContent).toContain("Installed gamma");
    });
    expect(row("gamma").textContent).not.toContain("registry unavailable");
    publish([
      record("alpha", 1n, YAS_EXTENSION_PHASE_STOPPED),
      record("beta", 2n),
      record("gamma", 3n),
    ]);
    expect(button("alpha", "Start").disabled).toBe(false);
    dispose?.();
    dispose = undefined;
    expect(unsubscribe).toHaveBeenCalledOnce();
  });

  it("keeps controls and installs responsive while another extension stops for removal", async () => {
    const { host, row, button, publish } = await mount([
      record("alpha", 1n),
      record("beta", 2n),
    ]);
    host.controlExtension.mockImplementation(async (handle, action) => {
      if (action === YAS_EXTENSION_CONTROL_DISABLE) {
        publish([
          record("alpha", 1n, YAS_EXTENSION_PHASE_STOPPING),
          record("beta", 2n),
        ]);
      }
      return action === YAS_EXTENSION_CONTROL_REMOVE
        ? null
        : record(handle === 1n ? "alpha" : "beta", handle);
    });
    button("alpha", "Remove").click();
    await vi.waitFor(() =>
      expect(row("alpha").textContent).toContain(
        "Waiting for extension to stop",
      ),
    );
    expect(button("alpha", "Remove").disabled).toBe(true);
    expect(button("beta", "Restart").disabled).toBe(false);
    expect(button("gamma", "Install").disabled).toBe(false);
    button("beta", "Restart").click();
    await vi.waitFor(() =>
      expect(row("beta").textContent).toContain("Restarted beta"),
    );
    publish([
      record("alpha", 1n, YAS_EXTENSION_PHASE_STOPPED),
      record("beta", 2n),
    ]);
    await vi.waitFor(() =>
      expect(row("alpha").textContent).toContain("Removed alpha"),
    );
    expect(button("alpha", "Install").disabled).toBe(false);
  });
});
