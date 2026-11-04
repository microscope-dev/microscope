// @ts-check
// The built-in scatter-plot: a dot per pair of numbers.
//
//   x, y     numbers, one each per point; or
//   values   points, each a pair or a record whose first two members are
//            numbers
//   group    optional, one per point: up to three groups get their own
//            hues, and the rest are "other"
//   labels   optional, one per point, for its title
//
// Up to 10,000 points, each is a dot titled with its index and its exact
// coordinates, which opens it when clicked. Past that, the points are one
// image whose pixels darken with how many points fall in them. Points
// with a NaN or ±∞ are counted, not drawn. Up to 2,000 points, each one
// that moved since the stop before trails a line from where it was.

const HEIGHT = 300;
const LEFT = 56;
const TOP = 22;
const MOST_DOTS = 10_000;
const MOST_TRAILS = 2_000;

microscope.draw((input, { previous, width, paths }) => {
  const theme = microscope.theme;
  const points = pointsOf(input);
  const n = points.x.length;
  const groups = groupsOf(input.group, n);
  const labels = input.labels === undefined || input.labels === null ? null : input.labels;
  if (labels !== null && labels.length !== n) {
    throw new RangeError(`\`labels\` holds ${labels.length}, and the points number ${n}`);
  }
  const W = Math.round(Math.min(Math.max(width, 320), 1600));
  const right = W - 16;
  const bottom = HEIGHT - 26;

  let usable = 0;
  let xlo = Number.POSITIVE_INFINITY;
  let xhi = Number.NEGATIVE_INFINITY;
  let ylo = Number.POSITIVE_INFINITY;
  let yhi = Number.NEGATIVE_INFINITY;
  // Where the extremes are, to state them exactly.
  const at = { xlo: 0, xhi: 0, ylo: 0, yhi: 0 };
  for (let i = 0; i < n; i++) {
    const x = points.x[i];
    const y = points.y[i];
    if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
    usable += 1;
    if (x < xlo) [xlo, at.xlo] = [x, i];
    if (x > xhi) [xhi, at.xhi] = [x, i];
    if (y < ylo) [ylo, at.ylo] = [y, i];
    if (y > yhi) [yhi, at.yhi] = [y, i];
  }
  if (usable === 0) {
    xlo = ylo = 0;
    xhi = yhi = 1;
  }
  const xAxis = nice(xlo, xhi);
  const yAxis = nice(ylo, yhi);
  const toX = (v) => LEFT + ((v - xAxis.lo) / (xAxis.hi - xAxis.lo)) * (right - LEFT);
  const toY = (v) => bottom - ((v - yAxis.lo) / (yAxis.hi - yAxis.lo)) * (bottom - TOP);

  const shapes = [];
  for (const tick of yAxis.ticks) {
    shapes.push(
      microscope.line({
        x1: LEFT,
        y1: toY(tick),
        x2: right,
        y2: toY(tick),
        stroke: tick === 0 ? theme.ink3 : theme.line,
      }),
      microscope.text({
        x: LEFT - 6,
        y: toY(tick),
        text: label(tick),
        size: 11,
        anchor: "end",
        baseline: "middle",
        fill: theme.ink3,
      }),
    );
  }
  for (const tick of xAxis.ticks) {
    shapes.push(
      microscope.line({
        x1: toX(tick),
        y1: TOP,
        x2: toX(tick),
        y2: bottom,
        stroke: tick === 0 ? theme.ink3 : theme.line,
      }),
      microscope.text({
        x: toX(tick),
        y: bottom + 16,
        text: label(tick),
        size: 11,
        anchor: "middle",
        fill: theme.ink3,
      }),
    );
  }
  const hue = (g) => (g < 0 ? theme.ink3 : theme.series[g]);

  if (n > MOST_DOTS) {
    // One image: each pixel's count of points, as the hue's opacity.
    const columns = Math.max(1, Math.round(right - LEFT));
    const rows = Math.max(1, Math.round(bottom - TOP));
    const counts = new Uint32Array(columns * rows);
    let most = 0;
    for (let i = 0; i < n; i++) {
      const x = points.x[i];
      const y = points.y[i];
      if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
      const c = Math.min(columns - 1, Math.floor(toX(x) - LEFT));
      const r = Math.min(rows - 1, Math.floor(toY(y) - TOP));
      const cell = r * columns + c;
      counts[cell] += 1;
      if (counts[cell] > most) most = counts[cell];
    }
    const [red, green, blue] = rgb(theme.series[0]);
    const pixels = new Uint8ClampedArray(columns * rows * 4);
    const scale = Math.log1p(most) || 1;
    for (let cell = 0; cell < counts.length; cell++) {
      if (counts[cell] === 0) continue;
      pixels.set(
        [red, green, blue, Math.round(60 + 195 * (Math.log1p(counts[cell]) / scale))],
        cell * 4,
      );
    }
    shapes.push(
      microscope.image({
        x: LEFT,
        y: TOP,
        width: right - LEFT,
        height: bottom - TOP,
        pixels,
        columns,
        rows,
        title: "points",
      }),
    );
  } else {
    // Trails from where moved points were at the stop before.
    const drawTrails = (before) => {
      for (let i = 0; i < n; i++) {
        const x0 = before.x[i];
        const y0 = before.y[i];
        const x1 = points.x[i];
        const y1 = points.y[i];
        if ([x0, y0, x1, y1].every((value) => Number.isFinite(value)) && (x0 !== x1 || y0 !== y1)) {
          shapes.push(
            microscope.line({
              x1: toX(x0),
              y1: toY(y0),
              x2: toX(x1),
              y2: toY(y1),
              stroke: theme.ink3,
              strokeWidth: 1,
              opacity: 0.7,
            }),
          );
        }
      }
    };
    if (previous !== null && n <= MOST_TRAILS) {
      try {
        const before = pointsOf(previous);
        if (before.x.length === n) drawTrails(before);
      } catch {
        // The stop before had no points this chart reads.
      }
    }
    const select = points.path === null || points.path === undefined ? null : points.path;
    for (let i = 0; i < n; i++) {
      const x = points.x[i];
      const y = points.y[i];
      if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
      const name = labels === null ? "" : ` ${nameOf(labels[i])}`;
      shapes.push(
        microscope.circle({
          x: toX(x),
          y: toY(y),
          r: 4,
          fill: hue(groups.of[i]),
          stroke: theme.surface,
          strokeWidth: 1.5,
          title: `[${i}]${name}: x ${exact(points.x, points.rawX, i)}, y ${exact(points.y, points.rawY, i)}${groups.names.length > 0 ? ` (${groups.label(i)})` : ""}`,
          select:
            select === null ? undefined : points.form === "values" ? `${select}[${i}]` : undefined,
        }),
      );
    }
  }

  // A legend for groups.
  if (groups.names.length > 0) {
    let lx = LEFT;
    const keys = groups.names.map((name, g) => ({ name, fill: hue(g) }));
    if (groups.other > 0) keys.push({ name: "other", fill: theme.ink3 });
    for (const key of keys) {
      shapes.push(
        microscope.circle({ x: lx + 4, y: 10, r: 4, fill: key.fill }),
        microscope.text({
          x: lx + 12,
          y: 10,
          text: key.name,
          size: 11,
          baseline: "middle",
          fill: theme.ink2,
        }),
      );
      lx += 26 + key.name.length * 6.5;
    }
  }

  const caption = [`n=${n}`];
  if (groups.names.length > 0) {
    caption.push(
      `groups: ${groups.names.map((name, g) => `${name} ${groups.counts[g]}`).join(", ")}${groups.other > 0 ? `, other ${groups.other}` : ""}`,
    );
  }
  if (usable > 0) {
    caption.push(
      `x ${brief(exact(points.x, points.rawX, at.xlo))} to ${brief(exact(points.x, points.rawX, at.xhi))}`,
      `y ${brief(exact(points.y, points.rawY, at.ylo))} to ${brief(exact(points.y, points.rawY, at.yhi))}`,
    );
  }
  if (usable < n) caption.push(`${n - usable} with NaN or ±∞, not drawn`);
  if (n > MOST_DOTS) caption.push("one image: darker pixels hold more points");
  return microscope.picture({ width: W, height: HEIGHT, shapes, caption: caption.join(" · ") });

  function pointsOf(from) {
    if (from.x !== undefined && from.y !== undefined) {
      const x = numbers(from.x, "x");
      const y = numbers(from.y, "y");
      if (x.length !== y.length) {
        throw new RangeError(`\`x\` holds ${x.length} numbers and \`y\` ${y.length}`);
      }
      return { x, y, rawX: from.x, rawY: from.y, form: "xy", path: null };
    }
    if (from.values !== undefined && Array.isArray(from.values)) {
      const pairs = from.values.map((point, i) => {
        const parts = Array.isArray(point)
          ? point
          : typeof point === "object" && point !== null
            ? Object.values(point)
            : [];
        const [px, py] = parts;
        if (
          (typeof px !== "number" && typeof px !== "bigint") ||
          (typeof py !== "number" && typeof py !== "bigint")
        ) {
          throw new TypeError(
            `scatter-plot draws pairs of numbers, and \`values[${i}]\` is not one`,
          );
        }
        return [px, py];
      });
      return {
        x: Float64Array.from(pairs, (pair) => Number(pair[0])),
        y: Float64Array.from(pairs, (pair) => Number(pair[1])),
        rawX: pairs.map((pair) => pair[0]),
        rawY: pairs.map((pair) => pair[1]),
        form: "values",
        path: paths.values,
      };
    }
    throw new TypeError("scatter-plot takes `x` and `y`, or `values`: pairs of numbers");
  }
});

