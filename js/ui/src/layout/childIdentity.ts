import { enumeratePanes } from "@yas-run/core/layout";
import type { LayoutChild, LayoutLeaf } from "@yas-run/core/layout";

export interface IdentifiedLayoutChild {
  key: object;
  child: LayoutChild;
  leaves: ReadonlySet<LayoutLeaf>;
}

/** Preserve DOM owners across insertion and immutable edits to split branches.
 * Leaf objects survive layout mutations; child wrappers and split objects do
 * not. Match unchanged nodes first, then changed branches by surviving leaves.
 */
export function identifyLayoutChildren(
  previous: readonly IdentifiedLayoutChild[],
  children: readonly LayoutChild[],
): IdentifiedLayoutChild[] {
  const available = new Set(previous);
  const matches = children.map((child) => {
    const match = previous.find((entry) => entry.child.node === child.node);
    if (match) available.delete(match);
    return match;
  });
  return children.map((child, index) => {
    const leaves = new Set(enumeratePanes(child.node).map((pane) => pane.leaf));
    let match = matches[index];
    if (!match) {
      let overlap = 0;
      for (const candidate of available) {
        let shared = 0;
        for (const leaf of leaves) if (candidate.leaves.has(leaf)) shared++;
        if (shared > overlap) {
          match = candidate;
          overlap = shared;
        }
      }
      if (match) available.delete(match);
    }
    return { key: match?.key ?? {}, child, leaves };
  });
}
