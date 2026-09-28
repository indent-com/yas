import { describe, expect, it, vi } from "vitest";
import {
  YAS_EXTENSION_CONTROL_RESTART,
  YAS_EXTENSION_CONTROL_REMOVE,
  YAS_EXTENSION_PHASE_RUNNING,
  YAS_EXTENSION_PHASE_NEED_OBJECT,
  YAS_EXTENSION_RUNTIME_AUTO,
  YAS_FAMILY_EXTENSION,
  YAS_FAMILY_LIMIT_POLICIES,
} from "../yas/generated";
import type {
  YasExtensionRecord,
  YasExtensionSnapshot,
} from "../yas/extension";
import { YasNativeExtensionFacade } from "../yas/nativeExtensionFacade";
import { YasWriter } from "../yas/wire";

type FacadeClient = ConstructorParameters<typeof YasNativeExtensionFacade>[1];
type FacadeConnection = ConstructorParameters<
  typeof YasNativeExtensionFacade
>[0];

function nativeRecord(): YasExtensionRecord {
  return {
    extensionHandle: 0xfedc_ba98_7654_3210n,
    generation: 0x8000_0000_0000_0011n,
    definitionRevision: 0x8000_0000_0000_0022n,
    phase: YAS_EXTENSION_PHASE_RUNNING,
    runtime: 1,
    restartPolicy: 2,
    flags: 15,
    attempt: 0x8000_0000_0000_0033n,
    lastRunningAttempt: 0x8000_0000_0000_0033n,
    taskId: 7,
    nextStartUnixMs: 0n,
    directoryRevision: 4n,
    contentHash: new Uint8Array(32).fill(7),
    name: "systemd",
    runtimeLimits: {
      memoryBytes: 0n,
      stackBytes: 0n,
      maxActiveJobs: 0,
      maxPendingJobs: 0,
      maxJobBytes: 0n,
      slowConsumerTimeoutNs: 0n,
    },
    extensions: [],
  };
}

function connection(): FacadeConnection {
  const limits = YAS_FAMILY_LIMIT_POLICIES[YAS_FAMILY_EXTENSION]!.map(
    ([tag, width, required, , hardMax]) => ({
      tag,
      required,
      value:
        width === 4
          ? new YasWriter().u32(Number(hardMax)).finish()
          : new YasWriter().u64(hardMax).finish(),
    }),
  );
  return {
    family: vi.fn(() => ({
      family: YAS_FAMILY_EXTENSION,
      version: 1,
      runtimeState: 1,
      operations: [],
      limits,
    })),
    onInvalidation: vi.fn(() => () => undefined),
  };
}

function clientFor(
  record: YasExtensionRecord,
  overrides: Partial<FacadeClient> = {},
): FacadeClient {
  return {
    list: vi.fn().mockResolvedValue({ revision: 1n, definitions: [record] }),
    control: vi.fn(),
    deploy: vi.fn(),
    uploadObject: vi.fn(),
    catalog: {
      snapshot: { revision: 1n, definitions: [record] },
      subscribe: vi.fn(() => () => undefined),
      unwatch: vi.fn().mockResolvedValue(undefined),
    },
    ...overrides,
  };
}

