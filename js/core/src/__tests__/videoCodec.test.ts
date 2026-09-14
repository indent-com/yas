import { describe, expect, it } from "vitest";
import { av1SequenceProfile } from "../videoCodec";
import { SURFACE_HDR_PROBE } from "../surfaceHdrProbe";

describe("AV1 sequence profile", () => {
  it("reads the real HDR probe and skips delimiters and extension headers", () => {
    expect(av1SequenceProfile(SURFACE_HDR_PROBE)).toBe(0);
    expect(
      av1SequenceProfile(new Uint8Array([0x12, 0, 0x0e, 0, 1, 0x20])),
    ).toBe(1);
    expect(av1SequenceProfile(new Uint8Array([0x08, 0x40]))).toBe(2);
  });
  it.each([
    [],
    [0x80],
    [0x0a],
    [0x0a, 0x80],
    [0x0a, 2, 0x20],
    [0x0a, 1, 0xe0],
    [0x0e],
    [0x0e, 1, 1, 0x20],
    [0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f],
  ])("rejects incomplete or invalid OBUs: %j", (...bytes) => {
    expect(av1SequenceProfile(new Uint8Array(bytes))).toBeUndefined();
  });
});
