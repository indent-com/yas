import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { writeWorkspaceSessionUrl } from "../workspaceSessionUrl";
import type { EmbedOptions } from "../embed";

const ID = "123e4567-e89b-42d3-a456-426614174000";
const state = vi.hoisted(() => ({
  mount: vi.fn((_root: HTMLElement, _options: EmbedOptions) => () => {}),
  deviceId: vi.fn(async () => "123e4567-e89b-42d3-a456-426614174001"),
  transport: vi.fn(() => ({ status: "connecting" })),
}));

vi.mock("@yas-run/ui/embed", () => ({
  mountYasWorkspace: state.mount,
  getOrCreateWorkspaceSessionDeviceId: state.deviceId,
  shareTransport: state.transport,
}));
vi.mock("../../../web/src/lib/wasm", () => ({ initWasm: async () => ({}) }));

beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  // TextEncoder and externalized TweetNaCl use Node's typed-array realm.
  vi.stubGlobal("Uint8Array", new TextEncoder().encode("").constructor);
  localStorage.clear();
  document.body.innerHTML = '<div id="state"></div><div id="app" hidden></div>';
});

afterEach(() => {
  document.body.replaceChildren();
  localStorage.clear();
  history.replaceState(null, "", "/");
  vi.unstubAllGlobals();
});

async function openShare(hash: string) {
  history.replaceState(null, "", `/s${hash}`);
  await import("../../../web/src/share");
  await vi.waitFor(() => expect(state.mount).toHaveBeenCalledOnce());
  return state.mount.mock.calls[0][1];
}

it("mounts full-control shares with the regular home-server workspace shell", async () => {
  const options = await openShare(`#psk=example&workspace=${ID}&debug`);
  expect(options.home?.workspaceSessionDeviceId).toBe(
    "123e4567-e89b-42d3-a456-426614174001",
  );
  expect(options.home?.transport).toBe(state.transport.mock.results[0].value);
  expect(options.connections).toBeUndefined();
  expect(state.transport).toHaveBeenCalledWith(
    "wss://yas.run",
    "example",
    console,
  );
  expect(location.hash).toContain(`workspace=${ID}`);
  expect(location.hash).not.toContain("psk=");

  writeWorkspaceSessionUrl(ID, "replace");
  const reloadHash = location.hash;
  expect(reloadHash).toMatch(/^#e\./);
  vi.resetModules();
  vi.clearAllMocks();
  await openShare(reloadHash);
  expect(state.transport).toHaveBeenCalledWith(
    "wss://yas.run",
    "example",
    console,
  );
});

it.each(["workspace", "session"])(
  "uses the saved connection for a %s-only workspace link",
  async (field) => {
    localStorage.setItem("yas-share-last-psk", "saved-share");
    const options = await openShare(`#${field}=${ID}`);
    expect(options.home).toBeDefined();
    expect(state.transport).toHaveBeenCalledWith(
      "wss://yas.run",
      "saved-share",
      undefined,
    );
  },
);

it("remembers the connection after opening an encrypted share link", async () => {
  const { encryptPassphrase } =
    await import("../../../web/src/lib/passphrase-crypto");
  const hash = `#${encryptPassphrase("encrypted-share")}&workspace=${ID}`;
  await openShare(hash);
  expect(localStorage.getItem("yas-share-last-psk")).toBe("encrypted-share");
});

it("opens read-only shares without writable workspace-session stores", async () => {
  const options = await openShare("#example.ro");
  expect(options.home).toBeUndefined();
  expect(options.connections).toEqual([
    expect.objectContaining({ readOnly: true }),
  ]);
  expect(state.deviceId).not.toHaveBeenCalled();
});
