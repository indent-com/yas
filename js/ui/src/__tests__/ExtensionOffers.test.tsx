import { render } from "solid-js/web";
import { createSignal } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";
import { PALETTES } from "@yas-run/core/palettes";
import {
  YAS_EXTENSION_PHASE_RUNNING,
  type YasExtensionRecord,
  type YasWorkspace,
  type YasServerHello,
  type YasConnectionSnapshot,
} from "@yas-run/core";
import { ExtensionsPanel } from "../ExtensionsPanel";
import { ExtensionOffers } from "../ExtensionOffers";
import { checkExtensionViability, type Viability } from "../extensionViability";

vi.mock("../extensionViability", async (original) => ({
  ...(await original<typeof import("../extensionViability")>()),
  checkExtensionViability: vi.fn(),
}));

const disposers: (() => void)[] = [];
afterEach(() => {
  disposers.splice(0).forEach((dispose) => dispose());
  document.body.replaceChildren();
  localStorage.clear();
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

const checks = () =>
  new Map<string, Viability>([
    ["ready", { status: "available", reasons: [] }],
    ["limited", { status: "limited", reasons: ["Polling only"] }],
    ["blocked", { status: "unavailable", reasons: ["No runtime"] }],
    ["unknown", { status: "unknown", reasons: ["Probe timed out"] }],
    ["existing", { status: "available", reasons: [] }],
  ]);

function fixture() {
  const record = (name: string, handle: bigint) =>
    ({
      name,
      extensionHandle: handle,
      generation: 1n,
      definitionRevision: 1n,
      flags: 3,
      phase: YAS_EXTENSION_PHASE_RUNNING,
      contentHash: new Uint8Array(32).fill(1),
    }) as YasExtensionRecord;
  let records = [{ ...record("existing", 1n), flags: 1 }];
  const listeners = new Set<
    (records: readonly YasExtensionRecord[] | null) => void
  >();
  const hello = {
    serverName: "test",
    families: [],
    extensions: [],
    bootId: new Uint8Array(16),
    sessionId: new Uint8Array(16),
  } as unknown as YasServerHello;
  const host = {
    native: {
      connection: {
        hello,
        onReady: () => () => {},
        onCatalogChange: () => () => {},
      },
    },
    listExtensions: vi.fn(async () => records),
    subscribeExtensions: (
      listener: (records: readonly YasExtensionRecord[] | null) => void,
    ) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    installExtension: vi.fn(async ({ name }: { name: string }) => {
      const previous = records.find((item) => item.name === name);
      const installed = {
        ...record(
          name,
          previous?.extensionHandle ?? BigInt(records.length + 1),
        ),
        definitionRevision: (previous?.definitionRevision ?? 0n) + 1n,
      };
      records = [...records.filter((item) => item.name !== name), installed];
      listeners.forEach((listener) => listener(records));
      return installed;
    }),
    controlExtension: vi.fn(),
  };
  const manifest = {
    extensions: [...checks().keys()].map((name) => ({
      name,
      blake3: "01".repeat(32),
      bytes: 100,
      requirements: {
        version: 1,
        runtime: "wasmi",
        commandProvider: true,
        families: [],
        probes: [],
        activation: "Starts existing configured units.",
      },
    })),
  };
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => ({ ok: true, json: async () => manifest })),
  );
  const workspace = { getConnection: () => host } as unknown as YasWorkspace;
  function mount(offer = true) {
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(
      () => (
        <ExtensionsPanel
          workspace={workspace}
          connectionId="test"
          palette={PALETTES[0]!}
          fontSize={13}
          offer={
            offer ? { label: "Test server", target: "/test:local" } : undefined
          }
        />
      ),
      root,
    );
    disposers.push(dispose);
    const button = (text: string) =>
      Array.from(root.querySelectorAll("button")).find(
        (button) => button.textContent === text,
      )!;
    return { root, button, dispose };
  }
  return {
    host,
    workspace,
    mount,
    manifest,
    makeOutdated() {
      records = [
        { ...record("existing", 1n), contentHash: new Uint8Array(32).fill(2) },
      ];
    },
  };
}

