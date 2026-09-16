import { describe, it, expect } from "vitest";
import vectors from "../../../../protocol/yas/keyboard-vectors.json";
import {
  encodeTerminalKey,
  encodeTerminalText,
  type TerminalKey,
} from "../keyboardProtocol";
import { keyToBytes, TerminalKeyboard } from "../keyboard";
const decode = (bytes: Uint8Array | null) =>
  bytes === null ? null : new TextDecoder().decode(bytes);
describe("Kitty keyboard protocol", () => {
  it.each(vectors)("$name", ({ input, flags, expected }) => {
    expect(encodeTerminalKey(input as TerminalKey, flags)).toBe(expected);
  });
  it("encodes DOM layout alternatives and non-BMP text", () => {
    expect(
      decode(
        keyToBytes(
          new KeyboardEvent("keydown", {
            key: "+",
            code: "Equal",
            shiftKey: true,
            ctrlKey: true,
          }),
          false,
          5,
        ),
      ),
    ).toBe("\x1b[61:43;6u");
    expect(
      decode(
        keyToBytes(new KeyboardEvent("keydown", { key: "😀" }), false, 24),
      ),
    ).toBe("\x1b[128512;1;128512u");
  });
  it("does not mistake AltGraph text for Ctrl+Alt", () => {
    const e = new KeyboardEvent("keydown", {
      key: "@",
      code: "KeyQ",
      ctrlKey: true,
      altKey: true,
      modifierAltGraph: true,
    });
    expect(decode(keyToBytes(e, false, 1))).toBe("@");
  });
  it("tracks forwarded keys, releases on blur, and ignores late releases", () => {
    const keyboard = new TerminalKeyboard();
    const event = (type: string, repeat = false) =>
      new KeyboardEvent(type, { key: "a", code: "KeyA", repeat });
    expect(decode(keyboard.encode(event("keyup"), false, 11))).toBeNull();
    expect(decode(keyboard.encode(event("keydown"), false, 11))).toBe(
      "\x1b[97u",
    );
    expect(decode(keyboard.encode(event("keydown", true), false, 11))).toBe(
      "\x1b[97;1:2u",
    );
    expect(decode(keyboard.release(11))).toBe("\x1b[97;1:3u");
    expect(decode(keyboard.encode(event("keyup"), false, 11))).toBeNull();
  });
  it("encodes committed text and soft Enter without inventing a physical key", () => {
    expect(encodeTerminalText("你好", 24)).toBe("\x1b[0;1;20320:22909u");
    expect(encodeTerminalText("a\n", 8)).toBe("\x1b[97u\x1b[13u");
    expect(encodeTerminalText("a\n", 0)).toBe("a\r");
  });
});
