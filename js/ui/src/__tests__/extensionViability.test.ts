import metadata from "../../../../extensions/requirements.json";
import { afterEach, describe, expect, it, vi } from "vitest";
import * as yas from "@yas-run/core";
import {
  checkExtensionCapabilities,
  checkExtensionViability,
  extensionOfferFingerprint,
  interpretProbe,
  parseRequirements,
  runExtensionProbe,
} from "../extensionViability";
import type { RegistryEntry } from "../extensionRegistry";

const definitions = Object.entries(metadata).map(
  ([name, requirements]) =>
    ({
      name,
      description: "",
      file: `${name}.wasm`,
      blake3: "01".repeat(32),
      bytes: 100,
      brotliBytes: 50,
      requirements: parseRequirements(requirements),
    }) satisfies RegistryEntry,
);

function hello(): yas.YasServerHello {
  const families = new Map<number, yas.YasFamilyDescriptor>();
  for (const [key, direction] of Object.entries(
    yas.YAS_OPERATION_DIRECTION_MASKS,
  )) {
    const [family, cls, kind] = key.split("/").map(Number) as [
      number,
      number,
      number,
    ];
    let descriptor = families.get(family);
    if (!descriptor) {
      descriptor = {
        family,
        version: 1,
        runtimeState: yas.YAS_CORE_RUNTIME_AVAILABLE,
        operations: [],
        limits: (yas.YAS_FAMILY_LIMIT_POLICIES[family] ?? []).map(
          ([tag, width, required, , max]) => ({
            tag,
            required,
            value:
              width === 4
                ? new yas.YasWriter().u32(Number(max)).finish()
                : new yas.YasWriter().u64(max).finish(),
          }),
        ),
      };
      families.set(family, descriptor);
    }
    (descriptor.operations as yas.YasOperation[]).push({
      class: cls,
      kind,
      direction,
    });
  }
  return {
    minor: 0,
    receiveMaxFrame: 65536,
    receiveMaxDecoded: 65536,
    receiveMaxDatagram: 0,
    receiveMaxBuffered: 1048576n,
    serverMonotonicNs: 0n,
    catalogRevision: 1n,
    serverRelease: "test",
    bootId: new Uint8Array(16),
    sessionId: new Uint8Array(16),
    serverName: "default",
    families: [...families.values()],
    extensions: [
      {
        tag: yas.YAS_CORE_SERVER_HELLO_EXTENSION_SUPPORT_EXTENSION,
        required: false,
        value: new yas.YasWriter().u32(15).finish(),
      },
      {
        tag: yas.YAS_CORE_SERVER_HELLO_PLATFORM_EXTENSION,
        required: false,
        value: new yas.YasWriter()
          .utf8U16("linux")
          .utf8U16("x86_64")
          .utf8U16("gnu")
          .finish(),
      },
    ],
  };
}

afterEach(() => vi.useRealTimers());