describe("connection-time extension offers", () => {
  it("starts probes only after the workspace connection is ready", async () => {
    vi.mocked(checkExtensionViability).mockImplementation(async () => checks());
    const { workspace } = fixture();
    const [ready, setReady] = createSignal(false);
    disposers.push(
      render(
        () => (
          <ExtensionOffers
            workspace={workspace}
            connections={[
              {
                id: "test",
                status: "connected",
                ready: ready(),
                supportsExtensions: true,
              } as YasConnectionSnapshot,
            ]}
            readOnly={() => false}
            label={() => "Test server"}
            palette={PALETTES[0]!}
            fontSize={13}
          />
        ),
        document.body,
      ),
    );
    await Promise.resolve();
    expect(checkExtensionViability).not.toHaveBeenCalled();
    setReady(true);
    await vi.waitFor(() =>
      expect(checkExtensionViability).toHaveBeenCalledOnce(),
    );
  });

  it("offers and applies updates alongside missing extensions without duplicating definitions", async () => {
    vi.mocked(checkExtensionViability).mockImplementation(async () => checks());
    const { host, mount, makeOutdated } = fixture();
    makeOutdated();
    const { root, button } = mount();
    await vi.waitFor(() => expect(button("Review")).toBeDefined());
    expect(root.textContent).toContain("1 update");
    button("Review").click();
    const update = root.querySelector('[data-extension="existing"]')!;
    expect(update.textContent).toContain("Update available");
    expect(
      update.querySelector<HTMLInputElement>('input[type="checkbox"]')!.checked,
    ).toBe(true);
    expect(update.querySelector("[data-extension-update]")).not.toBeNull();
    button("Install / update selected").click();
    await vi.waitFor(() =>
      expect(host.installExtension).toHaveBeenCalledTimes(3),
    );
    expect(host.installExtension).toHaveBeenCalledWith(
      expect.objectContaining({
        name: "existing",
        expectedExtensionHandle: 1n,
        expectedGeneration: 1n,
        expectedDefinitionRevision: 1n,
      }),
    );
    await vi.waitFor(() =>
      expect(
        update.isConnected ? update.textContent : root.textContent,
      ).toContain("Updated existing"),
    );
    expect(root.querySelectorAll('[data-extension="existing"]')).toHaveLength(
      1,
    );
    expect(root.querySelector("[data-extension-update]")).toBeNull();
  });

  it("selects newly eligible rows on recheck while preserving explicit deselections", async () => {
    const first = checks();
    first.set("limited", {
      status: "unknown",
      reasons: ["Waiting for server"],
    });
    vi.mocked(checkExtensionViability).mockResolvedValueOnce(first);
    const { root, button } = fixture().mount();
    await vi.waitFor(() => expect(button("Review")).toBeDefined());
    button("Review").click();
    const selectedNames = () =>
      Array.from(
        root.querySelectorAll<HTMLInputElement>(
          "[data-extension] input:checked",
        ),
      ).map((input) => input.getAttribute("aria-label"));
    expect(selectedNames()).toEqual(["Select ready"]);
    const ready = root.querySelector<HTMLInputElement>(
      '[data-extension="ready"] input',
    )!;
    ready.checked = false;
    ready.dispatchEvent(new Event("change", { bubbles: true }));
    vi.mocked(checkExtensionViability).mockResolvedValueOnce(
      new Map(
        [...checks().keys()].map((name) => [
          name,
          { status: "available", reasons: [] },
        ]),
      ),
    );
    button("Recheck").click();
    await vi.waitFor(() =>
      expect(selectedNames()).toEqual([
        "Select blocked",
        "Select limited",
        "Select unknown",
      ]),
    );
    expect(root.textContent).toContain("3 of 4 selected");
    const all = root.querySelector<HTMLInputElement>(
      'input[aria-label="Select all available"]',
    )!;
    expect(all.indeterminate).toBe(true);
    all.checked = true;
    all.dispatchEvent(new Event("change", { bubbles: true }));
    expect(selectedNames()).toHaveLength(4);
  });

  it("selects viable extensions and explains excluded candidates before installing", async () => {
    let finish!: (value: Map<string, Viability>) => void;
    vi.mocked(checkExtensionViability).mockReturnValueOnce(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    const { host, mount } = fixture();
    const input = document.createElement("input");
    document.body.append(input);
    input.focus();
    const { root, button } = mount();
    await vi.waitFor(() =>
      expect(checkExtensionViability).toHaveBeenCalledOnce(),
    );
    expect(root.textContent).toBe("");
    finish(checks());
    await vi.waitFor(() => expect(button("Review")).toBeDefined());
    expect(document.activeElement).toBe(input);
    expect(root.querySelector("[data-extension]")).toBeNull();
    button("Review").click();
    expect(
      Array.from(root.querySelectorAll("[data-extension]")).map((row) =>
        row.getAttribute("data-extension"),
      ),
    ).toEqual(["blocked", "limited", "ready", "unknown"]);
    expect(
      Array.from(
        root.querySelectorAll<HTMLInputElement>(
          "[data-extension] input:checked",
        ),
      ).map((input) => input.getAttribute("aria-label")),
    ).toEqual(["Select limited", "Select ready"]);
    expect(root.textContent).toContain("No runtime");
    expect(root.textContent).toContain("Probe timed out");
    expect(root.textContent).toContain("Polling only");
    expect(root.textContent).toContain("Starts existing configured units.");
    button("Install selected").click();
    await vi.waitFor(() =>
      expect(host.installExtension).toHaveBeenCalledTimes(2),
    );
    expect(
      host.installExtension.mock.calls.map(([request]) => request.name).sort(),
    ).toEqual(["limited", "ready"]);
    await vi.waitFor(() =>
      expect(root.textContent).toContain("Installed ready"),
    );
  });

  it("remembers dismissal across reconnects and offers changed requirements again", async () => {
    vi.mocked(checkExtensionViability).mockImplementation(async () => checks());
    const { mount, manifest } = fixture();
    const first = mount();
    await vi.waitFor(() => expect(first.button("Dismiss")).toBeDefined());
    first.button("Dismiss").click();
    expect(first.root.textContent).toBe("");
    first.dispose();
    const second = mount();
    await vi.waitFor(() =>
      expect(checkExtensionViability).toHaveBeenCalledTimes(2),
    );
    expect(second.root.textContent).toBe("");
    second.dispose();
    manifest.extensions[0]!.requirements.probes = ["systemd"] as never;
    const third = mount();
    await vi.waitFor(() => expect(third.button("Review")).toBeDefined());
    third.button("Dismiss").click();
    third.dispose();
    manifest.extensions[0]!.blake3 = "02".repeat(32);
    const fourth = mount();
    await vi.waitFor(() => expect(fourth.button("Review")).toBeDefined());
  });

  it("keeps unavailable and unknown entries inspectable in Manage", async () => {
    vi.mocked(checkExtensionViability).mockImplementation(async () => checks());
    const { mount } = fixture();
    const { root } = mount(false);
    await vi.waitFor(() =>
      expect(root.textContent).toContain("Probe timed out"),
    );
    const blocked = root.querySelector('[data-extension="blocked"]')!;
    expect(blocked.textContent).toContain("No runtime");
    expect(blocked.querySelector("button")!.disabled).toBe(true);
    const unknown = root.querySelector('[data-extension="unknown"]')!;
    expect(unknown.querySelector("button")!.disabled).toBe(false);
    expect(
      root.querySelector('[data-extension="existing"]')!.textContent,
    ).toContain("Enable");
  });

  it("shares in-flight installs with Manage after the offer closes", async () => {
    vi.mocked(checkExtensionViability).mockImplementation(async () => checks());
    const { host, mount } = fixture();
    let finish!: () => void;
    const pending = new Promise<void>((resolve) => {
      finish = resolve;
    });
    const install = host.installExtension.getMockImplementation()!;
    host.installExtension.mockImplementation(async (request) => {
      await pending;
      return install(request);
    });
    const offer = mount();
    await vi.waitFor(() => expect(offer.button("Review")).toBeDefined());
    offer.button("Review").click();
    offer.root
      .querySelector<HTMLButtonElement>('[data-extension="ready"] button')!
      .click();
    await vi.waitFor(() =>
      expect(host.installExtension).toHaveBeenCalledOnce(),
    );
    const manage = mount(false);
    await vi.waitFor(() =>
      expect(
        manage.root.querySelector('[data-extension="ready"]'),
      ).not.toBeNull(),
    );
    const button = manage.root.querySelector<HTMLButtonElement>(
      '[data-extension="ready"] button',
    )!;
    expect(button.disabled).toBe(true);
    button.click();
    expect(host.installExtension).toHaveBeenCalledOnce();
    offer.dispose();
    finish();
    await vi.waitFor(() =>
      expect(manage.root.textContent).toContain("Installed ready"),
    );
    expect(
      manage.root
        .querySelector('[data-extension="ready"]')!
        .getAttribute("data-busy"),
    ).toBe("false");
  });
});
