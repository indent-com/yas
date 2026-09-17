import {
  createContext,
  createEffect,
  createRenderEffect,
  createRoot,
  getOwner,
  mergeProps,
  onCleanup,
  untrack,
  useContext,
  type JSX,
} from "solid-js";
import { createStore } from "solid-js/store";
import type { LayoutLeaf } from "@yas-run/core/layout";
import { autoFocusPaneTarget, paneKeyboardTarget } from "./treeContext";

export interface PaneContentProps {
  paneId: string;
  leaf: LayoutLeaf;
  sessionId: string | null;
  isFocused: boolean;
  visible: boolean;
  surfaceSizingVisible: boolean;
}

interface MountedPane {
  element: HTMLDivElement;
  update: (props: PaneContentProps) => void;
  dispose: () => void;
}

const PaneMountContext =
  createContext<(props: PaneContentProps) => MountedPane>();

/** Own content by leaf, independently of the recursive layout's DOM owners.
 * Wrapping a leaf in a split, collapsing an ancestor, or switching container
 * kinds only moves its DOM. The canvas, decoder subscription, and input state
 * live until the leaf actually leaves the layout.
 */
export function PersistentPanes(props: {
  leaves: readonly LayoutLeaf[];
  render: (props: PaneContentProps) => JSX.Element;
  children: JSX.Element;
}) {
  const owner = getOwner();
  const panes = new Map<LayoutLeaf, MountedPane>();
  const mount = (initial: PaneContentProps): MountedPane => {
    const { leaf, ...initialState } = initial;
    let pane = panes.get(leaf);
    if (!pane) {
      pane = createRoot((dispose) => {
        // Track fields independently: a path change must not reassert focus
        // or restart a resize driver whose boolean inputs stayed the same.
        // Saved workspace leaves are frozen. A store clones frozen objects
        // and unwraps proxies, so keep the identity key outside the store.
        const [state, setState] = createStore(initialState);
        const current = mergeProps(state, { leaf });
        const update = ({ leaf: _leaf, ...next }: PaneContentProps) =>
          setState(next);
        const element = (
          <div style={{ width: "100%", height: "100%" }}>
            {props.render(current)}
          </div>
        ) as HTMLDivElement;
        return { element, update, dispose };
      }, owner);
      panes.set(leaf, pane);
    }
    return pane;
  };

  createEffect(() => {
    const retained = new Set(props.leaves);
    for (const [leaf, pane] of panes) {
      if (retained.has(leaf)) continue;
      panes.delete(leaf);
      pane.dispose();
      pane.element.remove();
    }
  });
  onCleanup(() => {
    for (const pane of panes.values()) pane.dispose();
    panes.clear();
  });

  return (
    <PaneMountContext.Provider value={mount}>
      {props.children}
    </PaneMountContext.Provider>
  );
}

/** A disposable structural slot; removing it must not dispose its content. */
export function PaneSlot(props: PaneContentProps) {
  const mount = useContext(PaneMountContext)!;
  let disposed = false;
  onCleanup(() => {
    disposed = true;
  });
  const slot = (
    <div style={{ width: "100%", height: "100%" }} />
  ) as HTMLDivElement;
  createRenderEffect(() => {
    const current = { ...props };
    const pane = untrack(() => mount(current));
    pane.update(current);
    // Keep DOM insertion outside Solid's child reconciliation: the old slot
    // can be disposed after the new one has adopted this same element.
    if (pane.element.parentNode !== slot) {
      const active = pane.element.ownerDocument.activeElement;
      const focused =
        active instanceof HTMLElement && pane.element.contains(active)
          ? active
          : null;
      slot.appendChild(pane.element);
      // Reparenting can drop DOM focus even though the logical focus boolean
      // did not change. Wait for the new slot to enter the document; a newer
      // placement or an intervening focus in app chrome must win.
      queueMicrotask(() => {
        if (disposed || !slot.isConnected || pane.element.parentNode !== slot)
          return;
        autoFocusPaneTarget(
          () => props.isFocused,
          () =>
            focused?.isConnected ? focused : paneKeyboardTarget(pane.element),
          pane.element.ownerDocument,
        );
      });
    }
  });
  return slot;
}
