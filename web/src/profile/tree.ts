// A call tree as the server sends it, in columns, and the geometry of its
// flame graph. Nodes are in depth-first order, each node's children
// largest first, so a subtree is the run of nodes from it to its `end`.

import { assert } from "../assert";
import type { ProfileKey, ProfileTree } from "../protocol";

export interface CallTree {
  count: number;
  own: Float64Array;
  total: Float64Array;
  /** Indices into `keys`. */
  key: Uint32Array;
  depth: Uint32Array;
  /** The caller's node, or -1 for a root. */
  parent: Int32Array;
  /** One past each node's last descendant. */
  end: Uint32Array;
  keys: ProfileKey[];
  /** The weight of every sample the query kept. */
  weight: number;
  samples: number;
  /** For a diff, each node's weights in the base, and the base's. */
  baseOwn: Float64Array | null;
  baseTotal: Float64Array | null;
  baseWeight: number | null;
}

/** Decodes an answer and the columns sent before it: own and total
 * weights as little-endian `f64`s, then keys and depths as `u32`s, and
 * for a diff, the base's own and total weights as `f64`s. */
export function decodeTree(answer: ProfileTree, payload: Uint8Array | undefined): CallTree {
  const count = answer.nodes;
  const bytes = payload ?? new Uint8Array();
  const size = count * (answer.base ? 40 : 24);
  if (bytes.byteLength !== size) {
    throw new Error(`a tree of ${count} nodes needs ${size} bytes, not ${bytes.byteLength}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const own = new Float64Array(count);
  const total = new Float64Array(count);
  const key = new Uint32Array(count);
  const depth = new Uint32Array(count);
  for (let node = 0; node < count; node += 1) {
    own[node] = view.getFloat64(node * 8, true);
    total[node] = view.getFloat64((count + node) * 8, true);
    key[node] = view.getUint32(count * 16 + node * 4, true);
    depth[node] = view.getUint32(count * 20 + node * 4, true);
  }
  const parent = new Int32Array(count);
  const end = new Uint32Array(count);
  // The open ancestors of the node being read, outermost first.
  const open: number[] = [];
  for (let node = 0; node < count; node += 1) {
    const at = depth[node] ?? 0;
    while (open.length > at) {
      const closed = open.pop();
      assert(closed !== undefined, "closing a node with no open ancestor");
      end[closed] = node;
    }
    if (open.length !== at) {
      throw new Error(`node ${node} is ${at} deep under ${open.length} ancestors`);
    }
    parent[node] = open.at(-1) ?? -1;
    open.push(node);
  }
  for (const node of open) {
    end[node] = count;
  }
  const baseColumn = (column: number) =>
    Float64Array.from({ length: count }, (_, node) =>
      view.getFloat64(count * 24 + (column * count + node) * 8, true),
    );
  return {
    count,
    own,
    total,
    key,
    depth,
    parent,
    end,
    keys: answer.keys,
    weight: answer.weight,
    samples: answer.samples,
    baseOwn: answer.base ? baseColumn(0) : null,
    baseTotal: answer.base ? baseColumn(1) : null,
    baseWeight: answer.base?.weight ?? null,
  };
}

export function nameOf(tree: CallTree, node: number): string {
  return tree.keys[tree.key[node] ?? 0]?.name ?? "";
}

/** A node's path of names from its root. */
export function pathOf(tree: CallTree, node: number): string[] {
  assert(node >= 0 && node < tree.count, `node ${node} is outside a tree of ${tree.count}`);
  const path: string[] = [];
  for (let at = node; at >= 0; at = tree.parent[at] ?? -1) {
    path.push(nameOf(tree, at));
  }
  assert(
    path.length === (tree.depth[node] ?? 0) + 1,
    `node ${node} at depth ${tree.depth[node]} has a path of ${path.length}`,
  );
  return path.toReversed();
}

/** The node a path of names from a root reaches, or null. */
export function findPath(tree: CallTree, path: readonly string[]): number | null {
  let node = -1;
  let from = 0;
  let to = tree.count;
  for (const [depth, name] of path.entries()) {
    let found = -1;
    for (let child = from; child < to; child = tree.end[child] ?? to) {
      if ((tree.depth[child] ?? 0) === depth && nameOf(tree, child) === name) {
        found = child;
        break;
      }
    }
    if (found < 0) {
      return null;
    }
    node = found;
    from = node + 1;
    to = tree.end[node] ?? from;
    assert(from <= to && to <= tree.count, `node ${node} spans ${from}..${to} of ${tree.count}`);
  }
  assert(
    node < 0 || tree.depth[node] === path.length - 1,
    `path of ${path.length} reached node ${node} at depth ${tree.depth[node]}`,
  );
  return node < 0 ? null : node;
}

/** One node's rectangle: its row, and where it spans across, as
 * fractions of the width. */
export interface Rect {
  node: number;
  depth: number;
  x: number;
  width: number;
}

/**
 * The flame graph, roots on top, zoomed to `zoom`: the zoomed node spans
 * the whole width with its subtree below it, its ancestors span it above,
 * and nothing else is drawn. Rectangles narrower than `narrowest` are
 * left out, and so are their subtrees, which are narrower still.
 */
export function layout(tree: CallTree, zoom: number | null, narrowest: number): Rect[] {
  assert(narrowest >= 0, `narrowest rectangle ${narrowest} is negative`);
  const rects: Rect[] = [];
  if (tree.count === 0) {
    return rects;
  }
  let from = 0;
  let to = tree.count;
  let scale = tree.weight > 0 ? 1 / tree.weight : 0;
  if (zoom !== null) {
    assert(zoom >= 0 && zoom < tree.count, `zoom ${zoom} is outside a tree of ${tree.count}`);
    for (let at = tree.parent[zoom] ?? -1; at >= 0; at = tree.parent[at] ?? -1) {
      rects.push({ node: at, depth: tree.depth[at] ?? 0, x: 0, width: 1 });
    }
    rects.reverse();
    assert(
      rects.length === tree.depth[zoom],
      `zoom ${zoom} at depth ${tree.depth[zoom]} has ${rects.length} ancestors`,
    );
    from = zoom;
    to = tree.end[zoom] ?? zoom + 1;
    assert(zoom < to && to <= tree.count, `zoom ${zoom} ends at ${to} of ${tree.count}`);
    const total = tree.total[zoom] ?? 0;
    scale = total > 0 ? 1 / total : 0;
  }
  // The next free x of each depth: siblings follow each other, and a
  // node's first child starts where it does.
  const next: number[] = [];
  for (let node = from; node < to;) {
    const depth = tree.depth[node] ?? 0;
    const x = next[depth] ?? 0;
    const width = (tree.total[node] ?? 0) * scale;
    next[depth] = x + width;
    if (width < narrowest) {
      node = tree.end[node] ?? node + 1;
      continue;
    }
    rects.push({ node, depth, x, width });
    next[depth + 1] = x;
    node += 1;
  }
  return rects;
}

/** Which rectangle holds a point, by row and fraction across. */
export function hit(rects: readonly Rect[], depth: number, x: number): Rect | null {
  for (const rect of rects) {
    if (rect.depth === depth && x >= rect.x && x < rect.x + rect.width) {
      return rect;
    }
  }
  return null;
}
