import type {
  LayoutChild,
  LayoutDirection,
  LayoutLeaf,
  LayoutNode,
  LayoutSplit,
} from "@yas-run/core/layout";
import { enumeratePanes } from "@yas-run/core/layout";
import { removePaneFromLayout } from "./paneRemoval";
import type { SpatialDirection } from "./spatialNavigation";

export type TiledLayout = "horizontal" | "vertical" | "tabs" | "stacking";

export interface LayoutMutation {
  root: LayoutNode;
  assignments: Record<string, string | null>;
  focusedPaneId: string;
  /** Every old pane id that survived, mapped to its id in the new tree. */
  paneIdMap: ReadonlyMap<string, string>;
}

function pathForPane(root: LayoutNode, paneId: string): number[] | null {
  if (root.type === "leaf") return paneId === "0" ? [] : null;
  const path = paneId.split(".").map(Number);
  if (path.some((index) => !Number.isInteger(index) || index < 0)) return null;
  let current: LayoutNode = root;
  for (const index of path) {
    if (current.type !== "split" || !current.children[index]) return null;
    current = current.children[index].node;
  }
  return current.type === "leaf" ? path : null;
}

function pathForLeaf(
  root: LayoutNode,
  leaf: LayoutLeaf,
  path: readonly number[] = [],
): number[] | null {
  if (root.type === "leaf") return root === leaf ? [...path] : null;
  for (let index = 0; index < root.children.length; index += 1) {
    const found = pathForLeaf(root.children[index].node, leaf, [
      ...path,
      index,
    ]);
    if (found) return found;
  }
  return null;
}

function nodeAtPath(
  root: LayoutNode,
  path: readonly number[],
): LayoutNode | null {
  let current = root;
  for (const index of path) {
    if (current.type !== "split" || !current.children[index]) return null;
    current = current.children[index].node;
  }
  return current;
}

function replaceAtPath(
  root: LayoutNode,
  path: readonly number[],
  replacement: LayoutNode,
): LayoutNode {
  if (path.length === 0) return replacement;
  if (root.type !== "split") return root;
  const [head, ...rest] = path;
  return {
    ...root,
    children: root.children.map((child, index) =>
      index === head
        ? { ...child, node: replaceAtPath(child.node, rest, replacement) }
        : child,
    ),
  };
}

function paneId(path: readonly number[]): string {
  return path.length === 0 ? "0" : path.join(".");
}

function assignmentsByLeaf(
  root: LayoutNode,
  assignments: Readonly<Record<string, string | null>>,
): Map<LayoutLeaf, string | null> {
  return new Map(
    enumeratePanes(root).map(({ id, leaf }) => [leaf, assignments[id] ?? null]),
  );
}

/**
 * Re-key assignments after an immutable tree transformation. Pane ids are
 * paths and therefore disposable; leaf identity is the stable bridge across
 * the edit. New leaves may supply their initial assignment in `newValues`.
 */
function finishMutation(
  oldRoot: LayoutNode,
  nextRoot: LayoutNode,
  oldAssignments: Readonly<Record<string, string | null>>,
  focusedLeaf: LayoutLeaf,
  newValues: ReadonlyMap<LayoutLeaf, string | null> = new Map(),
): LayoutMutation | null {
  const oldPanes = enumeratePanes(oldRoot);
  const oldIds = new Map(oldPanes.map(({ id, leaf }) => [leaf, id]));
  const oldValues = assignmentsByLeaf(oldRoot, oldAssignments);
  const assignments: Record<string, string | null> = {};
  const paneIdMap = new Map<string, string>();
  let focusedPaneId: string | null = null;

  for (const { id, leaf } of enumeratePanes(nextRoot)) {
    assignments[id] = newValues.has(leaf)
      ? (newValues.get(leaf) ?? null)
      : (oldValues.get(leaf) ?? null);
    if (leaf === focusedLeaf) focusedPaneId = id;
    const oldId = oldIds.get(leaf);
    if (oldId) paneIdMap.set(oldId, id);
  }

  return focusedPaneId
    ? { root: nextRoot, assignments, focusedPaneId, paneIdMap }
    : null;
}

