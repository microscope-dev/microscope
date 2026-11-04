// @ts-check
// The built-in flame-graph: a call tree as an icicle, root on top.
//
//   nodes    records of name, value (its own samples), and parent (the
//            index of its parent; a root's is itself, negative, or past
//            the end); or
//   stacks   folded stacks: a map, or pairs, of "a;b;c" to a count
//   values   either, as Draw as… hands a value over
//
// Each frame's width is its total: its own value and its descendants'.
// Frames are colored by name, so a function keeps its color, and titled
// with self, total, and share of the whole; a node opens when clicked.
// Frames narrower than a pixel are counted, not drawn. Frames whose total
// changed since the stop before are outlined.

const ROW = 18;
const MOST_DEPTH = 48;

microscope.draw((input, { previous, width, paths }) => {
  const theme = microscope.theme;
  const tree = treeOf(input, paths);
  const before = new Map();
  if (previous !== null) {
    try {
      const old = treeOf(previous, {});
      old.walk((node, key) => before.set(key, old.total[node]));
    } catch {
      // The stop before had no tree this chart reads.
    }
  }
  const W = Math.round(Math.min(Math.max(width, 320), 1600));
  const left = 4;
  const right = W - 4;
  const whole = tree.total[tree.root];
  const wholeNumber = Number(whole);
  const shapes = [];
  let hidden = 0;
  let deepest = 0;
  let changed = 0;
  let cut = 0;

  const draw = (node, x, depth, key) => {
    const total = tree.total[node];
    const span = wholeNumber > 0 ? (Number(total) / wholeNumber) * (right - left) : right - left;
    if (span < 1) {
      hidden += tree.count[node];
      return;
    }
    if (depth >= MOST_DEPTH) {
      cut += tree.count[node];
      return;
    }
    deepest = Math.max(deepest, depth);
    const y = 4 + depth * ROW;
    const name = tree.names[node];
    const share = wholeNumber > 0 ? (100 * Number(total)) / wholeNumber : 100;
    const was = before.get(key);
    const moved = was !== undefined && was !== total;
    if (moved) changed += 1;
    shapes.push(
      microscope.rect({
        x: x + 0.5,
        y,
        width: Math.max(0.5, span - 1),
        height: ROW - 2,
        radius: 2,
        fill: tint(name, theme),
        stroke: moved ? theme.ink : undefined,
        strokeWidth: moved ? 1.5 : undefined,
        title: `${name}\nself ${tree.self[node]} · total ${total} · ${Number(share.toPrecision(3))}% of all${moved ? `\nwas ${was} at the stop before` : ""}`,
        select: tree.select(node),
      }),
    );
    const fits = Math.floor((span - 8) / 6.2);
    if (fits >= 3) {
      shapes.push(
        microscope.text({
          x: x + 4,
          y: y + ROW / 2 - 1,
          text: name.length > fits ? `${name.slice(0, fits - 1)}…` : name,
          size: 11,
          baseline: "middle",
          fill: theme.ink,
        }),
      );
    }
    let at = x;
    for (const child of tree.children[node]) {
      draw(child, at, depth + 1, `${key};${tree.names[child]}`);
      at += wholeNumber > 0 ? (Number(tree.total[child]) / wholeNumber) * (right - left) : 0;
    }
  };
  draw(tree.root, left, 0, tree.names[tree.root]);

  const caption = [
    `${tree.names.length - (tree.synthetic ? 1 : 0)} frames`,
    `${whole} in all`,
    "width = total",
  ];
  if (hidden > 0) caption.push(`${hidden} frames under 1 px, counted not drawn`);
  if (cut > 0) caption.push(`${cut} frames deeper than ${MOST_DEPTH}, not drawn`);
  if (before.size > 0) caption.push(`${changed} outlined: total changed since the stop before`);
  return microscope.picture({
    width: W,
    height: 8 + (deepest + 1) * ROW,
    shapes,
    caption: caption.join(" · "),
  });
});

