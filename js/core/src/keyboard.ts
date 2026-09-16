import {
  controlByte,
  encodeTerminalKey,
  namedKeys,
  REPORT_ALL,
  REPORT_EVENTS,
  type TerminalKey,
} from "./keyboardProtocol";
export {
  encodeTerminalText,
  REPORT_ALL,
  REPORT_EVENTS,
} from "./keyboardProtocol";
export const encoder = new TextEncoder();

export function ctrlCharToByte(char: string): Uint8Array | null {
  if (Array.from(char).length !== 1) return null;
  const byte = controlByte(char.codePointAt(0)!);
  return byte === null ? null : new Uint8Array([byte]);
}

const physicalCharacters: Record<string, string> = {
  Space: " ",
  Backquote: "`",
  Minus: "-",
  Equal: "=",
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Semicolon: ";",
  Quote: "'",
  Comma: ",",
  Period: ".",
  Slash: "/",
  IntlBackslash: "\\",
};
const shiftedCharacters: Record<string, string> = {
  Backquote: "~",
  Minus: "_",
  Equal: "+",
  BracketLeft: "{",
  BracketRight: "}",
  Backslash: "|",
  Semicolon: ":",
  Quote: '"',
  Comma: "<",
  Period: ">",
  Slash: "?",
  Digit1: "!",
  Digit2: "@",
  Digit3: "#",
  Digit4: "$",
  Digit5: "%",
  Digit6: "^",
  Digit7: "&",
  Digit8: "*",
  Digit9: "(",
  Digit0: ")",
};
function physicalCharacter(code: string): string | undefined {
  if (/^Key[A-Z]$/.test(code)) return code[3].toLowerCase();
  if (/^Digit[0-9]$/.test(code)) return code[5];
  return physicalCharacters[code];
}
let layout = new Map<string, string>();
/** Layout maps are optional in browsers. Observed unshifted keys also teach
 * the fallback, and unavailable metadata is simply omitted. */
export async function refreshKeyboardLayout(): Promise<void> {
  const keyboard = (
    navigator as Navigator & {
      keyboard?: { getLayoutMap?: () => Promise<Map<string, string>> };
    }
  ).keyboard;
  try {
    if (keyboard?.getLayoutMap) layout = new Map(await keyboard.getLayoutMap());
  } catch {
    /* Unsupported or disallowed by the embedding page. */
  }
}

export function terminalKeyFromEvent(e: KeyboardEvent): TerminalKey | null {
  if (e.isComposing || e.key === "Dead" || e.key === "Process") return null;
  const altGraph = e.getModifierState?.("AltGraph") ?? false;
  const control = e.ctrlKey && !altGraph;
  const alt = e.altKey && !altGraph;
  let modifiers =
    (e.shiftKey ? 1 : 0) |
    (alt ? 2 : 0) |
    (control ? 4 : 0) |
    (e.metaKey ? 8 : 0) |
    (e.getModifierState?.("CapsLock") ? 64 : 0) |
    (e.getModifierState?.("NumLock") ? 128 : 0);
  let key = namedKeys[e.key];
  let text = "";
  let shiftedKey: number | undefined;
  let baseKey: number | undefined;
  const character =
    Array.from(e.key).length === 1 && e.key.codePointAt(0)! >= 32;
  if (character) {
    const physical = physicalCharacter(e.code);
    const lower = Array.from(e.key.toLowerCase());
    if (e.code && !e.shiftKey && !control && !alt && !e.metaKey && !altGraph)
      layout.set(e.code, lower.length === 1 ? lower[0] : e.key);
    let unshifted =
      layout.get(e.code) ?? (lower.length === 1 ? lower[0] : e.key);
    if (
      !layout.has(e.code) &&
      e.shiftKey &&
      shiftedCharacters[e.code] === e.key
    )
      unshifted = physical ?? unshifted;
    key =
      Array.from(unshifted).length === 1
        ? unshifted.codePointAt(0)!
        : e.key.codePointAt(0)!;
    shiftedKey = e.shiftKey ? e.key.codePointAt(0)! : undefined;
    baseKey = physical?.codePointAt(0);
    if (!control && !alt && !e.metaKey) text = e.key;
    // Option-generated non-ASCII characters are resolved text on macOS.
    if (
      alt &&
      !control &&
      !e.metaKey &&
      e.key.codePointAt(0)! > 127 &&
      /Mac/.test(navigator.platform)
    ) {
      modifiers &= ~2;
      text = e.key;
    }
  } else if (/^F([1-9]|[12][0-9]|3[0-5])$/.test(e.key)) {
    key = 57363 + Number(e.key.slice(1));
  } else if (key === undefined) {
    const modifiersByCode: Record<string, number> = {
      ShiftLeft: 57441,
      ControlLeft: 57442,
      AltLeft: 57443,
      MetaLeft: 57444,
      HyperLeft: 57445,
      SuperLeft: 57444,
      ShiftRight: 57447,
      ControlRight: 57448,
      AltRight: 57449,
      MetaRight: 57450,
      HyperRight: 57451,
      SuperRight: 57450,
    };
    key = e.key === "AltGraph" ? 57453 : modifiersByCode[e.code];
    if (key === undefined && control) {
      // Some browsers report Ctrl+letters as a C0 byte or Unidentified.
      const physical = physicalCharacter(e.code);
      if (physical) key = physical.codePointAt(0)!;
      else if (
        e.key.length === 1 &&
        e.key.charCodeAt(0) >= 1 &&
        e.key.charCodeAt(0) <= 26
      )
        key = e.key.charCodeAt(0) + 96;
    }
  }
  if (e.code?.startsWith("Numpad") || e.location === 3) {
    const keypad: Record<string, number> = {
      Decimal: 57409,
      Divide: 57410,
      Multiply: 57411,
      Subtract: 57412,
      Add: 57413,
      Enter: 57414,
      Equal: 57415,
      Comma: 57416,
    };
    const navigation: Record<string, number> = {
      ArrowLeft: 57417,
      ArrowRight: 57418,
      ArrowUp: 57419,
      ArrowDown: 57420,
      PageUp: 57421,
      PageDown: 57422,
      Home: 57423,
      End: 57424,
      Insert: 57425,
      Delete: 57426,
      Clear: 57427,
    };
    const suffix = e.code?.slice(6);
    key =
      navigation[e.key] ??
      keypad[suffix] ??
      (/^[0-9]$/.test(suffix) ? 57399 + Number(suffix) : key);
  }
  if (key === undefined) return null;
  return {
    key,
    modifiers,
    eventType: e.type === "keyup" ? 3 : e.repeat ? 2 : 1,
    text,
    shiftedKey,
    baseKey,
  };
}