/** Up to three groups by their first appearance, and the rest as other. */
function groupsOf(group, n) {
  if (group === undefined || group === null) {
    return { names: [], of: new Int8Array(n), counts: [], other: 0, label: () => "" };
  }
  if (group.length !== n) {
    throw new RangeError(`\`group\` holds ${group.length}, and the points number ${n}`);
  }
  const names = [];
  const of = new Int8Array(n);
  const counts = [0, 0, 0];
  let other = 0;
  const seen = Array.from(group, nameOf);
  // Hues follow names: the three most common, in name order.
  const tally = new Map();
  for (const name of seen) tally.set(name, (tally.get(name) ?? 0) + 1);
  const chosen = [...tally.entries()]
    .toSorted((a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : 1))
    .slice(0, 3)
    .map(([name]) => name)
    .toSorted();
  names.push(...chosen);
  for (let i = 0; i < n; i++) {
    const g = names.indexOf(seen[i]);
    of[i] = g;
    if (g < 0) other += 1;
    else counts[g] += 1;
  }
  return { names, of, counts, other, label: (i) => seen[i] };
}

/** @returns {ArrayLike<number>} */
function numbers(value, what) {
  if (value instanceof BigInt64Array || value instanceof BigUint64Array) {
    return Float64Array.from(value, Number);
  }
  if (ArrayBuffer.isView(value) && !(value instanceof DataView)) {
    // oxlint-disable-next-line no-inline-comments -- a JSDoc type cast has to sit inline
    return /** @type {ArrayLike<number>} */ (/** @type {unknown} */ (value));
  }
  if (Array.isArray(value)) {
    return Float64Array.from(value, (item, index) => {
      if (typeof item === "number") return item;
      if (typeof item === "bigint") return Number(item);
      throw new TypeError(`scatter-plot draws numbers, and \`${what}[${index}]\` is not one`);
    });
  }
  throw new TypeError(`\`${what}\` must be numbers`);
}

