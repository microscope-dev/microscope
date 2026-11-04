// @ts-check
// The built-in histogram: how many samples fall in each range.
//
//   values   raw samples, or
//   counts   counts already binned, with
//   edges    the bins' edges, one more than the counts
//   bins     optional: how many bins to cut raw samples into
//   log      optional; true draws counts in powers of ten
//
// Raw samples get Freedman–Diaconis bins unless `bins` says otherwise,
// over the samples' whole range, and markers at p50, p90, and p99, found
// by selection rather than sorting (linear interpolation, type 7). Each
// bin's title gives its range, count, and the share of samples up to it.
// An outline shows the stop before's counts in the same bins.

const HEIGHT = 260;
const LEFT = 56;
const TOP = 44;
const MOST_BINS = 200;

microscope.draw((input, { previous, width }) => {
  const theme = microscope.theme;
  const W = Math.round(Math.min(Math.max(width, 320), 1600));
  const right = W - 16;
  const bottom = HEIGHT - 26;
  const log = input.log === true;
  const caption = [];

  /** @type {number[]} */
  let edges;
  /** @type {number[]} */
  let counts;
  /** @type {[string, number][]} */
  let marks = [];
  let n = 0;
  let odd = 0;
  /** @type {null | ((from: any) => number[] | null)} */
  let rebin = null;
  if (input.counts !== undefined && input.counts !== null) {
    counts = Array.from(numbers(input.counts, "counts"));
    edges = Array.from(numbers(input.edges, "edges"));
    if (edges.length !== counts.length + 1) {
      throw new RangeError(
        `\`edges\` holds ${edges.length}, and \`counts\` needs ${counts.length + 1}, one more`,
      );
    }
    for (let k = 1; k < edges.length; k++) {
      if (!(edges[k] > edges[k - 1]))
        throw new RangeError(`\`edges\` must rise, and edges[${k}] does not`);
    }
    if (counts.some((count) => !(count >= 0)))
      throw new RangeError("`counts` must not be negative");
    n = counts.reduce((total, count) => total + count, 0);
    marks = [
      ["p50", 0.5],
      ["p90", 0.9],
      ["p99", 0.99],
    ].map(([name, p]) => [String(name), binnedQuantile(counts, edges, Number(p), n)]);
    caption.push(`n=${exactCount(input.counts)}`, `${counts.length} bins as given`);
    rebin = (from) =>
      from.counts !== undefined && from.counts.length === counts.length
        ? Array.from(numbers(from.counts, "counts"))
        : null;
  } else if (input.values !== undefined && input.values !== null) {
    const raw = numbers(input.values, "values");
    const samples = new Float64Array(raw.length);
    for (let i = 0; i < raw.length; i++) {
      if (Number.isFinite(raw[i])) samples[n++] = raw[i];
      else odd += 1;
    }
    const finite = samples.subarray(0, n);
    let lo = Number.POSITIVE_INFINITY;
    let hi = Number.NEGATIVE_INFINITY;
    let total = 0;
    for (const v of finite) {
      if (v < lo) lo = v;
      if (v > hi) hi = v;
      total += v;
    }
    if (n === 0) {
      lo = 0;
      hi = 1;
    }
    const work = Float64Array.from(finite);
    const q = (p) => quantile(work, p);
    let bins;
    let how;
    if (input.bins !== undefined && input.bins !== null) {
      bins = Math.max(1, Math.min(MOST_BINS, Math.floor(Number(input.bins))));
      how = `${bins} bins as asked`;
    } else {
      const iqr = n > 1 ? q(0.75) - q(0.25) : 0;
      const fd = iqr > 0 ? (2 * iqr) / Math.cbrt(n) : 0;
      const wanted = fd > 0 ? Math.ceil((hi - lo) / fd) : Math.ceil(Math.log2(Math.max(1, n)) + 1);
      bins = Math.max(1, Math.min(MOST_BINS, wanted));
      how =
        fd > 0
          ? wanted > MOST_BINS
            ? `${bins} bins (Freedman–Diaconis wanted ${wanted})`
            : `${bins} bins (Freedman–Diaconis)`
          : `${bins} bins (Sturges: the middle half is one value)`;
    }
    if (lo === hi) {
      lo -= 0.5;
      hi += 0.5;
    }
    const step = (hi - lo) / bins;
    edges = Array.from({ length: bins + 1 }, (_, k) => (k === bins ? hi : lo + k * step));
    const bin = (v) => Math.min(bins - 1, Math.max(0, Math.floor((v - lo) / step)));
    counts = Array.from({ length: bins }, () => 0);
    for (const v of finite) counts[bin(v)] += 1;
    if (n > 0)
      marks = [
        ["p50", q(0.5)],
        ["p90", q(0.9)],
        ["p99", q(0.99)],
      ];
    caption.push(`n=${n}`, `${how} of ${short(step)}`);
    if (n > 0)
      caption.push(
        `min ${brief(exact(lo))}`,
        `max ${brief(exact(hi))}`,
        `mean ${short(total / n)}`,
      );
    rebin = (from) => {
      if (from.values === undefined) return null;
      const before = numbers(from.values, "values");
      const binned = Array.from({ length: bins }, () => 0);
      for (let i = 0; i < before.length; i++) {
        const v = before[i];
        if (Number.isFinite(v) && v >= lo && v <= hi) binned[bin(v)] += 1;
      }
      return binned;
    };
  } else {
    throw new TypeError("histogram takes `values`, raw samples, or `counts` and `edges`");
  }

  /** @type {number[] | null} */
  let before = null;
  if (previous !== null && rebin !== null) {
    try {
      before = rebin(previous);
    } catch {
      before = null;
    }
  }
  const most = Math.max(1, ...counts, ...(before ?? []));
  const yAxis = log ? logTicks(most) : nice(0, most);
  const toY = (count) => {
    if (log)
      return count <= 0
        ? bottom
        : bottom - (Math.log10(count) / Math.log10(yAxis.hi)) * (bottom - TOP);
    return bottom - (count / yAxis.hi) * (bottom - TOP);
  };
  const xlo = edges[0];
  const xhi = edges[counts.length];
  const toX = (v) => LEFT + ((v - xlo) / (xhi - xlo)) * (right - LEFT);

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
  for (const tick of nice(xlo, xhi).ticks.filter((t) => t >= xlo && t <= xhi)) {
    shapes.push(
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
  let running = 0;
  counts.forEach((count, k) => {
    running += count;
    const x0 = toX(edges[k]);
    const x1 = toX(edges[k + 1]);
    const share = n > 0 ? (100 * running) / n : 0;
    shapes.push(
      microscope.rect({
        x: x0 + 0.5,
        y: toY(count),
        width: Math.max(0.5, x1 - x0 - 1),
        height: Math.max(0, bottom - toY(count)),
        fill: theme.series[0],
        title: `[${exact(edges[k])}, ${exact(edges[k + 1])}${k === counts.length - 1 ? "]" : ")"}: ${count} · ${short(share)}% up to here`,
      }),
    );
  });
  if (before !== null) {
    const outline = [LEFT, bottom];
    before.forEach((count, k) => {
      outline.push(toX(edges[k]), toY(count), toX(edges[k + 1]), toY(count));
    });
    outline.push(right, bottom);
    shapes.push(
      microscope.polyline({
        points: outline,
        stroke: theme.ink,
        strokeWidth: 1,
        opacity: 0.7,
        title: "the stop before",
      }),
    );
  }
  // Each quantile's line, and its label above the plot; labels that would
  // overlap stack instead.
  let lastX = Number.NEGATIVE_INFINITY;
  let row = 0;
  for (const [name, at] of marks) {
    if (!Number.isFinite(at)) continue;
    const x = toX(at);
    row = x - lastX < 64 ? row + 1 : 0;
    lastX = x;
    const end = x > right - 60;
    shapes.push(
      microscope.line({
        x1: x,
        y1: TOP - 6,
        x2: x,
        y2: bottom,
        stroke: theme.ink,
        strokeWidth: 1,
        dash: [3, 2],
        title: `${name} ${exact(at)}`,
      }),
      microscope.text({
        x: end ? x - 3 : x + 3,
        y: TOP - 8 - row * 12,
        text: `${name} ${short(at)}`,
        size: 11,
        anchor: end ? "end" : "start",
        fill: theme.ink,
        title: `${name} ${exact(at)}`,
      }),
    );
  }

  if (odd > 0) caption.push(`${odd} NaN or ±∞, not counted`);
  if (input.values !== undefined && input.counts === undefined)
    caption.push("quantiles: linear (type 7)");
  if (input.counts !== undefined) caption.push("quantiles interpolate within bins");
  if (before !== null) caption.push("outline = the stop before");
  if (log) caption.push("log counts");
  return microscope.picture({ width: W, height: HEIGHT, shapes, caption: caption.join(" · ") });
});

/** The p-quantile of `values`, interpolating linearly between the order
 * statistics around (n - 1)p, as R's type 7 does; it reorders `values`. */
function quantile(values, p) {
  const n = values.length;
  if (n === 0) return Number.NaN;
  const h = (n - 1) * p;
  const lo = Math.floor(h);
  const below = select(values, lo);
  if (lo + 1 >= n) return below;
  // After selecting lo, everything past it is at least as large.
  let above = Number.POSITIVE_INFINITY;
  for (let i = lo + 1; i < n; i++) if (values[i] < above) above = values[i];
  return below + (h - lo) * (above - below);
}

/** The k-th smallest of `values` (Hoare's selection), which it leaves at
 * index k with smaller values before it and larger after. */
function select(values, k) {
  let left = 0;
  let right = values.length - 1;
  while (right > left) {
    const pivot = values[(left + right) >> 1];
    let i = left;
    let j = right;
    while (i <= j) {
      while (values[i] < pivot) i++;
      while (values[j] > pivot) j--;
      if (i <= j) {
        const t = values[i];
        values[i] = values[j];
        values[j] = t;
        i++;
        j--;
      }
    }
    if (k <= j) right = j;
    else if (k >= i) left = i;
    else break;
  }
  return values[k];
}

/** A quantile of binned counts, interpolating within its bin. */
function binnedQuantile(counts, edges, p, n) {
  if (n <= 0) return Number.NaN;
  const target = p * n;
  let running = 0;
  for (let k = 0; k < counts.length; k++) {
    if (running + counts[k] >= target && counts[k] > 0) {
      return edges[k] + ((target - running) / counts[k]) * (edges[k + 1] - edges[k]);
    }
    running += counts[k];
  }
  return edges[counts.length];
}

function exactCount(counts) {
  let total = 0n;
  let whole = true;
  for (const count of counts) {
    if (typeof count === "bigint") total += count;
    else if (Number.isSafeInteger(count)) total += BigInt(count);
    else whole = false;
  }
  return whole ? String(total) : String(Array.from(counts, Number).reduce((a, b) => a + b, 0));
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
      throw new TypeError(`histogram counts numbers, and \`${what}[${index}]\` is not one`);
    });
  }
  throw new TypeError(`\`${what}\` must be numbers`);
}

function exact(v) {
  return Object.is(v, -0) ? "-0" : String(v);
}

/** A caption's number: integers whole, floats to six significant digits;
 * titles and the Table have every digit. */
function brief(text) {
  return /^-?\d*\.\d{7,}|e/iu.test(text) && Number.isFinite(Number(text))
    ? String(Number(Number(text).toPrecision(6)))
    : text;
}

function short(v) {
  return Number(v.toPrecision(4)).toString();
}

function nice(lo, hi, count = 5) {
  if (lo === hi) hi = lo + 1;
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

function logTicks(most) {
  const ticks = [1];
  let tick = 1;
  while (tick < most) {
    tick *= 10;
    ticks.push(tick);
  }
  return { lo: 1, hi: Math.max(10, tick), ticks };
}

function label(v) {
  if (Math.abs(v) >= 1e4) {
    return new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: 2 }).format(v);
  }
  return short(v);
}
