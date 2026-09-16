/** Kitty progressive enhancement bits, as negotiated by the application. */
export const DISAMBIGUATE = 1;
export const REPORT_EVENTS = 2;
export const REPORT_ALTERNATES = 4;
export const REPORT_ALL = 8;
export const REPORT_TEXT = 16;

/** Functional keys use Kitty's private-use numbers internally, including
 * navigation keys whose wire representation uses a letter or tilde. */
export interface TerminalKey {
  key: number;
  modifiers: number;
  eventType: 1 | 2 | 3;
  text?: string;
  shiftedKey?: number;
  baseKey?: number;
}

export const namedKeys: Readonly<Record<string, number>> = {
  Escape: 27,
  Enter: 13,
  Return: 13,
  Tab: 9,
  Backspace: 127,
  Insert: 57348,
  Delete: 57349,
  ArrowLeft: 57350,
  ArrowRight: 57351,
  ArrowUp: 57352,
  ArrowDown: 57353,
  PageUp: 57354,
  PageDown: 57355,
  Home: 57356,
  End: 57357,
  CapsLock: 57358,
  ScrollLock: 57359,
  NumLock: 57360,
  PrintScreen: 57361,
  Pause: 57362,
  ContextMenu: 57363,
  MediaPlay: 57428,
  MediaPause: 57429,
  MediaPlayPause: 57430,
  MediaReverse: 57431,
  MediaStop: 57432,
  MediaFastForward: 57433,
  MediaRewind: 57434,
  MediaTrackNext: 57435,
  MediaTrackPrevious: 57436,
  MediaRecord: 57437,
  AudioVolumeDown: 57438,
  AudioVolumeUp: 57439,
  AudioVolumeMute: 57440,
};

export function controlByte(key: number): number | null {
  if (key >= 97 && key <= 122) return key - 96;
  if (key >= 65 && key <= 90) return key - 64;
  const controls: Record<number, number> = {
    32: 0,
    64: 0,
    50: 0,
    51: 27,
    91: 27,
    52: 28,
    92: 28,
    53: 29,
    93: 29,
    54: 30,
    94: 30,
    126: 30,
    55: 31,
    95: 31,
    47: 31,
    56: 127,
    63: 127,
  };
  return controls[key] ?? null;
}

function keypadEquivalent(key: number): number {
  if (key >= 57399 && key <= 57408) return key - 57399 + 48;
  return (
    (
      {
        57409: 46,
        57410: 47,
        57411: 42,
        57412: 45,
        57413: 43,
        57414: 13,
        57415: 61,
        57416: 44,
        57417: 57350,
        57418: 57351,
        57419: 57352,
        57420: 57353,
        57421: 57354,
        57422: 57355,
        57423: 57356,
        57424: 57357,
        57425: 57348,
        57426: 57349,
      } as Record<number, number>
    )[key] ?? key
  );
}

/** Encode an already normalized key. Shared fixtures also exercise the native
 * viewer's encoder. Text from paste never passes through this function. */