/** Convert a DOM key using the application's negotiated keyboard flags. */
export function keyToBytes(
  e: KeyboardEvent,
  appCursor: boolean,
  flags = 0,
): Uint8Array | null {
  if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey && e.code === "KeyV")
    return null;
  // Preserve host Command shortcuts in legacy terminal mode.
  if (!flags && e.metaKey && Array.from(e.key).length === 1) return null;
  // Brave sometimes reports Shift+digit as its unshifted digit. Let the
  // textarea provide the layout-resolved text in legacy mode.
  if (
    !(flags & REPORT_ALL) &&
    e.shiftKey &&
    !e.ctrlKey &&
    !e.altKey &&
    !e.metaKey &&
    /^[0-9]$/.test(e.key) &&
    e.code?.startsWith("Digit")
  )
    return null;
  const key = terminalKeyFromEvent(e);
  if (!key) return null;
  const value = encodeTerminalKey(key, flags, appCursor);
  return value === null ? null : encoder.encode(value);
}

/** Track only keys actually forwarded by a pane; locally consumed shortcuts
 * never produce orphan releases. Blur releases held keys before losing input. */
export class TerminalKeyboard {
  private held = new Map<string, TerminalKey>();
  private flags = 0;
  encode(
    e: KeyboardEvent,
    appCursor: boolean,
    flags: number,
  ): Uint8Array | null {
    if (this.flags !== flags) this.held.clear();
    this.flags = flags;
    const id = e.code || e.key;
    if (e.type === "keyup") {
      const held = this.held.get(id);
      this.held.delete(id);
      const current = terminalKeyFromEvent(e);
      if (!held || !current) return null;
      const value = encodeTerminalKey(
        { ...held, modifiers: current.modifiers, eventType: 3, text: "" },
        flags,
        appCursor,
      );
      return value === null ? null : encoder.encode(value);
    }
    const bytes = keyToBytes(e, appCursor, flags);
    if (bytes && flags & REPORT_EVENTS) {
      const key = terminalKeyFromEvent(e);
      if (key) this.held.set(id, key);
    }
    return bytes;
  }
  release(flags: number): Uint8Array | null {
    const held = [...this.held.values()];
    this.held.clear();
    if (flags !== this.flags || !(flags & REPORT_EVENTS)) return null;
    return encoder.encode(
      held
        .map(
          (key) =>
            encodeTerminalKey(
              { ...key, modifiers: 0, eventType: 3, text: "" },
              flags,
            ) ?? "",
        )
        .join(""),
    );
  }
}
