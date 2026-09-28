import { afterEach, describe, expect, it, vi } from "vitest";
import { observeWorkspaceViewport } from "../workspaceViewport";

let stop: (() => void) | undefined;
afterEach(() => {
  stop?.();
  stop = undefined;
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

function setup({ height = 700, cssHeight = 700, innerHeight = 874 } = {}) {
  const viewport = Object.assign(new EventTarget(), {
    width: 402,
    height,
    offsetTop: 0,
  });
  vi.stubGlobal("visualViewport", viewport);
  vi.stubGlobal("innerHeight", innerHeight);
  let shellHeight = cssHeight;
  vi.spyOn(
    document.documentElement,
    "getBoundingClientRect",
  ).mockImplementation(() => new DOMRect(0, 0, viewport.width, shellHeight));
  const changed = vi.fn();
  stop = observeWorkspaceViewport(changed);
  return {
    viewport,
    changed,
    resize(height: number, cssHeight: number, width = viewport.width) {
      viewport.height = height;
      viewport.width = width;
      shellHeight = cssHeight;
      viewport.dispatchEvent(new Event("resize"));
    },
    occlusion() {
      const state = changed.mock.lastCall![0];
      return state.baselineHeight - state.height;
    },
  };
}

describe("workspace viewport", () => {
  it("does not count the native PWA safe area as keyboard occlusion", () => {
    vi.stubGlobal("navigator", { standalone: true });
    const view = setup({ height: 812, cssHeight: 812, innerHeight: 874 });
    expect(view.occlusion()).toBe(0);
    view.resize(480, 812);
    expect(view.occlusion()).toBe(332);
    view.resize(812, 812);
    expect(view.occlusion()).toBe(0);
  });

  it("does not treat Safari's expanded or collapsed bars as a keyboard", () => {
    const view = setup();
    expect(view.occlusion()).toBe(0);
    view.resize(790, 790);
    expect(view.occlusion()).toBe(0);
    view.resize(700, 700);
    expect(view.occlusion()).toBe(0);
  });

  it.each([700, 390])(
    "tracks keyboard opening, panning and dismissal with a %ipx shell",
    (keyboardShellHeight) => {
      const view = setup();
      view.resize(390, keyboardShellHeight);
      expect(view.occlusion()).toBe(310);
      view.viewport.offsetTop = 96;
      view.viewport.dispatchEvent(new Event("scroll"));
      expect(view.changed.mock.lastCall![0].offsetTop).toBe(96);
      view.viewport.offsetTop = 0;
      view.resize(700, 700);
      expect(view.occlusion()).toBe(0);
    },
  );

  it("still detects a hardware-keyboard accessory bar", () => {
    const view = setup();
    view.resize(645, 700);
    expect(view.occlusion()).toBe(55);
  });

  it("resets the portrait baseline on rotation", () => {
    const view = setup();
    view.resize(340, 340, 874);
    expect(view.occlusion()).toBe(0);
  });

  it("stops observing when the workspace unmounts", () => {
    const view = setup();
    stop!();
    view.changed.mockClear();
    view.resize(390, 700);
    window.dispatchEvent(new Event("resize"));
    expect(view.changed).not.toHaveBeenCalled();
  });
});
