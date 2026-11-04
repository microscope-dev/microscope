// @ts-check
// The built-in line-plot: numbers in order, joined by straight segments.
//
//   values   numbers, or
//   series   a map or record of up to eight names to numbers
//   x        optional numbers, one per value, for the horizontal axis
//   log      optional; true draws the vertical axis in powers of ten
//
// Past one value a pixel column, each column keeps its first, lowest,
// highest, and last value (M4), so a one-sample spike still shows, and a
// band shades each column's range. NaN leaves a gap and ±∞ is an arrow at
// the edge, both counted in the caption. Each column's title lists every
// value there. A faint ghost is the line at the stop before.

const HEIGHT = 260;
const TOP = 26;
const BOTTOM = 24;
const LEFT = 56;

microscope.draw((input, { previous, width, paths }) => {
  const series = seriesOf(input);
  const ghosts = previous === null ? [] : safeSeries(previous);
  const x = input.x === undefined || input.x === null ? null : numbers(input.x, "x");
  const count = series.reduce((most, line) => Math.max(most, line.values.length), 0);
  if (x !== null && series.some((line) => line.values.length !== x.length)) {
    throw new RangeError(`\`x\` holds ${x.length} numbers, but the values number ${count}`);
  }
  const log = input.log === true;
  const theme = microscope.theme;
  const W = Math.round(Math.min(Math.max(width, 320), 1600));
  const right = W - (series.length > 1 ? 74 : 64);
  const bottom = HEIGHT - BOTTOM;
  const columns = Math.max(1, Math.floor(right - LEFT));

  // The vertical axis fits every finite value, now and at the stop before.
  let lo = Number.POSITIVE_INFINITY;
  let hi = Number.NEGATIVE_INFINITY;
  let nonPositive = 0;
  for (const line of [...series, ...ghosts]) {
    const own = series.includes(line);
    for (let i = 0; i < line.values.length; i++) {
      const v = line.values[i];
      if (!Number.isFinite(v)) continue;
      if (log && v <= 0) {
        if (own) nonPositive += 1;
        continue;
      }
      if (v < lo) lo = v;
      if (v > hi) hi = v;
    }
  }
  if (lo > hi) {
    lo = log ? 1 : 0;
    hi = log ? 10 : 1;
  }
  const axis = log ? logTicks(lo, hi) : nice(lo, hi);
  const toY = (v) => {
    const t = log
      ? (Math.log10(v) - Math.log10(axis.lo)) / (Math.log10(axis.hi) - Math.log10(axis.lo))
      : (v - axis.lo) / (axis.hi - axis.lo || 1);
    return bottom - t * (bottom - TOP);
  };

  // The horizontal axis: the given x, or each value's index.
  let xlo = 0;
  let xhi = Math.max(1, count - 1);
  let sorted = true;
  if (x !== null) {
    xlo = Number.POSITIVE_INFINITY;
    xhi = Number.NEGATIVE_INFINITY;
    for (let i = 0; i < x.length; i++) {
      if (!Number.isFinite(x[i])) continue;
      if (x[i] < xlo) xlo = x[i];
      if (x[i] > xhi) xhi = x[i];
      if (i > 0 && x[i] < x[i - 1]) sorted = false;
    }
    if (xlo > xhi) {
      xlo = 0;
      xhi = 1;
    }
    if (xlo === xhi) {
      xlo -= 1;
      xhi += 1;
    }
  }
  const xAxis = x === null ? { lo: xlo, hi: xhi, ticks: integerTicks(xlo, xhi) } : nice(xlo, xhi);
  const toX = (i) => {
    const v = x === null ? i : x[i];
    return LEFT + ((v - xAxis.lo) / (xAxis.hi - xAxis.lo || 1)) * (right - LEFT);
  };
  const decimated = count > columns && sorted;
  const column = (i) => Math.min(columns - 1, Math.max(0, Math.floor(toX(i) - LEFT)));
  const columnX = (c) => LEFT + c + 0.5;
  // Each pixel column's values, first to last, when there are more values
  // than columns.
  const spans = [];
  if (decimated) {
    for (let i = 0; i < count; i++) {
      const c = column(i);
      const span = spans.at(-1);
      if (span !== undefined && span.column === c) {
        span.last = i;
      } else {
        spans.push({ column: c, first: i, last: i });
      }
    }
  }

  const shapes = [];
  // Gridlines and ticks, recessive.
  for (const tick of axis.ticks) {
    const y = toY(tick);
    shapes.push(
      microscope.line({
        x1: LEFT,
        y1: y,
        x2: right,
        y2: y,
        stroke: tick === 0 ? theme.ink3 : theme.line,
      }),
      microscope.text({
        x: LEFT - 6,
        y,
        text: label(tick),
        size: 11,
        anchor: "end",
        baseline: "middle",
        fill: theme.ink3,
      }),
    );
  }
  for (const tick of xAxis.ticks) {
    const px = LEFT + ((tick - xAxis.lo) / (xAxis.hi - xAxis.lo || 1)) * (right - LEFT);
    shapes.push(
      microscope.text({
        x: px,
        y: bottom + 16,
        text: label(tick),
        size: 11,
        anchor: "middle",
        fill: theme.ink3,
      }),
    );
  }

  // The stop before, as a faint line beneath.
  let ghosted = false;
  for (const ghost of ghosts) {
    const now = series.find((line) => line.name === ghost.name);
    if (now === undefined || ghost.values.length !== now.values.length) continue;
    ghosted = true;
    for (const points of trace(ghost.values, toX, toY, decimated ? spans : null, columnX, log)
      .lines) {
      shapes.push(
        microscope.polyline({ points, stroke: theme.ink3, strokeWidth: 1, opacity: 0.6 }),
      );
    }
  }

  let nans = 0;
  let infinities = 0;
  const traced = series.map((line) => {
    const drawn = trace(line.values, toX, toY, decimated ? spans : null, columnX, log);
    nans += drawn.nans;
    infinities += drawn.infinities.length;
    return { line, drawn };
  });
  for (const { line, drawn } of traced) {
    const color = theme.series[line.hue];
    if (decimated) {
      for (const band of drawn.bands) {
        shapes.push(microscope.polygon({ points: band, fill: color, opacity: 0.18 }));
      }
    }
    for (const points of drawn.lines) {
      shapes.push(microscope.polyline({ points, stroke: color, strokeWidth: 2 }));
    }
    // Markers, when points stand at least 6 px apart.
    if (!decimated && count > 1 && (right - LEFT) / (count - 1) >= 6) {
      for (let i = 0; i < line.values.length; i++) {
        const v = line.values[i];
        if (Number.isFinite(v) && !(log && v <= 0)) {
          shapes.push(
            microscope.circle({
              x: toX(i),
              y: toY(v),
              r: 3,
              fill: color,
              stroke: theme.surface,
              strokeWidth: 1.5,
            }),
          );
        }
      }
    }
    for (const [i, up] of drawn.infinities) {
      const px = toX(i);
      const y = up ? TOP : bottom;
      const d = up ? 5 : -5;
      shapes.push(
        microscope.polygon({
          points: [px - 4, y + d, px + 4, y + d, px, y],
          fill: color,
          title: `[${i}] ${line.name}: ${up ? "+" : "-"}∞`,
        }),
      );
    }
  }

  // Every column, or every value, is a mark whose title lists what is there.
  const marks = decimated
    ? spans
    : Array.from({ length: count }, (_, i) => ({ column: i, first: i, last: i }));
  const single = series.length === 1 && input.values !== undefined && paths.values !== null;
  for (const { column: mark, first, last } of marks) {
    const left = decimated ? LEFT + mark : mark === 0 ? LEFT : (toX(mark - 1) + toX(mark)) / 2;
    const end = decimated
      ? LEFT + mark + 1
      : mark === count - 1
        ? right
        : (toX(mark) + toX(mark + 1)) / 2;
    const lines = [first === last ? `[${first}]` : `[${first}…${last}]`];
    if (x !== null && first === last) lines[0] += ` x = ${exact(x[first], input.x, first)}`;
    for (const { line } of traced) {
      if (first === last) {
        lines.push(`${line.name}: ${exact(line.values[first], line.raw, first)}`);
      } else {
        const range = extremes(line.values, first, last);
        lines.push(
          range === null
            ? `${line.name}: no number`
            : `${line.name}: min ${exact(line.values[range.min], line.raw, range.min)} at [${range.min}], max ${exact(line.values[range.max], line.raw, range.max)} at [${range.max}]`,
        );
      }
    }
    shapes.push(
      microscope.rect({
        x: left,
        y: TOP,
        width: Math.max(0.5, end - left),
        height: bottom - TOP,
        fill: "transparent",
        title: lines.join("\n"),
        select: single && first === last ? `${paths.values}[${first}]` : undefined,
      }),
    );
  }

  // A legend for several series or a ghost, and each line's last value at
  // its end.
  if (series.length > 1 || ghosted) {
    const keys = traced.map(({ line }) => ({ name: line.name, stroke: theme.series[line.hue] }));
    if (ghosted) keys.push({ name: "stop before", stroke: theme.ink3 });
    let lx = LEFT;
    for (const key of keys) {
      shapes.push(
        microscope.line({
          x1: lx,
          y1: 10,
          x2: lx + 12,
          y2: 10,
          stroke: key.stroke,
          strokeWidth: 2,
        }),
        microscope.text({
          x: lx + 16,
          y: 10,
          text: key.name,
          size: 11,
          baseline: "middle",
          fill: theme.ink2,
        }),
      );
      lx += 28 + key.name.length * 6.5;
    }
  }
  const ends = traced
    .map(({ line }) => {
      const i = lastFinite(line.values, log);
      return i === null ? null : { line, i, y: toY(line.values[i]) };
    })
    .filter((end) => end !== null)
    .toSorted((a, b) => a.y - b.y);
  for (let k = 1; k < ends.length; k++) {
    ends[k].y = Math.max(ends[k].y, ends[k - 1].y + 13);
  }
  for (const { line, i, y } of ends) {
    shapes.push(
      microscope.line({
        x1: right + 4,
        y1: y,
        x2: right + 12,
        y2: y,
        stroke: theme.series[line.hue],
        strokeWidth: 2,
      }),
      microscope.text({
        x: right + 15,
        y,
        text: label(line.values[i]),
        size: 11,
        baseline: "middle",
        family: "mono",
        fill: theme.ink,
      }),
    );
  }

  // The caption: how many, the extremes, and what is not drawn as a value.
  const caption = [series.length > 1 ? `${series.length} series × ${count}` : `n=${count}`];
  /** @type {{ line: (typeof series)[number], at: number }[]} */
  const lows = [];
  /** @type {{ line: (typeof series)[number], at: number }[]} */
  const highs = [];
  for (const line of series) {
    const range = extremes(line.values, 0, line.values.length - 1);
    if (range !== null) {
      lows.push({ line, at: range.min });
      highs.push({ line, at: range.max });
    }
  }
  lows.sort((a, b) => a.line.values[a.at] - b.line.values[b.at]);
  highs.sort((a, b) => b.line.values[b.at] - a.line.values[a.at]);
  /** @param {{ line: { name: string } }} s */
  const who = (s) => (series.length > 1 ? `${s.line.name} ` : "");
  const [lowest] = lows;
  const [highest] = highs;
  if (lowest !== undefined && highest !== undefined) {
    caption.push(
      `min ${who(lowest)}${brief(exact(lowest.line.values[lowest.at], lowest.line.raw, lowest.at))} at [${lowest.at}]`,
      `max ${who(highest)}${brief(exact(highest.line.values[highest.at], highest.line.raw, highest.at))} at [${highest.at}]`,
    );
  }
  if (series.length === 1) {
    const mean = meanOf(series[0].values);
    if (mean !== null) caption.push(`mean ${short(mean)}`);
  }
  if (nans > 0) caption.push(`${nans} NaN, left as gaps`);
  if (infinities > 0) caption.push(`${infinities} ±∞, arrows at the edge`);
  if (nonPositive > 0) caption.push(`${nonPositive} ≤ 0 left out of the log scale`);
  if (decimated) caption.push(`${short(count / columns)} values a column: min/max band`);
  if (input.series !== undefined && seriesCount(input.series) > series.length) {
    caption.push(
      `${seriesCount(input.series) - series.length} more series not drawn; the Table has them`,
    );
  }
  if (log) caption.push("log scale");
  return microscope.picture({ width: W, height: HEIGHT, shapes, caption: caption.join(" · ") });
});

