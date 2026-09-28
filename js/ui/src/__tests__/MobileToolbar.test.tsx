import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import {
  PALETTES,
  YasTerminalSurface,
  terminalSurfaceForInput,
} from "@yas-run/core";
import { MobileToolbar } from "../MobileToolbar";
import { themeFor, uiScale } from "../theme";
import { t } from "../i18n";

vi.mock("@yas-run/core", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@yas-run/core")>()),
  terminalSurfaceForInput: vi.fn(),
}));

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.replaceChildren();
  vi.restoreAllMocks();
});

function touch(button: HTMLElement, type: string, x = 20) {
  const point = { identifier: 1, clientX: x, clientY: 20 };
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperties(event, {
    touches: {
      value: type === "touchend" || type === "touchcancel" ? [] : [point],
    },
    changedTouches: { value: [point] },
  });
  button.dispatchEvent(event);
  return event;
}

function mount() {
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
    measureText: () => ({ width: 8 }),
  } as never);
  const input = document.createElement("textarea");
  const host = document.createElement("div");
  document.body.append(input, host);
  input.focus();
  const terminal = new YasTerminalSurface({ sessionId: "s1" });
  const sendInput = vi.fn();
  const sendClipboard = vi.fn().mockResolvedValue(undefined);
  const readWaylandClipboardText = vi.fn().mockResolvedValue("old remote text");
  const usesWaylandClipboard = vi.fn().mockReturnValue(false);
  terminal["_workspace"] = { sendInput } as never;
  terminal["_yasConn"] = {
    transport: { status: "connected" },
    sendClipboard,
    readWaylandClipboardText,
    usesWaylandClipboard,
  } as never;
  vi.spyOn(terminal, "focus").mockImplementation(() => input.focus());
  vi.mocked(terminalSurfaceForInput).mockReturnValue(terminal);
  const read = vi.fn().mockResolvedValue([]);
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    value: { read },
  });
  dispose = render(
    () => (
      <MobileToolbar
        keyboardTarget={() => input}
        clipboardConnection={() => null}
        theme={themeFor(PALETTES[0])}
        scale={uiScale(14)}
      />
    ),
    host,
  );
  const paste = host.querySelector<HTMLButtonElement>(
    `button[title="${t("keyboard.pasteClipboard")}"]`,
  )!;
  vi.spyOn(paste, "getBoundingClientRect").mockReturnValue(
    new DOMRect(0, 0, 80, 40),
  );
  return {
    paste,
    input,
    read,
    sendInput,
    sendClipboard,
    readWaylandClipboardText,
    usesWaylandClipboard,
  };
}

describe("mobile toolbar Paste", () => {
  it.each([false, true])(
    "pastes the device screenshot ahead of text (remote clipboard owned: %s)",
    async (remoteOwned) => {
      const {
        paste,
        read,
        sendInput,
        sendClipboard,
        readWaylandClipboardText,
        usesWaylandClipboard,
      } = mount();
      usesWaylandClipboard.mockReturnValue(remoteOwned);
      const bytes = new Uint8Array([137, 80, 78, 71]);
      const getType = vi.fn(
        async (mime: string) =>
          new Blob([mime === "image/png" ? bytes : "old clipboard text"], {
            type: mime,
          }),
      );
      read.mockResolvedValue([{ types: ["text/plain", "image/png"], getType }]);

      touch(paste, "touchstart");
      touch(paste, "touchend");

      await vi.waitFor(() => expect(sendInput).toHaveBeenCalledOnce());
      expect(readWaylandClipboardText).not.toHaveBeenCalled();
      expect(read).toHaveBeenCalledOnce();
      expect(getType).toHaveBeenCalledExactlyOnceWith("image/png");
      expect(sendClipboard).toHaveBeenCalledWith("image/png", bytes);
      expect(Array.from(sendInput.mock.calls[0][1] as Uint8Array)).toEqual([
        0x16,
      ]);
    },
  );

  it.each(["read", "transfer"])(
    "does not substitute old text when the screenshot %s fails",
    async (failure) => {
      const {
        paste,
        read,
        sendInput,
        sendClipboard,
        readWaylandClipboardText,
        usesWaylandClipboard,
      } = mount();
      usesWaylandClipboard.mockReturnValue(true);
      const getType = vi.fn(
        async () => new Blob([new Uint8Array([137, 80, 78, 71])]),
      );
      if (failure === "read")
        read.mockRejectedValue(new Error("Clipboard denied"));
      else {
        read.mockResolvedValue([
          { types: ["image/png", "text/plain"], getType },
        ]);
        sendClipboard.mockRejectedValue(new Error("Transfer failed"));
      }

      touch(paste, "touchstart");
      touch(paste, "touchend");
      await new Promise((resolve) => setTimeout(resolve, 0));

      expect(read).toHaveBeenCalledOnce();
      expect(readWaylandClipboardText).not.toHaveBeenCalled();
      expect(sendInput).not.toHaveBeenCalled();
      if (failure === "transfer")
        expect(getType).toHaveBeenCalledExactlyOnceWith("image/png");
    },
  );

  it("delivers a screenshot from touchend without waiting for a compatibility click", async () => {
    const { paste, input, read, sendInput, sendClipboard } = mount();
    const bytes = new Uint8Array([137, 80, 78, 71]);
    let inTouchEnd = false;
    read.mockImplementation(() => {
      if (!inTouchEnd)
        return Promise.reject(
          new DOMException("Tap required", "NotAllowedError"),
        );
      return Promise.resolve([
        {
          types: ["image/png"],
          getType: async () => new Blob([bytes], { type: "image/png" }),
        },
      ]);
    });
    let commit!: () => void;
    sendClipboard.mockReturnValue(
      new Promise<void>((resolve) => {
        commit = resolve;
      }),
    );

    expect(touch(paste, "touchstart").defaultPrevented).toBe(true);
    expect(read).not.toHaveBeenCalled();
    inTouchEnd = true;
    touch(paste, "touchend");
    inTouchEnd = false;
    expect(read).toHaveBeenCalledOnce();
    expect(document.activeElement).toBe(input);
    await vi.waitFor(() =>
      expect(sendClipboard).toHaveBeenCalledWith("image/png", bytes),
    );
    expect(sendInput).not.toHaveBeenCalled();

    commit();
    await vi.waitFor(() => expect(sendInput).toHaveBeenCalledOnce());
    expect(Array.from(sendInput.mock.calls[0][1] as Uint8Array)).toEqual([
      0x16,
    ]);
    paste.dispatchEvent(
      new MouseEvent("click", {
        bubbles: true,
        cancelable: true,
        detail: 1,
        clientX: 20,
        clientY: 20,
      }),
    );
    expect(read).toHaveBeenCalledOnce();
  });

  it.each(["touchcancel", "touchmove"])("does not paste after %s", (type) => {
    const { paste, read } = mount();
    touch(paste, "touchstart");
    touch(paste, type, 70);
    touch(paste, "touchend");
    expect(read).not.toHaveBeenCalled();
  });
});