function nameOf(key) {
  if (typeof key === "string") return key;
  if (typeof key === "object" && key !== null) {
    if (typeof key.name === "string") return key.name;
    if (typeof key.variant === "string") return key.variant;
  }
  return String(key);
}

function exact(values, raw, i) {
  const held = raw[i];
  if (typeof held === "bigint") return String(held);
  const v = values[i];
  if (Object.is(v, -0)) return "-0";
  if (raw instanceof Float32Array && Number.isFinite(v)) {
    for (let digits = 1; digits <= 9; digits++) {
      const shortest = Number(v.toPrecision(digits));
      if (Math.fround(shortest) === v) return String(shortest);
    }
  }
  return String(v);
}

/** A caption's number: integers whole, floats to six significant digits;
 * titles and the Table have every digit. */
function brief(text) {
  return /^-?\d*\.\d{7,}|e/iu.test(text) && Number.isFinite(Number(text))
    ? String(Number(Number(text).toPrecision(6)))
    : text;
}

function rgb(color) {
  const hex = /^#([0-9a-f]{6})$/iu.exec(color)?.[1] ?? "2a78d6";
  return [0, 2, 4].map((at) => Number.parseInt(hex.slice(at, at + 2), 16));
}

function nice(lo, hi, count = 5) {
  if (lo === hi) {
    const pad = Math.abs(lo) / 10 || 1;
    lo -= pad;
    hi += pad;
  }
  const raw = (hi - lo) / count;
  const magnitude = 10 ** Math.floor(Math.log10(raw));
  const r = raw / magnitude;
  const step = (r >= 7.5 ? 10 : r >= 3.5 ? 5 : r >= 1.5 ? 2 : 1) * magnitude;
  const start = Math.floor(lo / step) * step;
  const end = Math.ceil(hi / step) * step;
  const ticks = [];
  for (let k = 0; start + k * step <= end + step / 2; k++)
    ticks.push(Number((start + k * step).toPrecision(12)));
  return { lo: start, hi: end, ticks };
}

function label(v) {
  if (Math.abs(v) >= 1e4) {
    return new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: 2 }).format(v);
  }
  return Number(v.toPrecision(4)).toString();
}
