import { mkdtempSync, rmSync } from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { BunUnixSocketTransport } from "../transports/unix-bun";
import { NodeUnixSocketTransport } from "../transports/unix";
import type { AbstractUnixSocketTransport } from "../transports/unix-base";
import type { UnixSocketTransportOptions } from "../transports/unix-base";
import { YasConnection } from "../yas/session";
import { YAS_PREFACE } from "../yas/wire";

// These tests exercise real AF_UNIX sockets on POSIX.  The transport is
// intentionally local-only, so we skip on platforms where
// `net.connect({ path })` does not talk to a unix socket (Windows uses
// named pipes, not covered by these tests).
const skipOnWin32 = process.platform === "win32";

type Factory = (
  path: string,
  opts?: UnixSocketTransportOptions,
) => AbstractUnixSocketTransport;

// Bun's socket API is only available under the Bun runtime.  Skip the
// suite when running under plain Node/vitest.
const hasBun = typeof (globalThis as { Bun?: unknown }).Bun !== "undefined";

const suites: Array<{ name: string; skip: boolean; make: Factory }> = [
  {
    name: "NodeUnixSocketTransport",
    skip: skipOnWin32,
    make: (p, o) => new NodeUnixSocketTransport(p, o),
  },
  {
    name: "BunUnixSocketTransport",
    skip: skipOnWin32 || !hasBun,
    make: (p, o) => new BunUnixSocketTransport(p, o),
  },
];

for (const { name, skip, make } of suites) {
  const d = skip ? describe.skip : describe;

  d(name, () => {
    let tmp: string;
    let sockPath: string;
    let server: Server;
    let lastClient: Socket | null = null;
    let clientBytes = Buffer.alloc(0);

    beforeEach(async () => {
      tmp = mkdtempSync(join(tmpdir(), "yas-unix-transport-"));
      sockPath = join(tmp, "yas.sock");
      clientBytes = Buffer.alloc(0);
      lastClient = null;
      server = createServer((socket) => {
        lastClient = socket;
        socket.on("data", (chunk: Buffer) => {
          clientBytes = Buffer.concat([clientBytes, chunk]);
        });
      });
      await new Promise<void>((resolve) => server.listen(sockPath, resolve));
    });

    afterEach(async () => {
      await new Promise<void>((resolve) => server.close(() => resolve()));
      rmSync(tmp, { recursive: true, force: true });
    });

    async function waitFor<T>(
      probe: () => T | null | undefined,
      timeoutMs = 1000,
    ): Promise<T> {
      const start = Date.now();
      while (Date.now() - start < timeoutMs) {
        const v = probe();
        if (v !== null && v !== undefined && v !== false) return v as T;
        await new Promise((r) => setTimeout(r, 5));
      }
      throw new Error("timeout");
    }

    it("starts disconnected", () => {
      const t = make(sockPath);
      expect(t.status).toBe("disconnected");
      t.close();
    });

    it("connects and reports connected", async () => {
      const t = make(sockPath);
      const statuses: string[] = [];
      t.addEventListener("statuschange", (s) => statuses.push(s));
      t.connect();
      await waitFor(() => t.status === "connected");
      expect(statuses).toContain("connecting");
      expect(statuses).toContain("connected");
      t.close();
    });

    it("is a YAS byte-stream transport", () => {
      const t = make(sockPath);
      expect(t.yasFraming).toBe("stream");
      t.close();
    });

    it("writes bytes unchanged", async () => {
      const t = make(sockPath);
      t.connect();
      await waitFor(() => t.status === "connected");
      t.send(new Uint8Array([1, 2, 3]));
      t.send(new Uint8Array([4]));
      await waitFor(() => clientBytes.byteLength === 4);
      expect(Array.from(clientBytes)).toEqual([1, 2, 3, 4]);
      t.close();
    });

    it("delivers server bytes unchanged across chunk boundaries", async () => {
      const t = make(sockPath);
      const received: number[] = [];
      t.addEventListener("message", (d) => received.push(...new Uint8Array(d)));
      t.connect();
      await waitFor(() => t.status === "connected");
      await waitFor(() => lastClient !== null);
      const whole = [0xaa, 0xbb, 0xcc, 0xdd, 0xee];
      for (const byte of whole) lastClient!.write(Uint8Array.of(byte));
      await waitFor(() => received.length === whole.length);
      expect(received).toEqual(whole);
      t.close();
    });

    it("starts a YAS session with the raw preface and a framed HELLO", async () => {
      const t = make(sockPath, { reconnect: false });
      const connection = new YasConnection(t);
      const hello = connection.connect();
      hello.catch(() => {});
      await waitFor(() => clientBytes.byteLength > YAS_PREFACE.length + 4);
      expect(Array.from(clientBytes.subarray(0, YAS_PREFACE.length))).toEqual(
        Array.from(YAS_PREFACE),
      );
      const frameLength = clientBytes.readUInt32LE(YAS_PREFACE.length);
      await waitFor(
        () => clientBytes.byteLength === YAS_PREFACE.length + 4 + frameLength,
      );
      connection.close();
    });

    it("send() is a no-op before connected", async () => {
      const t = make(sockPath);
      // Intentionally do not call connect().
      t.send(new Uint8Array([1]));
      await new Promise((r) => setTimeout(r, 20));
      expect(clientBytes.byteLength).toBe(0);
      t.close();
    });

    it("close() prevents reconnect", async () => {
      const t = make(sockPath, { reconnectDelay: 20 });
      t.connect();
      await waitFor(() => t.status === "connected");
      t.close();
      expect(t.status).toBe("closed");
      // Wait past the reconnect interval and confirm no new connection.
      const before = server.connections ?? 0;
      await new Promise((r) => setTimeout(r, 100));
      const after = server.connections ?? 0;
      expect(after).toBeLessThanOrEqual(before);
    });

    it("reconnects after peer close when reconnect enabled", async () => {
      const t = make(sockPath, { reconnectDelay: 20 });
      t.connect();
      await waitFor(() => t.status === "connected");
      lastClient!.destroy();
      await waitFor(() => t.status === "disconnected");
      await waitFor(() => t.status === "connected", 2000);
      t.close();
    });

    it("reconnect:false disables reconnection", async () => {
      const t = make(sockPath, {
        reconnect: false,
        reconnectDelay: 10,
      });
      t.connect();
      await waitFor(() => t.status === "connected");
      const firstClient = lastClient!;
      firstClient.destroy();
      await waitFor(() => t.status === "disconnected");
      await new Promise((r) => setTimeout(r, 100));
      expect(t.status).toBe("disconnected");
      t.close();
    });

    it("reports error for unreachable socket path", async () => {
      const missing = join(tmp, "does-not-exist.sock");
      const t = make(missing, {
        reconnect: false,
        connectTimeoutMs: 200,
      });
      t.connect();
      await waitFor(() => t.status === "error" || t.status === "disconnected");
      expect(t.lastError).not.toBeNull();
      t.close();
    });
  });
}
