import { describe, expect, it } from "vitest";

import {
  YAS_PROCESS_DEFAULT_LIMITS,
  YAS_PROCESS_LIMIT_MAX_MUTATION_REPLAYS,
  YAS_PROCESS_LIMIT_MAX_PENDING_WAITS,
  YAS_PROCESS_LIMIT_MAX_PROCESSES_PER_SESSION,
  YAS_PROCESS_LIMIT_MAX_PROCESSES_PER_SESSION_EXTENDED,
  YAS_PROCESS_LIMIT_MAX_STREAM_BUFFER_BYTES,
  YAS_PROCESS_LIMIT_MAX_STREAM_BUFFER_BYTES_EXTENDED,
  YasCursor,
  YasWriter,
  processLimitsExtensions,
  processLimitsFromExtensions,
} from "../yas";

function u32(value: Uint8Array | undefined): number | undefined {
  return value ? new YasCursor(value).u32("limit") : undefined;
}

describe("Process family limits", () => {
  it("advertises an unconfigured server with the original ten tags", () => {
    const extensions = processLimitsExtensions(YAS_PROCESS_DEFAULT_LIMITS);
    expect(extensions).toHaveLength(10);
    expect(processLimitsFromExtensions(extensions)).toEqual(
      YAS_PROCESS_DEFAULT_LIMITS,
    );
  });

  it("clamps legacy tags and carries larger values in extended tags", () => {
    const configured = {
      ...YAS_PROCESS_DEFAULT_LIMITS,
      maxProcessesPerSession: 1024,
      maxProcesses: 4096,
      maxPendingSpawns: 64,
      maxStreamBufferBytes: 64n * 1024n * 1024n,
      maxEnvc: 4096,
      maxPendingWaits: 1024,
      maxPendingOperations: 256,
    };
    const extensions = processLimitsExtensions(configured);
    const tag = (t: number) => extensions.find((e) => e.tag === t)?.value;
    expect(u32(tag(YAS_PROCESS_LIMIT_MAX_PROCESSES_PER_SESSION))).toBe(16);
    expect(u32(tag(YAS_PROCESS_LIMIT_MAX_PROCESSES_PER_SESSION_EXTENDED))).toBe(
      1024,
    );
    expect(
      new YasCursor(tag(YAS_PROCESS_LIMIT_MAX_STREAM_BUFFER_BYTES)!).u64("l"),
    ).toBe(8n * 1024n * 1024n);
    expect(
      new YasCursor(
        tag(YAS_PROCESS_LIMIT_MAX_STREAM_BUFFER_BYTES_EXTENDED)!,
      ).u64("l"),
    ).toBe(64n * 1024n * 1024n);
    expect(u32(tag(YAS_PROCESS_LIMIT_MAX_PENDING_WAITS))).toBe(1024);
    expect(processLimitsFromExtensions(extensions)).toEqual(configured);

    const legacyOnly = extensions.filter(
      (e) => e.tag <= YAS_PROCESS_LIMIT_MAX_MUTATION_REPLAYS,
    );
    expect(processLimitsFromExtensions(legacyOnly)).toEqual(
      YAS_PROCESS_DEFAULT_LIMITS,
    );
  });

  it("rejects an extended value below its legacy tag", () => {
    const extensions = processLimitsExtensions({
      ...YAS_PROCESS_DEFAULT_LIMITS,
      maxProcessesPerSession: 1024,
    }).map((e) =>
      e.tag === YAS_PROCESS_LIMIT_MAX_PROCESSES_PER_SESSION_EXTENDED
        ? { ...e, value: new YasWriter().u32(8).finish() }
        : e,
    );
    expect(() => processLimitsFromExtensions(extensions)).toThrow(
      "invalid Process family limit",
    );
  });
});