describe("YasNativeExtensionFacade", () => {
  it("waits for confirmed removal while unrelated catalogue updates arrive", async () => {
    const record = nativeRecord();
    const other = { ...record, extensionHandle: 2n, name: "other" };
    let notify!: (snapshot: YasExtensionSnapshot) => void;
    const unsubscribe = vi.fn();
    const catalog = {
      snapshot: { revision: 1n, definitions: [record, other] },
      subscribe: vi.fn((listener: typeof notify) => {
        notify = listener;
        return unsubscribe;
      }),
      unwatch: vi.fn().mockResolvedValue(undefined),
    };
    const client = clientFor(record, {
      catalog,
      control: vi.fn().mockResolvedValue(record),
    });
    const facade = new YasNativeExtensionFacade(connection(), client);
    const settled = vi.fn();
    const removal = facade
      .controlExtension(record.extensionHandle, YAS_EXTENSION_CONTROL_REMOVE)
      .then(settled);
    await vi.waitFor(() => expect(catalog.subscribe).toHaveBeenCalledOnce());

    // An update for a different extension is not deletion confirmation.
    notify({
      revision: 2n,
      definitions: [
        record,
        { ...other, definitionRevision: other.definitionRevision + 1n },
      ],
    });
    await Promise.resolve();
    expect(settled).not.toHaveBeenCalled();
    // A reset's empty inventory is not authoritative either.
    notify({ revision: 0n, definitions: [] });
    await Promise.resolve();
    expect(settled).not.toHaveBeenCalled();
    catalog.snapshot = { revision: 3n, definitions: [other] };
    notify(catalog.snapshot);
    await removal;
    expect(settled).toHaveBeenCalledWith(null);
    expect(unsubscribe).toHaveBeenCalledOnce();
  });

  it("rejects a pending removal on disposal and releases its catalogue listener", async () => {
    const record = nativeRecord();
    const unsubscribe = vi.fn();
    const client = clientFor(record, {
      control: vi.fn().mockResolvedValue(record),
    });
    client.catalog.subscribe = vi.fn(() => unsubscribe);
    const facade = new YasNativeExtensionFacade(connection(), client);
    const removal = facade.controlExtension(
      record.extensionHandle,
      YAS_EXTENSION_CONTROL_REMOVE,
    );
    const rejected = expect(removal).rejects.toThrow("disposed");
    await vi.waitFor(() =>
      expect(client.catalog.subscribe).toHaveBeenCalledOnce(),
    );
    facade.dispose();
    await rejected;
    expect(unsubscribe).toHaveBeenCalledOnce();
  });

  it("controls the exact native handle, generation, and revision", async () => {
    const record = nativeRecord();
    const control = vi.fn().mockResolvedValue({
      extensionHandle: record.extensionHandle,
      generation: record.generation,
      definitionRevision: record.definitionRevision,
      extensions: [],
    });
    const client = clientFor(record, {
      control,
    });
    const facade = new YasNativeExtensionFacade(connection(), client);

    const result = await facade.controlExtension(
      record.extensionHandle,
      YAS_EXTENSION_CONTROL_RESTART,
    );
    expect(result).toBe(record);
    expect(control).toHaveBeenCalledWith(
      expect.objectContaining({
        extensionHandle: record.extensionHandle,
        generation: record.generation,
        expectedDefinitionRevision: record.definitionRevision,
        action: YAS_EXTENSION_CONTROL_RESTART,
      }),
    );
  });

  it("accepts a coalesced later revision from the same generation", async () => {
    const record = nativeRecord();
    const committed = {
      ...record,
      definitionRevision: record.definitionRevision + 1n,
    };
    const delivered = {
      ...committed,
      definitionRevision: committed.definitionRevision + 1n,
      attempt: committed.attempt + 1n,
    };
    const control = vi.fn().mockResolvedValue({
      extensionHandle: committed.extensionHandle,
      generation: committed.generation,
      definitionRevision: committed.definitionRevision,
      extensions: [],
    });
    const client = clientFor(record, {
      control,
      catalog: {
        snapshot: { revision: 3n, definitions: [delivered] },
        subscribe: vi.fn(() => () => undefined),
        unwatch: vi.fn().mockResolvedValue(undefined),
      },
    });
    const facade = new YasNativeExtensionFacade(connection(), client);

    const result = await facade.controlExtension(
      record.extensionHandle,
      YAS_EXTENSION_CONTROL_RESTART,
    );

    expect(result).toBe(delivered);
  });

  it("waits for the catalogue to reach a restarted generation", async () => {
    const record = nativeRecord();
    const restarted = {
      ...record,
      generation: record.generation + 1n,
      attempt: record.attempt + 1n,
    };
    const control = vi.fn().mockResolvedValue({
      extensionHandle: restarted.extensionHandle,
      generation: restarted.generation,
      definitionRevision: restarted.definitionRevision,
      extensions: [],
    });
    const subscribe = vi.fn(
      (listener: (snapshot: { definitions: YasExtensionRecord[] }) => void) => {
        listener({ definitions: [restarted] });
        return () => undefined;
      },
    );
    const client = clientFor(record, {
      control,
      catalog: {
        snapshot: { revision: 1n, definitions: [record] },
        subscribe,
        unwatch: vi.fn().mockResolvedValue(undefined),
      },
    });
    const facade = new YasNativeExtensionFacade(connection(), client);

    const result = await facade.controlExtension(
      record.extensionHandle,
      YAS_EXTENSION_CONTROL_RESTART,
    );

    expect(subscribe).toHaveBeenCalledOnce();
    expect(result).toBe(restarted);
  });

  it("deploys an update with its full high-bit CAS identity", async () => {
    const record = nativeRecord();
    const deploy = vi.fn().mockResolvedValue({
      extensionHandle: record.extensionHandle,
      generation: record.generation,
      definitionRevision: record.definitionRevision,
      extensions: [],
    });
    const client = clientFor(record, {
      deploy,
    });
    const facade = new YasNativeExtensionFacade(connection(), client);

    await facade.installExtension({
      contentHash: record.contentHash,
      name: record.name,
      expectedExtensionHandle: record.extensionHandle,
      expectedGeneration: record.generation,
      expectedDefinitionRevision: record.definitionRevision,
      module: vi.fn(() => Promise.reject(new Error("object not requested"))),
    });
    expect(deploy).toHaveBeenCalledWith(
      expect.objectContaining({
        expectedExtensionHandle: record.extensionHandle,
        expectedGeneration: record.generation,
        expectedDefinitionRevision: record.definitionRevision,
        runtime: YAS_EXTENSION_RUNTIME_AUTO,
      }),
    );
  });

  it("uploads a changed update before deploying it", async () => {
    const record = nativeRecord();
    const contentHash = new Uint8Array(32).fill(9);
    const updated = {
      ...record,
      definitionRevision: record.definitionRevision + 1n,
      contentHash,
    };
    const order: string[] = [];
    const catalog = {
      snapshot: { revision: 1n, definitions: [record] },
      subscribe: vi.fn(() => () => undefined),
      unwatch: vi.fn().mockResolvedValue(undefined),
    };
    const uploadObject = vi.fn(async () => {
      order.push("upload");
    });
    const deploy = vi.fn(async () => {
      order.push("deploy");
      catalog.snapshot = { revision: 2n, definitions: [updated] };
      return {
        extensionHandle: updated.extensionHandle,
        generation: updated.generation,
        definitionRevision: updated.definitionRevision,
        extensions: [],
      };
    });
    const client = clientFor(record, { catalog, uploadObject, deploy });
    const facade = new YasNativeExtensionFacade(connection(), client);
    const module = vi.fn().mockResolvedValue(new Uint8Array([0, 97, 115, 109]));
    const progress: string[] = [];

    const result = await facade.installExtension({
      contentHash,
      name: record.name,
      expectedExtensionHandle: record.extensionHandle,
      expectedGeneration: record.generation,
      expectedDefinitionRevision: record.definitionRevision,
      module,
      onProgress: (stage) => progress.push(stage),
    });

    expect(order).toEqual(["upload", "deploy"]);
    expect(uploadObject).toHaveBeenCalledOnce();
    expect(module).toHaveBeenCalledOnce();
    expect(result).toBe(updated);
    expect(progress).toEqual([
      "checking",
      "downloading",
      "uploading",
      "deploying",
      "waiting",
    ]);
  });

  it("reports lazy object upload stages and skips them for cached objects", async () => {
    for (const cached of [false, true]) {
      const record = nativeRecord();
      const catalog = {
        snapshot: { revision: 1n, definitions: [] as YasExtensionRecord[] },
        subscribe: vi.fn(() => () => undefined),
        unwatch: vi.fn().mockResolvedValue(undefined),
      };
      let uploaded = cached;
      const client = clientFor(record, {
        catalog,
        deploy: vi.fn(async () => {
          catalog.snapshot = {
            revision: 2n,
            definitions: [
              {
                ...record,
                phase: uploaded
                  ? YAS_EXTENSION_PHASE_RUNNING
                  : YAS_EXTENSION_PHASE_NEED_OBJECT,
              },
            ],
          };
          return { ...record, extensions: [] };
        }),
        uploadObject: vi.fn(async () => {
          uploaded = true;
        }),
      });
      const progress: string[] = [];
      const facade = new YasNativeExtensionFacade(connection(), client);
      await facade.installExtension({
        name: record.name,
        contentHash: record.contentHash,
        module: async () => new Uint8Array([1]),
        onProgress: (stage) => progress.push(stage),
      });
      expect(progress).toEqual(
        cached
          ? ["checking", "deploying", "waiting"]
          : [
              "checking",
              "deploying",
              "waiting",
              "downloading",
              "uploading",
              "deploying",
              "waiting",
            ],
      );
    }
  });
});