function isTiledSplit(node: LayoutNode | null): node is LayoutSplit {
  return node?.type === "split" && node.direction !== "workspace";
}

/**
 * Put a new populated container after `targetPaneId`.
 *
 * Matching parent splits are extended instead of producing the staircase of
 * redundant two-child wrappers that a naive BSP insertion creates. A
 * different parent orientation is preserved by nesting at the focused leaf,
 * exactly like sway's split containers.
 */
export function splitPaneWithAssignment(
  root: LayoutNode,
  assignments: Readonly<Record<string, string | null>>,
  targetPaneId: string,
  value: string,
  direction: TiledLayout,
  placeAfter = true,
): LayoutMutation | null {
  if (Object.values(assignments).includes(value)) {
    return null;
  }
  const path = pathForPane(root, targetPaneId);
  if (!path) return null;
  const target = nodeAtPath(root, path);
  if (target?.type !== "leaf") return null;

  const inserted: LayoutLeaf = { type: "leaf" };
  let nextRoot: LayoutNode;
  if (path.length > 0) {
    const parentPath = path.slice(0, -1);
    const parent = nodeAtPath(root, parentPath);
    if (parent?.type === "split" && parent.direction === direction) {
      const at = path[path.length - 1] + (placeAfter ? 1 : 0);
      const children = [...parent.children];
      children.splice(at, 0, { node: inserted, weight: 1 });
      nextRoot = replaceAtPath(root, parentPath, { ...parent, children });
    } else {
      const children: [LayoutChild, LayoutChild] = placeAfter
        ? [
            { node: target, weight: 1 },
            { node: inserted, weight: 1 },
          ]
        : [
            { node: inserted, weight: 1 },
            { node: target, weight: 1 },
          ];
      nextRoot = replaceAtPath(root, path, {
        type: "split",
        direction,
        children,
      });
    }
  } else {
    const children: [LayoutChild, LayoutChild] = placeAfter
      ? [
          { node: target, weight: 1 },
          { node: inserted, weight: 1 },
        ]
      : [
          { node: inserted, weight: 1 },
          { node: target, weight: 1 },
        ];
    nextRoot = {
      type: "split",
      direction,
      children,
    };
  }
  return finishMutation(
    root,
    nextRoot,
    assignments,
    inserted,
    new Map([[inserted, value]]),
  );
}

/** Change the focused container's current child layout. */
export function setPaneLayout(
  root: LayoutNode,
  assignments: Readonly<Record<string, string | null>>,
  focusedPaneId: string,
  direction: TiledLayout,
): LayoutMutation | null {
  const path = pathForPane(root, focusedPaneId);
  if (!path || path.length === 0) return null;
  const leaf = nodeAtPath(root, path);
  const parentPath = path.slice(0, -1);
  const parent = nodeAtPath(root, parentPath);
  if (leaf?.type !== "leaf" || !isTiledSplit(parent)) return null;
  if (parent.direction === direction) return null;
  return finishMutation(
    root,
    replaceAtPath(root, parentPath, { ...parent, direction }),
    assignments,
    leaf,
  );
}

/** Toggle the focused container between horizontal and vertical splitting. */
export function togglePaneSplit(
  root: LayoutNode,
  assignments: Readonly<Record<string, string | null>>,
  focusedPaneId: string,
): LayoutMutation | null {
  const path = pathForPane(root, focusedPaneId);
  if (!path || path.length === 0) return null;
  const parent = nodeAtPath(root, path.slice(0, -1));
  const direction =
    parent?.type === "split" && parent.direction === "horizontal"
      ? "vertical"
      : "horizontal";
  return setPaneLayout(root, assignments, focusedPaneId, direction);
}

function axisFor(direction: SpatialDirection): "horizontal" | "vertical" {
  return direction === "left" || direction === "right"
    ? "horizontal"
    : "vertical";
}