/** The call tree: names, own values, totals, children, and a root. */
function treeOf(given, givenPaths) {
  // Draw as… hands over `values`: records are nodes, and pairs stacks.
  let input = given;
  let paths = givenPaths;
  if (given.nodes === undefined && given.stacks === undefined && given.values !== undefined) {
    const records =
      Array.isArray(given.values) &&
      given.values.every(
        (item) => typeof item === "object" && item !== null && !Array.isArray(item),
      );
    input = records ? { nodes: given.values } : { stacks: given.values };
    paths = records ? { nodes: givenPaths.values } : {};
  }
  /** @type {string[]} */
  const names = [];
  /** @type {(number | bigint)[]} */
  const self = [];
  /** @type {number[]} */
  const parents = [];
  let select = (/** @type {number} */ _node) => /** @type {string | undefined} */ (undefined);
  if (input.nodes !== undefined && input.nodes !== null) {
    if (!Array.isArray(input.nodes))
      throw new TypeError("`nodes` must be an array of records of name, value, and parent");
    input.nodes.forEach((node, index) => {
      if (
        typeof node !== "object" ||
        node === null ||
        node.name === undefined ||
        node.value === undefined ||
        node.parent === undefined
      ) {
        throw new TypeError(`\`nodes[${index}]\` must be a record of name, value, and parent`);
      }
      names.push(typeof node.name === "string" ? node.name : String(node.name?.name ?? node.name));
      self.push(count(node.value, `nodes[${index}].value`));
      parents.push(Number(node.parent));
    });
    const path = paths.nodes;
    if (path !== null && path !== undefined)
      select = (node) => (node < input.nodes.length ? `${path}[${node}]` : undefined);
  } else if (input.stacks !== undefined && input.stacks !== null) {
    const pairs = Array.isArray(input.stacks) ? input.stacks : Object.entries(input.stacks);
    const index = new Map();
    pairs.forEach((pair, k) => {
      const [stack, value] = Array.isArray(pair) ? pair : [pair?.stack, pair?.count];
      if (typeof stack !== "string")
        throw new TypeError(`\`stacks[${k}]\` must pair "a;b;c" text with a count`);
      let parent = -1;
      let key = "";
      for (const frame of stack.split(";")) {
        key = key === "" ? frame : `${key};${frame}`;
        let node = index.get(key);
        if (node === undefined) {
          node = names.length;
          index.set(key, node);
          names.push(frame);
          self.push(0);
          parents.push(parent);
        }
        parent = node;
      }
      if (parent >= 0) self[parent] = add(self[parent], count(value, `stacks[${k}]`));
    });
  } else {
    throw new TypeError(
      "flame-graph takes `nodes`, records of name, value, and parent, or `stacks`, folded text with counts",
    );
  }
  const n = names.length;
  /** @type {number[][]} */
  const children = names.map(() => []);
  const roots = [];
  for (let i = 0; i < n; i++) {
    const parent = parents[i];
    if (!Number.isInteger(parent) || parent < 0 || parent >= n || parent === i) roots.push(i);
    else children[parent].push(i);
  }
  // Several roots hang from one, named for them all.
  let root = roots[0] ?? 0;
  let synthetic = false;
  if (roots.length !== 1) {
    root = n;
    names.push("all");
    self.push(0);
    children.push(roots);
    synthetic = true;
  }
  // Totals, children first, by an explicit stack: trees may be deep.
  const total = /** @type {(number | bigint)[]} */ (self.slice());
  const frames = /** @type {number[]} */ (names.map(() => 1));
  const order = [];
  const stack = [root];
  const seen = new Uint8Array(names.length);
  while (stack.length > 0) {
    const node = /** @type {number} */ (stack.pop());
    if (seen[node]) throw new RangeError(`the nodes' parents form a cycle through ${names[node]}`);
    seen[node] = 1;
    order.push(node);
    for (const child of children[node]) stack.push(child);
  }
  if (order.length < names.length) {
    throw new RangeError("some nodes' parents form a cycle, which no root reaches");
  }
  for (let k = order.length - 1; k >= 0; k--) {
    const node = order[k];
    for (const child of children[node]) {
      total[node] = add(total[node], total[child]);
      frames[node] += frames[child];
    }
  }
  // Wider frames first, so the eye finds the heaviest paths at the left.
  for (const list of children)
    list.sort((a, b) => Number(total[b]) - Number(total[a]) || (names[a] < names[b] ? -1 : 1));
  const walk = (visit) => {
    const pending = [[root, names[root]]];
    while (pending.length > 0) {
      const [node, key] = /** @type {[number, string]} */ (pending.pop());
      visit(node, key);
      for (const child of children[node]) pending.push([child, `${key};${names[child]}`]);
    }
  };
  return { names, self, total, count: frames, children, root, synthetic, select, walk };
}

function count(value, what) {
  if (typeof value === "bigint") return value;
  if (typeof value === "number" && Number.isFinite(value) && value >= 0) return value;
  throw new TypeError(`\`${what}\` must be a count, not ${String(value)}`);
}

/** Exact sums: bigints stay whole, and so do numbers while they can. */
function add(a, b) {
  if (typeof a === "bigint" || typeof b === "bigint") {
    if (
      (typeof a === "number" && !Number.isInteger(a)) ||
      (typeof b === "number" && !Number.isInteger(b))
    )
      return Number(a) + Number(b);
    return BigInt(a) + BigInt(b);
  }
  return a + b;
}

/** A light tint of a series hue chosen by the frame's package, so a name
 * keeps its color and its neighbors from the same package share it. */
function tint(name, theme) {
  const where = name.replace(/^[(*&]+/, "").split(/::|\.(?=[A-Za-z_(*])|\//)[0] ?? name;
  let hash = 2166136261;
  for (let k = 0; k < where.length; k++) {
    hash ^= where.charCodeAt(k);
    hash = Math.imul(hash, 16777619);
  }
  const hue = theme.series[(hash >>> 0) % theme.series.length];
  return microscope.color.scale(0.55, theme.surface, hue);
}
