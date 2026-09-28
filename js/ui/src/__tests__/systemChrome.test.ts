import { PALETTES } from "@yas-run/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { applySystemChrome, isStandaloneApp } from "../systemChrome";
import { themeFor } from "../theme";

const palette = (id: string) => PALETTES.find((entry) => entry.id === id)!;

beforeEach(() => {
  document.head.innerHTML = '<meta name="theme-color" content="#000">';
});

afterEach(() => {
  document.documentElement.removeAttribute("data-theme");
  document.documentElement.removeAttribute("data-yas-standalone");
  document.documentElement.removeAttribute("data-yas-contained-viewport");
  document.documentElement.removeAttribute("style");
  document.body.removeAttribute("style");
  document.head.innerHTML = "";
  vi.unstubAllGlobals();
});

describe("system chrome", () => {
  it("recognizes an iOS Home Screen app even when display-mode does not", () => {
    vi.stubGlobal("navigator", { standalone: true });
    vi.stubGlobal("matchMedia", () => ({ matches: false }));
    applySystemChrome(PALETTES[0]);
    expect(isStandaloneApp()).toBe(true);
    expect(document.documentElement.hasAttribute("data-yas-standalone")).toBe(
      true,
    );
  });

  it("uses the native safe viewport for an iOS Home Screen app", () => {
    document.head.innerHTML +=
      '<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover, interactive-widget=resizes-content">';
    vi.stubGlobal("navigator", { standalone: true });
    vi.stubGlobal("matchMedia", () => ({ matches: false }));
    applySystemChrome(PALETTES[0]);
    const viewport = document.querySelector<HTMLMetaElement>(
      'meta[name="viewport"]',
    )!;
    expect(viewport.content).toBe(
      "width=device-width, initial-scale=1, viewport-fit=contain, interactive-widget=resizes-content",
    );
    expect(
      document.documentElement.hasAttribute("data-yas-contained-viewport"),
    ).toBe(true);
    // Palette updates must not keep changing the viewport and trigger reflows.
    const original = viewport.content;
    applySystemChrome(palette("catppuccin-latte"));
    expect(viewport.content).toBe(original);
  });

  it("recognizes display-mode and clears the marker in a browser tab", () => {
    vi.stubGlobal("navigator", { standalone: false });
    vi.stubGlobal("matchMedia", () => ({ matches: true }));
    applySystemChrome(PALETTES[0]);
    expect(document.documentElement.hasAttribute("data-yas-standalone")).toBe(
      true,
    );
    vi.stubGlobal("matchMedia", () => ({ matches: false }));
    applySystemChrome(PALETTES[0]);
    expect(document.documentElement.hasAttribute("data-yas-standalone")).toBe(
      false,
    );
  });

  it.each(["default", "catppuccin-latte"])("follows the %s palette", (id) => {
    const selected = palette(id);
    const background = themeFor(selected).solidPanelBg;

    applySystemChrome(selected);

    expect(document.documentElement.dataset.theme).toBe(
      selected.dark ? "dark" : "light",
    );
    expect(document.documentElement.style.colorScheme).toBe(
      selected.dark ? "dark" : "light",
    );
    expect(
      document.documentElement.style.getPropertyValue(
        "--yas-system-bar-background",
      ),
    ).toBe(background);
    expect(document.body.style.backgroundColor).toBe(background);
    expect(
      document.querySelector<HTMLMetaElement>('meta[name="theme-color"]')
        ?.content,
    ).toBe(background);
  });
});