/** The series to draw: their names, numbers, and hues, which follow names. */
function seriesOf(input) {
  if (input.values !== undefined && input.values !== null) {
    return [{ name: "values", values: numbers(input.values, "values"), raw: input.values, hue: 0 }];
  }
  if (input.series === undefined || input.series === null) {
    throw new TypeError("line-plot takes `values`, or `series`: names and their numbers");
  }
  const named = entries(input.series, "series").map(([name, values]) => ({
    name: nameOf(name),
    values: numbers(values, `series ${nameOf(name)}`),
    raw: values,
  }));
  const kept = named.slice(0, 8);
  const order = kept.map((line) => line.name).toSorted();
  return kept.map((line) => ({
    name: line.name,
    values: line.values,
    raw: line.raw,
    hue: order.indexOf(line.name),
  }));
}

/** The previous stop's series, or none when its inputs no longer read. */
function safeSeries(previous) {
  try {
    return seriesOf(previous);
  } catch {
    return [];
  }
}

function seriesCount(series) {
  try {
    return entries(series, "series").length;
  } catch {
    return 0;
  }
}

/** A map's [key, value] pairs, or a record's members. */
function entries(value, what) {
  if (Array.isArray(value) && value.every((entry) => Array.isArray(entry) && entry.length === 2)) {
    return value;
  }
  if (
    typeof value === "object" &&
    value !== null &&
    !Array.isArray(value) &&
    !ArrayBuffer.isView(value)
  ) {
    return Object.entries(value);
  }
  throw new TypeError(`\`${what}\` must be a map or a record of names to numbers`);
}