export function encodeTerminalKey(
  input: TerminalKey,
  flags: number,
  appCursor = false,
): string | null {
  const all = !!(flags & REPORT_ALL);
  const disambiguate = all || !!(flags & DISAMBIGUATE);
  const events = !!(flags & REPORT_EVENTS);
  const release = input.eventType === 3;
  if (release && !events) return null;
  let key = input.key;
  if (!disambiguate) key = keypadEquivalent(key);
  const functional = key >= 57348 && key <= 57454;
  const modifierKey = key >= 57441 && key <= 57454;
  if (modifierKey && !all) return null;
  // Lock state is not attached to text-producing keys in compatibility mode.
  const modifiers = input.modifiers & (all || functional ? 255 : 63);
  const chord = modifiers & 63;
  const text = input.text ?? "";
  const c0 = key === 13 || key === 9 || key === 127;
  if (release && c0 && !all) return null;
  const escape =
    all ||
    (disambiguate &&
      (functional || key === 27 || (chord & ~1) !== 0 || (c0 && chord !== 0)));
  const character =
    text ||
    String.fromCodePoint(
      chord & 1 && input.shiftedKey ? input.shiftedKey : key,
    );
  if (!escape && !release) {
    if (!functional && key >= 32 && key !== 127 && (chord & ~1) === 0)
      return character;
    if (c0 || key === 27) {
      // Preserve YAS's explicit Ctrl+Enter binding even for legacy programs.
      if (key === 13 && chord & 4) return `\x1b[13;${modifiers + 1}u`;
      if (!(chord & ~7)) {
        const alt = chord & 2 ? "\x1b" : "";
        if (key === 9 && chord & 1) return alt + "\x1b[Z";
        return alt + String.fromCodePoint(key === 127 && chord & 4 ? 8 : key);
      }
    }
    if (!functional && key >= 32 && key !== 127 && !(chord & ~7)) {
      // Ctrl+Shift has its own CSI-u encoding; Ctrl+Alt retains the familiar
      // ESC-prefixed control byte when extended mode has not been requested.
      if ((chord & 5) !== 5) {
        const value = chord & 4 ? controlByte(key) : null;
        return (
          (chord & 2 ? "\x1b" : "") +
          (value === null ? character : String.fromCodePoint(value))
        );
      }
    }
  }
  let number = String(key);
  let suffix = "u";
  const letters: Record<number, string> = {
    57350: "D",
    57351: "C",
    57352: "A",
    57353: "B",
    57356: "H",
    57357: "F",
    57364: "P",
    57365: "Q",
    57367: "S",
    57427: "E",
  };
  const tildes: Record<number, number> = {
    57348: 2,
    57349: 3,
    57354: 5,
    57355: 6,
    57368: 15,
    57369: 17,
    57370: 18,
    57371: 19,
    57372: 20,
    57373: 21,
    57374: 23,
    57375: 24,
  };
  if (key === 57366) {
    // CSI R conflicts with the cursor-position report in enhanced mode.
    if (!flags && !modifiers && !release) return "\x1bOR";
    number = flags ? "13" : "1";
    suffix = flags ? "~" : "R";
  } else if (letters[key]) {
    suffix = letters[key];
    if (!flags && !modifiers && !release) {
      if (
        (key >= 57364 && key <= 57367) ||
        (appCursor && key >= 57350 && key <= 57353)
      )
        return "\x1bO" + suffix;
    }
    number = "1";
  } else if (tildes[key]) {
    number = String(tildes[key]);
    suffix = "~";
  } else if (key === 57363 && !flags) {
    number = "29";
    suffix = "~";
  }
  if (suffix === "u" && flags & REPORT_ALTERNATES) {
    const shifted = chord & 1 ? input.shiftedKey : undefined;
    const base =
      input.baseKey && input.baseKey !== key ? input.baseKey : undefined;
    if (shifted && shifted !== key) number += `:${shifted}`;
    else if (base) number += ":";
    if (base) number += `:${base}`;
  }
  const event = events && input.eventType !== 1 ? `:${input.eventType}` : "";
  const associated =
    all &&
    flags & REPORT_TEXT &&
    !release &&
    text &&
    !/[\x00-\x1f\x7f-\x9f]/u.test(text)
      ? Array.from(text, (c) => c.codePointAt(0)!).join(":")
      : "";
  const parameters =
    modifiers || event || associated ? `;${modifiers + 1}${event}` : "";
  if (number === "1" && suffix !== "u" && suffix !== "~" && !parameters)
    number = "";
  return `\x1b[${number}${parameters}${associated ? ";" + associated : ""}${suffix}`;
}

/** IME/soft keyboard input has text but no reliable physical key. */
export function encodeTerminalText(text: string, flags: number): string {
  if (!(flags & REPORT_ALL)) return text.replace(/\n/g, "\r");
  // Control characters (including soft Enter) remain individual key events.
  return text
    .split(/([\r\n\t\x7f])/u)
    .map((part) => {
      if (!part) return "";
      const control = part === "\n" ? 13 : part.codePointAt(0)!;
      if (
        part.length === 1 &&
        (control === 13 || control === 9 || control === 127)
      )
        return (
          encodeTerminalKey(
            { key: control, modifiers: 0, eventType: 1 },
            flags,
          ) ?? ""
        );
      if (flags & REPORT_TEXT)
        return (
          encodeTerminalKey(
            { key: 0, modifiers: 0, eventType: 1, text: part },
            flags,
          ) ?? ""
        );
      return Array.from(
        part,
        (c) =>
          encodeTerminalKey(
            { key: c.codePointAt(0)!, modifiers: 0, eventType: 1 },
            flags,
          ) ?? "",
      ).join("");
    })
    .join("");
}
