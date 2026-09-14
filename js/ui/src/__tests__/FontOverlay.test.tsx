import { PALETTES } from "@yas-run/core";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FontOverlay } from "../FontOverlay";
import { t } from "../i18n";

let dispose: (() => void) | undefined;
const scrollDescriptor = Object.getOwnPropertyDescriptor(
  HTMLElement.prototype,
  "scrollIntoView",
);
beforeEach(() => {
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
    configurable: true,
    value: vi.fn(),
  });
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue(
    new DOMRect(0, 0, 200, 40),
  );
});
afterEach(() => {
  dispose?.();
  document.body.replaceChildren();
  vi.restoreAllMocks();
  if (scrollDescriptor) {
    Object.defineProperty(
      HTMLElement.prototype,
      "scrollIntoView",
      scrollDescriptor,
    );
  } else {
    Reflect.deleteProperty(HTMLElement.prototype, "scrollIntoView");
  }
});

function mount(curated = false) {
  const [serverFonts, setServerFonts] = createSignal([
    "First Mono",
    "Second Mono",
  ]);
  const preview = vi.fn();
  const select = vi.fn();
  const close = vi.fn();
  dispose = render(
    () => (
      <FontOverlay
        currentFamily="First Mono"
        currentSize={14}
        currentGamma={1}
        serverFonts={serverFonts()}
        fontChoices={
          curated
            ? [
                { label: "First Mono", stack: "First Mono" },
                { label: "Second Mono", stack: '"Second Mono", monospace' },
              ]
            : undefined
        }
        palette={PALETTES[0]}
        fontSize={13}
        onPreview={preview}
        onSelect={select}
        onClose={close}
      />
    ),
    document.body,
  );
  return { preview, select, close, setServerFonts };
}

function touch(element: HTMLElement, type: string, x = 20) {
  const point = { identifier: 1, clientX: x, clientY: 20 };
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperties(event, {
    touches: { value: type === "touchend" ? [] : [point] },
    changedTouches: { value: [point] },
  });
  element.dispatchEvent(event);
  return event;
}

function secondFont() {
  return document.querySelectorAll<HTMLButtonElement>("ul button")[1];
}

describe("Font panel touch selection", () => {
  it.each([
    { fonts: [] },
    { fonts: ["First Mono", "Second Mono"] },
    { fonts: ["Second Mono", "First Mono"] },
    { fonts: ["First Mono"] },
  ])(
    "keeps the choice across a disconnect and catalogue refresh: $fonts",
    ({ fonts }) => {
      const { preview, select, setServerFonts } = mount();
      secondFont().click();
      expect(preview).toHaveBeenLastCalledWith("Second Mono", 14, 1);

      setServerFonts([]);
      setServerFonts(fonts);
      const selected = document.querySelector('ul button[aria-pressed="true"]');
      expect(selected?.textContent ?? null).toBe(
        fonts.includes("Second Mono") ? "Second Mono" : null,
      );

      document
        .querySelector("form")!
        .dispatchEvent(
          new Event("submit", { bubbles: true, cancelable: true }),
        );
      expect(select).toHaveBeenCalledExactlyOnceWith("Second Mono", 14, 1);
    },
  );

  it("applies a typed family after replacing a list selection", () => {
    const { select, setServerFonts } = mount();
    secondFont().click();
    const input = document.querySelector<HTMLInputElement>(
      'input[name="yas-font-search"]',
    )!;
    input.value = "Custom Mono";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    setServerFonts([]);
    setServerFonts(["First Mono", "Second Mono", "Custom Mono"]);

    document
      .querySelector("form")!
      .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    expect(select).toHaveBeenCalledExactlyOnceWith("Custom Mono", 14, 1);
  });

  it.each([false, true])(
    "previews a tap once, then applies it (curated=%s)",
    (curated) => {
      const { preview, select, close } = mount(curated);
      const font = secondFont();
      const stack = curated ? '"Second Mono", monospace' : "Second Mono";
      const focused = document.activeElement;
      touch(font, "touchstart");
      touch(font, "touchend");
      font.dispatchEvent(
        new MouseEvent("click", {
          bubbles: true,
          cancelable: true,
          detail: 1,
          clientX: 20,
          clientY: 20,
        }),
      );
      expect(preview).toHaveBeenCalledExactlyOnceWith(stack, 14, 1);
      expect(font.getAttribute("aria-pressed")).toBe("true");
      expect(document.activeElement).toBe(focused);
      expect(select).not.toHaveBeenCalled();
      expect(close).not.toHaveBeenCalled();

      const apply = [...document.querySelectorAll("button")].find(
        (button) => button.textContent === t("font.apply"),
      )!;
      touch(apply, "touchstart");
      touch(apply, "touchend");
      expect(select).toHaveBeenCalledExactlyOnceWith(stack, 14, 1);
    },
  );

  it("scrolls the list without selecting a font on swipe", () => {
    const { preview, select } = mount();
    const list = document.querySelector("ul")!;
    list.style.overflowY = "auto";
    Object.defineProperties(list, {
      clientHeight: { value: 80 },
      scrollHeight: { value: 400 },
    });
    const font = secondFont();
    expect(touch(font, "touchstart").defaultPrevented).toBe(false);
    expect(touch(font, "touchmove", 60).defaultPrevented).toBe(false);
    touch(font, "touchend");
    expect(preview).not.toHaveBeenCalled();
    expect(select).not.toHaveBeenCalled();
  });
});