function nameOf(key) {
  if (typeof key === "string") return key;
  if (typeof key === "object" && key !== null && typeof key.name === "string") return key.name;
  return String(key);
}

/** Numbers as doubles, from a typed array or an array of numbers.
 * @returns {ArrayLike<number>} */
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
      throw new TypeError(`\`${what}[${index}]\` is not a number`);
    });
  }
  throw new TypeError(`line-plot draws numbers, and \`${what}\` is ${describe(value)}`);
}

function describe(value) {
  if (value === null) return "null";
  if (Array.isArray(value)) return "an array of other things";
  return typeof value === "object" ? "a record" : `a ${typeof value}`;
}

/** The value at `i` exactly as the program holds it: a 64-bit integer
 * whole, a float as the shortest text that reads back as it. */
function exact(v, raw, i) {
  const held = raw !== undefined && raw !== null && typeof raw === "object" ? raw[i] : undefined;
  if (typeof held === "bigint") return String(held);
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

/** The polylines, bands, and edges of one series, decimated by column
 * when `spans` lists each column's values. */
function trace(values, toX, toY, spans, columnX, log) {
  const lines = [];
  const bands = [];
  const infinities = [];
  let nans = 0;
  let points = [];
  let top = [];
  let base = [];
  const finish = () => {
    if (points.length >= 2) lines.push(points);
    if (top.length >= 2) {
      const ring = top.slice();
      for (let k = base.length - 2; k >= 0; k -= 2) ring.push(base[k], base[k + 1]);
      bands.push(ring);
    }
    points = [];
    top = [];
    base = [];
  };
  const usable = (v) => Number.isFinite(v) && !(log && v <= 0);
  if (spans === null) {
    for (let i = 0; i < values.length; i++) {
      const v = values[i];
      if (Number.isNaN(v)) nans += 1;
      if (v === Number.POSITIVE_INFINITY || v === Number.NEGATIVE_INFINITY)
        infinities.push([i, v > 0]);
      if (!usable(v)) {
        finish();
        continue;
      }
      points.push(toX(i), toY(v));
    }
    finish();
    return { lines, bands, infinities, nans };
  }
  for (const span of spans) {
    let first = Number.NaN;
    let last = Number.NaN;
    let lo = Number.POSITIVE_INFINITY;
    let hi = Number.NEGATIVE_INFINITY;
    let gap = false;
    for (let i = span.first; i <= span.last && i < values.length; i++) {
      const v = values[i];
      if (Number.isNaN(v)) nans += 1;
      if (v === Number.POSITIVE_INFINITY || v === Number.NEGATIVE_INFINITY)
        infinities.push([i, v > 0]);
      if (usable(v)) {
        if (Number.isNaN(first)) first = v;
        last = v;
        if (v < lo) lo = v;
        if (v > hi) hi = v;
      } else {
        gap = true;
      }
    }
    const px = columnX(span.column);
    if (!Number.isNaN(first)) {
      points.push(px, toY(first), px, toY(lo), px, toY(hi), px, toY(last));
      top.push(px, toY(hi));
      base.push(px, toY(lo));
    }
    if (gap) finish();
  }
  finish();
  return { lines, bands, infinities, nans };
}

/** Where the lowest and highest finite values of `first..=last` are. */
function extremes(values, first, last) {
  let min = -1;
  let max = -1;
  for (let i = first; i <= last; i++) {
    const v = values[i];
    if (!Number.isFinite(v)) continue;
    if (min < 0 || v < values[min]) min = i;
    if (max < 0 || v > values[max]) max = i;
  }
  return min < 0 ? null : { min, max };
}

function lastFinite(values, log) {
  for (let i = values.length - 1; i >= 0; i--) {
    if (Number.isFinite(values[i]) && !(log && values[i] <= 0)) return i;
  }
  return null;
}

function meanOf(values) {
  let total = 0;
  let n = 0;
  for (const v of values) {
    if (Number.isFinite(v)) {
      total += v;
      n += 1;
    }
  }
  return n === 0 ? null : total / n;
}

/** Round ticks that cover lo..hi: steps of 1, 2, or 5 times a power of ten. */
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

function integerTicks(lo, hi) {
  const axis = nice(lo, hi);
  return axis.ticks.filter((tick) => Number.isInteger(tick) && tick >= lo && tick <= hi);
}

function logTicks(lo, hi) {
  const start = 10 ** Math.floor(Math.log10(lo));
  const end = 10 ** Math.ceil(Math.log10(hi));
  const ticks = [];
  for (let t = start; t <= end * 1.0001; t *= 10) ticks.push(t);
  return { lo: start, hi: end === start ? start * 10 : end, ticks };
}

/** A tick's label: short, with K, M, or G past ten thousand. */
function label(v) {
  const a = Math.abs(v);
  if (a >= 1e4) {
    return new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: 2 }).format(v);
  }
  return short(v);
}

function short(v) {
  return Number(v.toPrecision(4)).toString();
}
