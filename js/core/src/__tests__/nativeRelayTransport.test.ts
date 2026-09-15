import { describe, expect, it, vi } from "vitest";
import { YasNativeRelayTransport } from "../yas/nativeRelayTransport";
import type { YasRelayClient, YasRelayRoute } from "../yas/relay";
import { MockYasTransport } from "./mock-yas-transport";

const route: YasRelayRoute = {
  handle: 1n,
  generation: 1n,
  name: "work",
  label: "Work",
  description: "",
  availability: 1,
  transportHint: 0,
  flags: 0,
  extensions: [],
};

function fixture() {
  const tunnels: MockYasTransport[] = [];
  const relay = {
    connect: vi.fn(async () => {
      const tunnel = new MockYasTransport("disconnected");
      tunnels.push(tunnel);
      return { transport: tunnel, relayHandle: BigInt(tunnels.length), route };
    }),
    disconnect: vi.fn().mockResolvedValue(undefined),
  };
  const transport = new YasNativeRelayTransport(
    relay as unknown as YasRelayClient,
    route,
  );
  return { relay, transport, tunnels };
}

describe("reconnectable native Relay transport", () => {
  it("survives tunnel closure and synchronous reconnect from a status listener", async () => {
    const { transport, tunnels } = fixture();
    transport.connect();
    await vi.waitFor(() => expect(transport.status).toBe("connected"));
    transport.addEventListener("statuschange", (status) => {
      if (status === "disconnected") transport.connect();
    });
    tunnels[0].close();
    await vi.waitFor(() => expect(tunnels).toHaveLength(2));
    expect(transport.status).toBe("connected");
    const input = new Uint8Array([1, 2, 3]);
    transport.send(input);
    expect(tunnels[1].sent).toEqual([input]);
    transport.close();
    expect(transport.status).toBe("closed");
  });

  it("reports an unavailable home session as a retryable error", async () => {
    const { relay, transport } = fixture();
    relay.connect.mockImplementationOnce(() => {
      throw new Error("home session is reconnecting");
    });
    expect(() => transport.connect()).not.toThrow();
    await vi.waitFor(() => expect(transport.status).toBe("error"));
    expect(transport.lastError).toBe("home session is reconnecting");
    transport.reconnect();
    await vi.waitFor(() => expect(transport.status).toBe("connected"));
    transport.close();
  });

  it("invalidates the old session before a manual reconnect", async () => {
    const { transport } = fixture();
    transport.connect();
    await vi.waitFor(() => expect(transport.status).toBe("connected"));
    const statuses: string[] = [];
    transport.addEventListener("statuschange", (status) =>
      statuses.push(status),
    );
    transport.reconnect();
    await vi.waitFor(() => expect(transport.status).toBe("connected"));
    expect(statuses).toEqual(["disconnected", "connecting", "connected"]);
    transport.close();
  });

  it("does not start a queued attempt after disposal", async () => {
    const { relay, transport } = fixture();
    transport.connect();
    transport.close();
    await Promise.resolve();
    await Promise.resolve();
    expect(relay.connect).not.toHaveBeenCalled();
    expect(transport.status).toBe("closed");
  });
});
