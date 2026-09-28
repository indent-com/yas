import type { TerminalPalette } from "@yas-run/core";
import { themeFor } from "./theme";

const SYSTEM_BAR_BACKGROUND = "--yas-system-bar-background";

export function isStandaloneApp(): boolean {
  // iOS can report navigator.standalone while the display-mode query is false.
  return (
    (navigator as Navigator & { standalone?: boolean }).standalone === true ||
    window.matchMedia?.("(display-mode: standalone)").matches === true
  );
}

/** Keep browser/installed-app chrome in the same polarity and colour as the
 * top workspace tab bar. Safari 26 samples the fixed safe-area backdrop rather
 * than `theme-color`; older browsers still use the meta tag. */
export function applySystemChrome(palette: TerminalPalette): void {
  const colorScheme = palette.dark ? "dark" : "light";
  const background = themeFor(palette).solidPanelBg;
  const root = document.documentElement;

  const standalone = isStandaloneApp();
  root.toggleAttribute("data-yas-standalone", standalone);
  // In an iOS Home Screen app, cover lets the native scroll-edge effect
  // overlap controls immediately below the status bar. Let WebKit own the
  // safe edges instead of reserving an extra blank anti-blur strip in the UI.
  const ios =
    (navigator as Navigator & { standalone?: boolean }).standalone === true ||
    (typeof CSS !== "undefined" &&
      CSS.supports("-webkit-touch-callout", "none"));
  let contained = false;
  if (standalone && ios) {
    const viewport = document.querySelector<HTMLMetaElement>(
      'meta[name="viewport"]',
    );
    if (viewport) {
      const content = viewport.content.replace(
        /viewport-fit\s*=\s*cover/i,
        "viewport-fit=contain",
      );
      if (content !== viewport.content) viewport.content = content;
      contained = /viewport-fit\s*=\s*contain/i.test(viewport.content);
    }
  }
  root.toggleAttribute("data-yas-contained-viewport", contained);
  root.dataset.theme = colorScheme;
  root.style.colorScheme = colorScheme;
  root.style.setProperty(SYSTEM_BAR_BACKGROUND, background);
  document.body.style.backgroundColor = background;
  document
    .querySelector<HTMLMetaElement>('meta[name="theme-color"]')
    ?.setAttribute("content", background);
}