function isLeading(direction: SpatialDirection): boolean {
  return direction === "left" || direction === "up";
}

/** Insert at a specific content edge, dividing only the target's allocation. */
function insertChildBeside(
  root: LayoutNode,
  targetPath: readonly number[],
  source: LayoutChild,
  direction: SpatialDirection,
): LayoutNode {
  const target = nodeAtPath(root, targetPath)!;
  const axis = axisFor(direction);
  const leading = isLeading(direction);
  if (targetPath.length > 0) {
    const parentPath = targetPath.slice(0, -1);
    const parent = nodeAtPath(root, parentPath);
    if (parent?.type === "split" && parent.direction === axis) {
      const children = [...parent.children];
      const index = targetPath[targetPath.length - 1];
      const weight = children[index].weight / 2;
      const remaining = { ...children[index], weight };
      const inserted = { ...source, weight };
      children.splice(
        index,
        1,
        ...(leading ? [inserted, remaining] : [remaining, inserted]),
      );
      return replaceAtPath(root, parentPath, { ...parent, children });
    }
  }
  const inserted = { ...source, weight: 1 };
  const remaining = { node: target, weight: 1 };
  return replaceAtPath(root, targetPath, {
    type: "split",
    direction: axis,
    children: leading ? [inserted, remaining] : [remaining, inserted],
  });
}

function containerAxis(
  direction: LayoutDirection,
): "horizontal" | "vertical" | null {
  if (direction === "workspace") return null;
  return direction === "horizontal" || direction === "tabs"
    ? "horizontal"
    : "vertical";
}

/** One keyboard step within a tiled tree; floating frames are never targets. */
function moveTiledLeaf(
  root: LayoutNode,
  path: readonly number[],
  direction: SpatialDirection,
): LayoutNode | null {
  if (path.length === 0) return null;
  const owner = nodeAtPath(root, path.slice(0, -1));
  if (!isTiledSplit(owner)) return null;
  const source = owner.children[path[path.length - 1]];
  const axis = axisFor(direction);
  const leading = isLeading(direction);

  for (let depth = path.length - 1; depth >= 0; depth -= 1) {
    const parentPath = path.slice(0, depth);
    const parent = nodeAtPath(root, parentPath);
    if (!isTiledSplit(parent)) return null;
    if (containerAxis(parent.direction) !== axis) continue;
    const index = path[depth];
    const children = [...parent.children];
    if (depth === path.length - 1) {
      const neighbor = index + (leading ? -1 : 1);
      if (!children[neighbor]) {
        // At the root boundary there is nothing to move past. In a nested
        // container, keep looking for an ancestor to extract into.
        if (depth === 0) return null;
        continue;
      }
      [children[index], children[neighbor]] = [
        children[neighbor],
        children[index],
      ];
    } else {
      const branch = children[index];
      const remaining = removePaneFromLayout(
        branch.node,
        paneId(path.slice(depth + 1)),
      );
      if (!remaining) return null;
      // Split this branch's allocation so unrelated siblings keep their size.
      const weight = branch.weight / 2;
      const kept = { ...branch, node: remaining, weight };
      const extracted = { ...source, weight };
      children.splice(
        index,
        1,
        ...(leading ? [extracted, kept] : [kept, extracted]),
      );
    }
    return replaceAtPath(root, parentPath, { ...parent, children });
  }

  // No ancestor has this axis. Make one split around the tiled root.
  const remaining = removePaneFromLayout(root, paneId(path));
  if (!remaining) return null;
  const extracted = { ...source, weight: 1 };
  const kept = { node: remaining, weight: 1 };
  return {
    type: "split",
    direction: axis,
    children: leading ? [extracted, kept] : [kept, extracted],
  };
}