describe("extension viability", () => {
  it("checks all four published extensions using one shared server environment", async () => {
    const env = {
      get: vi.fn(async () => ({ entries: [], totalDataBytes: 0n })),
    };
    let handle = 0n;
    const process = {
      spawn: vi.fn(async () => ({
        processHandle: ++handle,
        stdout: {
          read: vi
            .fn()
            .mockResolvedValueOnce(
              new TextEncoder().encode(
                "ready\nsystem\nuser\nsignals\njournal\napplications\n",
              ),
            )
            .mockResolvedValue(null),
        },
      })),
      wait: vi.fn(async () => ({
        kind: yas.YAS_PROCESS_EXIT_KIND_CODE,
        code: 0,
      })),
    };
    const native = {
      env,
      process,
      connection: { hello: hello() },
    } as unknown as yas.YasNativeProductFamilies;
    const results = await checkExtensionViability(native, definitions);
    expect([...results].map(([name, result]) => [name, result.status])).toEqual(
      definitions.map((entry) => [entry.name, "available"]),
    );
    expect(env.get).toHaveBeenCalledOnce();
    expect(process.spawn).toHaveBeenCalledTimes(3);
  });

  it("understands every published requirement against the actual wire catalogue", () => {
    for (const entry of definitions) {
      expect(entry.requirements, entry.name).toBeDefined();
      expect(checkExtensionCapabilities(hello(), entry), entry.name).toEqual({
        status: "available",
        reasons: [],
      });
    }
  });

  it("treats missing or future metadata and old-server policy as unknown", () => {
    const entry = definitions[0]!;
    expect(
      parseRequirements({ ...entry.requirements, version: 2 }),
    ).toBeUndefined();
    expect(
      checkExtensionCapabilities(hello(), { ...entry, requirements: undefined })
        .status,
    ).toBe("unknown");
    expect(
      checkExtensionCapabilities({ ...hello(), extensions: [] }, entry).status,
    ).toBe("unknown");
    expect(
      yas.serverExtensionSupport([
        {
          tag: yas.YAS_CORE_SERVER_HELLO_EXTENSION_SUPPORT_EXTENSION,
          required: false,
          value: new Uint8Array(3),
        },
      ]),
    ).toBeNull();
  });

  it("rejects disabled persistence, unavailable runtimes, missing operations and oversized modules", () => {
    const entry = definitions[0]!;
    for (const flags of [14, 3]) {
      const server = hello();
      server.extensions[0]!.value = new yas.YasWriter().u32(flags).finish();
      expect(checkExtensionCapabilities(server, entry).status).toBe(
        "unavailable",
      );
    }
    const server = hello();
    const channel = server.families.find(
      (f) => f.family === yas.YAS_FAMILY_CHANNEL,
    )!;
    channel.operations = channel.operations.filter(
      (op) =>
        !(
          op.class === yas.YAS_CLASS_EVENT && op.kind === yas.YAS_CHANNEL_ACCEPT
        ),
    );
    expect(checkExtensionCapabilities(server, entry).reasons[0]).toContain(
      "ACCEPT",
    );
    expect(
      checkExtensionCapabilities(hello(), { ...entry, bytes: 1024 ** 3 })
        .status,
    ).toBe("unavailable");
  });

  it("keeps degraded systemd and empty desktop catalogues useful", () => {
    expect(interpretProbe("systemd", 0, "system\n")).toMatchObject({
      status: "limited",
      reasons: expect.arrayContaining([expect.stringContaining("polling")]),
    });
    expect(interpretProbe("systemd", 20, "").status).toBe("unavailable");
    expect(interpretProbe("systemd", 0, "signals\njournal\n").status).toBe(
      "unknown",
    );
    expect(interpretProbe("xdg-applications", 0, "").status).toBe("limited");
    expect(interpretProbe("muster-config", 21, "").status).toBe("unknown");
    expect(interpretProbe("muster-config", 0, "ready").status).toBe(
      "available",
    );
  });

  it("keeps dismissal across reconnects but offers new module updates and changed requirements", () => {
    const server = hello();
    const original = extensionOfferFingerprint(
      server,
      definitions,
      "https://registry",
    );
    expect(
      extensionOfferFingerprint(
        {
          ...server,
          bootId: new Uint8Array(16).fill(4),
          sessionId: new Uint8Array(16).fill(7),
        },
        definitions,
        "https://registry",
      ),
    ).toBe(original);
    expect(
      extensionOfferFingerprint(
        server,
        definitions.map((entry) => ({ ...entry, blake3: "02".repeat(32) })),
        "https://registry",
      ),
    ).not.toBe(original);
    expect(
      extensionOfferFingerprint(
        server,
        definitions.slice(1),
        "https://registry",
      ),
    ).not.toBe(original);
    server.families[0]!.runtimeState = yas.YAS_CORE_RUNTIME_UNAVAILABLE;
    expect(
      extensionOfferFingerprint(server, definitions, "https://registry"),
    ).not.toBe(original);
  });

  it("kills a late-spawned probe after its deadline without waiting on the UI", async () => {
    vi.useFakeTimers();
    let spawn!: (value: yas.YasProcessStreams) => void;
    const process = {
      spawn: vi.fn(
        () =>
          new Promise<yas.YasProcessStreams>((resolve) => {
            spawn = resolve;
          }),
      ),
      control: vi.fn(async () => 1n),
      wait: vi.fn(),
    };
    const native = {
      process,
      env: { get: vi.fn(async () => ({ entries: [] })) },
      connection: { hello: hello() },
    } as unknown as yas.YasNativeProductFamilies;
    const pending = runExtensionProbe(native, "systemd");
    await vi.advanceTimersByTimeAsync(5000);
    expect(process.spawn).toHaveBeenCalledWith(
      expect.objectContaining({ environmentKind: yas.YAS_PROCESS_ENV_EMPTY }),
      4096n,
      0n,
    );
    expect(await pending).toMatchObject({
      status: "unknown",
      reasons: ["Host check timed out."],
    });
    const reset = vi.fn();
    spawn({
      processHandle: 4n,
      stdout: { reset },
    } as unknown as yas.YasProcessStreams);
    await Promise.resolve();
    expect(reset).toHaveBeenCalled();
    expect(process.control).toHaveBeenCalledWith(
      expect.objectContaining({
        processHandle: 4n,
        action: yas.YAS_PROCESS_CONTROL_KILL,
      }),
    );
    expect(process.wait).not.toHaveBeenCalled();
  });

  it("bounds probe output and stops the process when that bound is exceeded", async () => {
    const reset = vi.fn();
    const process = {
      spawn: vi.fn(async () => ({
        processHandle: 5n,
        stdout: { read: vi.fn(async () => new Uint8Array(4097)), reset },
      })),
      wait: vi.fn(async () => ({
        kind: yas.YAS_PROCESS_EXIT_KIND_CODE,
        code: 0,
      })),
      control: vi.fn(async () => 1n),
    };
    const native = {
      process,
      env: { get: async () => ({ entries: [] }) },
      connection: { hello: hello() },
    } as unknown as yas.YasNativeProductFamilies;
    expect(await runExtensionProbe(native, "systemd")).toMatchObject({
      status: "unknown",
      reasons: ["Host check exceeded its output limit."],
    });
    expect(reset).toHaveBeenCalled();
    expect(process.control).toHaveBeenCalledWith(
      expect.objectContaining({ action: yas.YAS_PROCESS_CONTROL_KILL }),
    );
  });
});
