import {
  enumeratePanes,
  validateLayoutNode,
  type LayoutChild,
  type LayoutLeaf,
  type LayoutNode,
  type LayoutSplit,
} from "@yas-run/core/layout";
import { describe, expect, it } from "vitest";
import {
  movePaneInDirection,
  movePaneBeside,
  splitPaneWithAssignment,
} from "../layout/swayLayout";
import type { SpatialDirection } from "../layout/spatialNavigation";

const leaf = (name: string): LayoutLeaf => ({ type: "leaf", command: name });
const split = (
  direction: LayoutSplit["direction"],
  ...nodes: LayoutNode[]
): LayoutSplit => ({
  type: "split",
  direction,
  children: nodes.map((node) => ({ node, weight: 1 })),
});
const values = (root: LayoutNode) =>
  Object.fromEntries(
    enumeratePanes(root).map(({ id, leaf }) => [id, leaf.command!]),
  );
const paneId = (root: LayoutNode, value: string) =>
  enumeratePanes(root).find(({ leaf }) => leaf.command === value)!.id;
const shape = (root: LayoutNode): unknown =>
  root.type === "leaf"
    ? root.command
    : [root.direction, ...root.children.map(({ node }) => shape(node))];
const move = (root: LayoutNode, value: string, direction: SpatialDirection) =>
  movePaneInDirection(root, values(root), paneId(root, value), direction);
const drop = (
  root: LayoutNode,
  value: string,
  target: string,
  direction: SpatialDirection,
) =>
  movePaneBeside(
    root,
    values(root),
    paneId(root, value),
    paneId(root, target),
    direction,
  );

