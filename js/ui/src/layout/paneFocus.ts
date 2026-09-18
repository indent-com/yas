import { untrack } from "solid-js";

const PANE_FOCUS_OWNER_SELECTOR =
  "[data-yas-pane-id], [data-yas-workspace-focus-owner]";

/** Prefer an editable CodeMirror body over its earlier, focusable scroller.
 * Bare terminal canvases have no tabindex; their input target lives beside
 * them. Read-only editors fall through to their focusable wrapper.
 */
export function paneKeyboardTarget(container: HTMLElement): HTMLElement | null {
  return (
    container.querySelector<HTMLElement>(
      '.cm-content[contenteditable="true"]',
    ) ?? container.querySelector<HTMLElement>("[tabindex], input, textarea")
  );
}

/**
 * Whether a focused pane may move DOM focus to its own keyboard target.
 *
 * Focus on the document body is unowned, and focus in another pane is a pane
 * handoff. Persistent web panes are marked as workspace focus owners because
 * their iframe is portaled outside the logical pane. A control outside those
 * content roots (status bar, overlay, dock chrome, …) owns focus explicitly
 * and must not have it stolen by a reactive pane update.
 */
export function canAutoFocusPane(
  active: Element | null,
  body: HTMLElement,
): boolean {
  return (
    active === null ||
    active === body ||
    active.matches("[data-yas-keyboard-fallback]") ||
    active.closest(PANE_FOCUS_OWNER_SELECTOR) !== null
  );
}

/** A delayed keyboard keep-alive must not reclaim a pane the user left. */
export function canRestorePaneKeyboardFocus(source: HTMLElement): boolean {
  if (!source.isConnected) return false;
  const pane = source.closest("[data-yas-pane-id]");
  return !pane || pane.getAttribute("data-yas-pane-focused") === "true";
}

/**
 * Focus a pane's keyboard target without stealing focus from app chrome.
 *
 * Some targets are attached by a child onMount after the owning pane's effect
 * runs. Retry that case once, rechecking both reactive pane ownership and DOM
 * focus ownership so an intervening overlay/control focus always wins.
 */
export function autoFocusPaneTarget(
  isFocused: () => boolean,
  findTarget: () => HTMLElement | null,
  ownerDocument: Document = document,
): void {
  const canFocus = () =>
    isFocused() &&
    canAutoFocusPane(ownerDocument.activeElement, ownerDocument.body);
  if (!canFocus()) return;

  const focus = (): boolean => {
    const target = findTarget();
    if (!target?.isConnected) return false;
    const active = ownerDocument.activeElement;
    const owner = target.closest(PANE_FOCUS_OWNER_SELECTOR);
    // A pane may contain an editor, search box, rename field, or toolbar.
    // Updating that pane does not authorize a handoff between its controls.
    if (
      active === target ||
      (owner && active?.closest(PANE_FOCUS_OWNER_SELECTOR) === owner)
    )
      return true;
    // Native focus dispatches application handlers synchronously. Their
    // signal reads must not become dependencies of the calling pane effect.
    untrack(() => target.focus({ preventScroll: true }));
    return true;
  };

  if (focus()) return;
  queueMicrotask(() => {
    if (canFocus()) focus();
  });
}