/** Reorder a sibling, or extract out of a nested container by one boundary. */
export function movePaneInDirection(
  root: LayoutNode,
  assignments: Readonly<Record<string, string | null>>,
  sourcePaneId: string,
  direction: SpatialDirection,
): LayoutMutation | null {
  const path = pathForPane(root, sourcePaneId);
  if (!path || !assignments[sourcePaneId]) return null;
  const source = nodeAtPath(root, path);
  if (source?.type !== "leaf") return null;
  let next: LayoutNode | null;
  if (root.type === "split" && root.direction === "workspace") {
    const base = root.children[path[0]];
    if (!base || base.rect != null) return null;
    const moved = moveTiledLeaf(base.node, path.slice(1), direction);
    next = moved ? replaceAtPath(root, [path[0]], moved) : null;
  } else {
    next = moveTiledLeaf(root, path, direction);
  }
  return next ? finishMutation(root, next, assignments, source) : null;
}

/** Place a dragged view on the requested side of its explicit drop target. */
export function movePaneBeside(
  root: LayoutNode,
  assignments: Readonly<Record<string, string | null>>,
  sourcePaneId: string,
  targetPaneId: string,
  direction: SpatialDirection,
): LayoutMutation | null {
  const sourcePath = pathForPane(root, sourcePaneId);
  const targetPath = pathForPane(root, targetPaneId);
  if (!sourcePath || !targetPath || !assignments[sourcePaneId]) return null;
  const source = nodeAtPath(root, sourcePath);
  const target = nodeAtPath(root, targetPath);
  if (source?.type !== "leaf" || target?.type !== "leaf") return null;
  const parentPath = sourcePath.slice(0, -1);
  const parent = nodeAtPath(root, parentPath);
  if (parent?.type !== "split") return null;
  const sourceIndex = sourcePath[sourcePath.length - 1];
  // A drag out of a floating frame adopts the destination's placement.
  const { rect: _rect, ...sourceChild } = parent.children[sourceIndex];

  if (source === target) {
    if (parent.direction !== "tabs" && parent.direction !== "stacking")
      return null;
    const remaining = removePaneFromLayout(parent, String(sourceIndex));
    if (!remaining) return null;
    const next = insertChildBeside(
      replaceAtPath(root, parentPath, remaining),
      parentPath,
      sourceChild,
      direction,
    );
    return finishMutation(root, next, assignments, source);
  }

  if (
    parent.direction === axisFor(direction) &&
    parentPath.join(".") === targetPath.slice(0, -1).join(".")
  ) {
    const targetIndex = targetPath[targetPath.length - 1];
    const insertion = targetIndex + (isLeading(direction) ? 0 : 1);
    const destination = insertion - (sourceIndex < insertion ? 1 : 0);
    if (destination === sourceIndex) return null;
    const children = [...parent.children];
    const [moved] = children.splice(sourceIndex, 1);
    children.splice(destination, 0, moved);
    return finishMutation(
      root,
      replaceAtPath(root, parentPath, { ...parent, children }),
      assignments,
      source,
    );
  }

  const withoutSource = removePaneFromLayout(root, sourcePaneId);
  if (!withoutSource) return null;
  let anchor = pathForLeaf(withoutSource, target);
  if (!anchor) return null;
  const targetParent = nodeAtPath(withoutSource, anchor.slice(0, -1));
  if (
    targetParent?.type === "split" &&
    (targetParent.direction === "tabs" || targetParent.direction === "stacking")
  ) {
    anchor = anchor.slice(0, -1);
  }
  const next = insertChildBeside(withoutSource, anchor, sourceChild, direction);
  return finishMutation(root, next, assignments, source);
}

/** Layouts available when cycling a tiled container. */
export function nextTiledLayout(direction: LayoutDirection): TiledLayout {
  if (direction === "horizontal") return "vertical";
  if (direction === "vertical") return "tabs";
  if (direction === "tabs") return "stacking";
  return "horizontal";
}

export function paneParentLayout(
  root: LayoutNode,
  focusedPaneId: string,
): LayoutDirection | null {
  const path = pathForPane(root, focusedPaneId);
  if (!path || path.length === 0) return null;
  const parent = nodeAtPath(root, path.slice(0, -1));
  return parent?.type === "split" ? parent.direction : null;
}

export const _test = { pathForPane, pathForLeaf, nodeAtPath, paneId };