describe("keyboard container movement", () => {
  it.each([
    ["horizontal", "left", "a"],
    ["horizontal", "right", "c"],
    ["vertical", "up", "a"],
    ["vertical", "down", "c"],
  ] as const)(
    "leaves the %s %s edge and its ratios unchanged",
    (axis, direction, source) => {
      const root = split(axis, leaf("a"), leaf("b"), leaf("c"));
      root.children.forEach((child, index) => {
        child.weight = index + 2;
      });
      expect(move(root, source, direction)).toBeNull();
    },
  );

  it("moves exactly one sibling with its weight and label", () => {
    const root = split(
      "horizontal",
      leaf("a"),
      split("vertical", leaf("b"), leaf("c")),
      leaf("d"),
    );
    root.children[0] = { ...root.children[0], weight: 3, label: "A" };
    const next = move(root, "a", "right")!;
    expect(shape(next.root)).toEqual([
      "horizontal",
      ["vertical", "b", "c"],
      "a",
      "d",
    ]);
    expect((next.root as LayoutSplit).children[1]).toBe(root.children[0]);
    expect(next.assignments[next.focusedPaneId]).toBe("a");
  });

  it("exits the nested group before crossing the next sibling", () => {
    const root = split(
      "horizontal",
      split("vertical", leaf("a"), leaf("b")),
      leaf("c"),
    );
    root.children[0].weight = 6;
    root.children[1].weight = 2;
    const next = move(root, "b", "right")!;
    expect(shape(next.root)).toEqual(["horizontal", "a", "b", "c"]);
    expect(
      (next.root as LayoutSplit).children.map(({ weight }) => weight),
    ).toEqual([3, 3, 2]);
    const crossed = move(next.root, "b", "right")!;
    expect(shape(crossed.root)).toEqual(["horizontal", "a", "c", "b"]);
    expect(move(crossed.root, "b", "right")).toBeNull();
  });

  it("extracts to the nearest matching ancestor instead of the whole workspace", () => {
    const root = split(
      "vertical",
      leaf("a"),
      split("horizontal", split("vertical", leaf("b"), leaf("c")), leaf("d")),
    );
    const next = move(root, "c", "left")!;
    expect(shape(next.root)).toEqual([
      "vertical",
      "a",
      ["horizontal", "c", "b", "d"],
    ]);
  });

  it("creates one perpendicular root split, then stops at its edge", () => {
    const root = split("horizontal", leaf("a"), leaf("b"), leaf("c"));
    const next = move(root, "b", "up")!;
    expect(shape(next.root)).toEqual([
      "vertical",
      "b",
      ["horizontal", "a", "c"],
    ]);
    expect(move(next.root, "b", "up")).toBeNull();
  });

  it.each([
    ["tabs", "left", "up", "vertical"],
    ["stacking", "up", "left", "horizontal"],
  ] as const)(
    "reorders %s on its axis and extracts on the perpendicular axis",
    (layout, reorder, extract, crossAxis) => {
      const root = split(layout, leaf("a"), leaf("b"), leaf("c"));
      const next = move(root, "b", reorder)!;
      expect(shape(next.root)).toEqual([layout, "b", "a", "c"]);
      const outside = move(next.root, "b", extract)!;
      expect(shape(outside.root)).toEqual([crossAxis, "b", [layout, "a", "c"]]);
    },
  );

  it("keeps tiled edge movement inside the base, preserving floating frames", () => {
    const frame: LayoutChild = {
      node: leaf("c"),
      weight: 1,
      rect: { x: 10, y: 20, width: 30, height: 40 },
    };
    const root = split("workspace", split("horizontal", leaf("a"), leaf("b")));
    root.children.push(frame);
    const next = move(root, "b", "up")!;
    expect(shape(next.root)).toEqual([
      "workspace",
      ["vertical", "b", "a"],
      "c",
    ]);
    expect((next.root as LayoutSplit).children[1]).toBe(frame);
    expect(move(next.root, "b", "up")).toBeNull();
    expect(move(root, "c", "up")).toBeNull();
  });

  it("does not manufacture panes for the sole tiled window", () => {
    expect(move(leaf("a"), "a", "up")).toBeNull();
    const root = split("workspace", leaf("a"), leaf("b"));
    root.children[1].rect = { x: 10, y: 10, width: 50, height: 50 };
    expect(move(root, "a", "up")).toBeNull();
  });

  it("preserves every leaf, assignment and focus through mixed movement sequences", () => {
    let root: LayoutNode = split(
      "workspace",
      split(
        "horizontal",
        split("vertical", leaf("a"), leaf("b")),
        split("tabs", leaf("c"), leaf("d")),
        split("stacking", leaf("e"), leaf("f")),
      ),
      leaf("float"),
    );
    const frame = (root as LayoutSplit).children[1];
    frame.rect = { x: 10, y: 10, width: 50, height: 50 };
    const originalLeaves = new Set(
      enumeratePanes(root).map(({ leaf }) => leaf),
    );
    const directions: SpatialDirection[] = ["up", "right", "down", "left"];
    let seed = 53;
    const random = (limit: number) => {
      seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
      return (seed >>> 16) % limit;
    };
    for (let step = 0; step < 250; step += 1) {
      const name = "abcdef"[random(6)];
      const direction = directions[random(directions.length)];
      const before = JSON.stringify(root);
      const oldPanes = enumeratePanes(root);
      const next = move(root, name, direction);
      expect(JSON.stringify(root)).toBe(before);
      if (!next) continue;
      expect(() => validateLayoutNode(next.root)).not.toThrow();
      const nextPanes = enumeratePanes(next.root);
      expect(nextPanes).toHaveLength(originalLeaves.size);
      expect(new Set(nextPanes.map(({ leaf }) => leaf))).toEqual(
        originalLeaves,
      );
      expect(next.assignments).toEqual(values(next.root));
      expect(next.assignments[next.focusedPaneId]).toBe(name);
      for (const pane of oldPanes) {
        expect(
          nextPanes.find(({ id }) => id === next.paneIdMap.get(pane.id))?.leaf,
        ).toBe(pane.leaf);
      }
      expect(next.root.type).toBe("split");
      expect((next.root as LayoutSplit).direction).toBe("workspace");
      expect((next.root as LayoutSplit).children[1]).toBe(frame);
      root = next.root;
    }
  });
});

describe("explicit edge drops", () => {
  it.each([
    ["a", "b", "left", null],
    ["b", "a", "right", null],
    ["a", "c", "left", ["horizontal", "b", "a", "c"]],
    ["c", "a", "right", ["horizontal", "a", "c", "b"]],
  ] as const)(
    "drops %s on %s's %s edge",
    (source, target, direction, expected) => {
      const root = split("horizontal", leaf("a"), leaf("b"), leaf("c"));
      const next = drop(root, source, target, direction);
      expect(next ? shape(next.root) : null).toEqual(expected);
    },
  );

  it.each(["tabs", "stacking"] as const)(
    "extracts the active view beside all remaining %s",
    (layout) => {
      const root = split(
        "horizontal",
        split(layout, leaf("a"), leaf("b"), leaf("c")),
        leaf("d"),
      );
      const next = drop(root, "b", "b", "up")!;
      expect(shape(next.root)).toEqual([
        "horizontal",
        ["vertical", "b", [layout, "a", "c"]],
        "d",
      ]);
    },
  );

  it("does not duplicate an existing owner when a drop has no source marker", () => {
    const root = split("horizontal", leaf("a"), leaf("b"));
    expect(
      splitPaneWithAssignment(root, values(root), "0", "a", "vertical"),
    ).toBeNull();
  });
});
