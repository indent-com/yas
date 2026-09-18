import type { LayoutLeaf } from "@yas-run/core/layout";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it } from "vitest";
import { PaneSlot, PersistentPanes } from "../layout/PersistentPanes";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.replaceChildren();
});

it.each(["visible", "hidden", "hidden before placement settles"])(
  "restores a newly placed pane only while visible (%s)",
  async (state) => {
    const leaf: LayoutLeaf = { type: "leaf" };
    const [visible, setVisible] = createSignal(state !== "hidden");
    dispose = render(
      () => (
        <PersistentPanes
          leaves={[leaf]}
          render={() => (
            <div data-yas-pane-id="0">
              <textarea />
            </div>
          )}
        >
          <PaneSlot
            leaf={leaf}
            paneId="0"
            sessionId="terminal"
            isFocused
            visible={visible()}
            surfaceSizingVisible={visible()}
          />
        </PersistentPanes>
      ),
      document.body,
    );
    if (state === "hidden before placement settles") setVisible(false);
    await Promise.resolve();

    expect(document.activeElement).toBe(
      state === "visible" ? document.querySelector("textarea") : document.body,
    );
  },
);
