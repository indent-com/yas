import { describe, expect, it } from "vitest";
import {
  YAS_TERMINAL_FRAME_COMPONENTS,
  YAS_TERMINAL_GOLDEN_FRAME_FLAGS,
  decodeTerminalGridV1,
} from "../yas";
import { encodeBrowserTerminalGrid } from "../yas/terminalRenderer";
import { lz4Decompress } from "../lz4";

const keyframe = {
  viewId: 1,
  sequence: 1,
  flags: YAS_TERMINAL_GOLDEN_FRAME_FLAGS,
  gridPayload: new Uint8Array([1, 0, 1, ...Array(22).fill(0)]),
};

describe("terminal keyboard state", () => {
  it("retains delta state, carries resets, and defaults each keyframe to legacy", () => {
    const initial = decodeTerminalGridV1(keyframe, null, 1024);
    expect(initial.keyboardFlags).toBe(0);
    const enhanced = decodeTerminalGridV1(
      {
        viewId: 1,
        sequence: 2,
        flags: YAS_TERMINAL_FRAME_COMPONENTS,
        gridPayload: new Uint8Array([0, 1, 3, 0, 1, 31]),
      },
      initial,
      1024,
    );
    expect(enhanced.keyboardFlags).toBe(31);
    const retained = decodeTerminalGridV1(
      {
        viewId: 1,
        sequence: 3,
        flags: 0,
        gridPayload: new Uint8Array([0]),
      },
      enhanced,
      1024,
    );
    expect(retained.keyboardFlags).toBe(31);
    const reset = decodeTerminalGridV1(
      {
        viewId: 1,
        sequence: 4,
        flags: YAS_TERMINAL_FRAME_COMPONENTS,
        gridPayload: new Uint8Array([0, 1, 3, 0, 1, 0]),
      },
      retained,
      1024,
    );
    expect(reset.keyboardFlags).toBe(0);
    expect(
      decodeTerminalGridV1({ ...keyframe, sequence: 5 }, retained, 1024)
        .keyboardFlags,
    ).toBe(0);

    const renderer = encodeBrowserTerminalGrid(enhanced);
    const decoded = lz4Decompress(renderer);
    expect(decoded?.at(-1)).toBe(31);
  });

  it.each([[], [32], [1, 0]])(
    "rejects malformed keyboard flags %j",
    (...body) => {
      const initial = decodeTerminalGridV1(keyframe, null, 1024);
      expect(() =>
        decodeTerminalGridV1(
          {
            viewId: 1,
            sequence: 2,
            flags: YAS_TERMINAL_FRAME_COMPONENTS,
            gridPayload: new Uint8Array([0, 1, 3, 0, body.length, ...body]),
          },
          initial,
          1024,
        ),
      ).toThrow();
    },
  );

  it("skips unknown optional components with nonempty bodies", () => {
    const initial = decodeTerminalGridV1(keyframe, null, 1024);
    expect(
      decodeTerminalGridV1(
        {
          viewId: 1,
          sequence: 2,
          flags: YAS_TERMINAL_FRAME_COMPONENTS,
          gridPayload: new Uint8Array([0, 1, 99, 0, 2, 12, 34]),
        },
        initial,
        1024,
      ).keyboardFlags,
    ).toBe(0);
  });
});
