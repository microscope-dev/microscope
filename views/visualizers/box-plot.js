// @ts-check
// The built-in box-plot: each group's spread.
//
//   groups       a map, record, or array of groups, each its samples or
//                its summary: a record of min, q1, median, q3, and max;
//                `values` may hold them instead
//   labels       optional, one per group when groups are an array
//   error        optional: "sd" for the mean ± one standard deviation,
//                "ci95" for the mean ± 1.96 standard errors, or numbers,
//                one half-width per group, with
//   mean         numbers, one per group, when error gives numbers
//   error_label  optional: what the error bars are, as "±1 sd"
//
// Boxes run from the first quartile to the third, with a line at the
// median, from samples by selection with linear interpolation (type 7).
// Whiskers reach the furthest samples within 1.5 × IQR of the box (Tukey),
// and samples beyond are outliers, drawn up to 1,000 a group and counted.
// A tick marks each median at the stop before.

const HEIGHT = 280;
const LEFT = 56;
const TOP = 26;
const MOST_OUTLIERS = 1_000;

microscope.draw((input, { previous, width, paths }) => {
  const theme = microscope.theme;
  const named = input.groups === undefined ? "values" : "groups";
  const groups = groupsOf(input[named], paths[named], input.labels);
  if (groups.length === 0) throw new RangeError("`groups` holds no groups");
  const errors = errorsOf(input, groups);
  const before = new Map();
  if (previous !== null) {
    try {
      for (const group of groupsOf(previous.groups ?? previous.values, null, previous.labels))
        before.set(group.name, group.stats.median);
    } catch {
      // The stop before had no groups this chart reads.
    }
  }
  const W = Math.round(Math.min(Math.max(width, 320), 1600));
  const right = W - 16;
  const bottom = HEIGHT - 40;

  let lo = Number.POSITIVE_INFINITY;
  let hi = Number.NEGATIVE_INFINITY;
  const take = (v) => {
    if (!Number.isFinite(v)) return;
    if (v < lo) lo = v;
    if (v > hi) hi = v;
  };
  for (const group of groups) {
    take(group.stats.min);
    take(group.stats.max);
  }
  for (const error of errors) {
    if (error !== null) {
      take(error.mean - error.half);
      take(error.mean + error.half);
    }
  }
  if (lo > hi) {
    lo = 0;
    hi = 1;
  }
  const axis = nice(lo, hi);
  const toY = (v) => bottom - ((v - axis.lo) / (axis.hi - axis.lo)) * (bottom - TOP);
  const shapes = [];
  for (const tick of axis.ticks) {
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
  const slot = (right - LEFT) / groups.length;
  const box = Math.max(6, Math.min(36, slot * 0.4));
  let outliers = 0;
  let hidden = 0;
  groups.forEach((group, k) => {
    const { stats } = group;
    const error = errors[k];
    const cx = LEFT + slot * k + slot / 2 - (error === null ? 0 : box / 2);
    const color = theme.series[0];
    const whisker = (from, to) => {
      shapes.push(
        microscope.line({ x1: cx, y1: toY(from), x2: cx, y2: toY(to), stroke: theme.ink2 }),
        microscope.line({
          x1: cx - box / 4,
          y1: toY(to),
          x2: cx + box / 4,
          y2: toY(to),
          stroke: theme.ink2,
        }),
      );
    };
    whisker(stats.q3, stats.high);
    whisker(stats.q1, stats.low);
    const summary = [
      `${group.name}${stats.n === null ? "" : `: n=${stats.n}`}`,
      `min ${exact(stats.min)}`,
      `q1 ${exact(stats.q1)}`,
      `median ${exact(stats.median)}`,
      `q3 ${exact(stats.q3)}`,
      `max ${exact(stats.max)}`,
    ];
    if (stats.n !== null) summary.push(`whiskers ${exact(stats.low)} to ${exact(stats.high)}`);
    shapes.push(
      microscope.rect({
        x: cx - box / 2,
        y: toY(stats.q3),
        width: box,
        height: Math.max(1, toY(stats.q1) - toY(stats.q3)),
        radius: 2,
        fill: color,
        opacity: 0.85,
        stroke: color,
        strokeWidth: 1.2,
        title: summary.join("\n"),
        select: group.select,
      }),
      microscope.line({
        x1: cx - box / 2,
        y1: toY(stats.median),
        x2: cx + box / 2,
        y2: toY(stats.median),
        stroke: theme.surface,
        strokeWidth: 2.5,
      }),
    );
    const was = before.get(group.name);
    if (was !== undefined && Number.isFinite(was) && was !== stats.median) {
      shapes.push(
        microscope.line({
          x1: cx - box / 2 - 6,
          y1: toY(was),
          x2: cx - box / 2 - 1,
          y2: toY(was),
          stroke: theme.ink,
          strokeWidth: 2,
          title: `${group.name}: median ${exact(was)} at the stop before`,
        }),
      );
    }
    for (const outlier of stats.outliers.slice(0, MOST_OUTLIERS)) {
      shapes.push(
        microscope.circle({
          x: cx,
          y: toY(outlier),
          r: 2.5,
          fill: "none",
          stroke: theme.ink2,
          title: `${group.name}: outlier ${exact(outlier)}`,
        }),
      );
    }
    outliers += stats.outliers.length;
    hidden += Math.max(0, stats.outliers.length - MOST_OUTLIERS);
    if (error !== null) {
      const ex = cx + box / 2 + 10;
      const title = `${group.name}: mean ${short(error.mean)} ${errors.label} (${short(error.mean - error.half)} to ${short(error.mean + error.half)})`;
      shapes.push(
        microscope.line({
          x1: ex,
          y1: toY(error.mean + error.half),
          x2: ex,
          y2: toY(error.mean - error.half),
          stroke: theme.series[1],
          strokeWidth: 1.5,
          title,
        }),
        microscope.line({
          x1: ex - 4,
          y1: toY(error.mean + error.half),
          x2: ex + 4,
          y2: toY(error.mean + error.half),
          stroke: theme.series[1],
          strokeWidth: 1.5,
        }),
        microscope.line({
          x1: ex - 4,
          y1: toY(error.mean - error.half),
          x2: ex + 4,
          y2: toY(error.mean - error.half),
          stroke: theme.series[1],
          strokeWidth: 1.5,
        }),
        microscope.circle({
          x: ex,
          y: toY(error.mean),
          r: 3,
          fill: theme.series[1],
          stroke: theme.surface,
          strokeWidth: 1.5,
          title,
        }),
      );
    }
    const middle = LEFT + slot * k + slot / 2;
    shapes.push(
      microscope.text({
        x: middle,
        y: bottom + 15,
        text: truncate(group.name, slot),
        size: 11,
        anchor: "middle",
        fill: theme.ink2,
        title: group.name,
      }),
    );
    if (stats.n !== null) {
      shapes.push(
        microscope.text({
          x: middle,
          y: bottom + 28,
          text: `n=${stats.n}`,
          size: 10,
          anchor: "middle",
          fill: theme.ink3,
        }),
      );
    }
  });
  if (errors.some((error) => error !== null)) {
    shapes.push(
      microscope.line({
        x1: right - 110,
        y1: 10,
        x2: right - 98,
        y2: 10,
        stroke: theme.series[1],
        strokeWidth: 2,
      }),
      microscope.text({
        x: right - 94,
        y: 10,
        text: `mean ${errors.label}`,
        size: 11,
        baseline: "middle",
        fill: theme.ink2,
      }),
    );
  }
  const caption = [`${groups.length} ${groups.length === 1 ? "group" : "groups"}`];
  if (groups.some((group) => group.stats.n !== null))
    caption.push("quartiles: linear (type 7)", "whiskers: 1.5 × IQR");
  caption.push(
    `${outliers} ${outliers === 1 ? "outlier" : "outliers"}${hidden > 0 ? `, ${hidden} counted but not drawn` : ""}`,
  );
  const odd = groups.reduce((total, group) => total + group.stats.odd, 0);
  if (odd > 0) caption.push(`${odd} NaN or ±∞ left out`);
  if (before.size > 0) caption.push("tick = median at the stop before");
  return microscope.picture({ width: W, height: HEIGHT, shapes, caption: caption.join(" · ") });
});

/** Each group's name, its statistics, and the part it is. */
function groupsOf(value, path, labels) {
  /** @type {[any, any][]} */
  let pairs;
  let keyed = false;
  if (Array.isArray(value) && value.length > 0 && value.every((item) => isEntry(item))) {
    pairs = value;
    keyed = true;
  } else if (Array.isArray(value)) {
    if (labels !== undefined && labels !== null && labels.length !== value.length) {
      throw new RangeError(
        `\`labels\` holds ${labels.length}, and there are ${value.length} groups`,
      );
    }
    pairs = value.map((group, index) => [
      labels === undefined || labels === null ? `[${index}]` : nameOf(labels[index]),
      group,
    ]);
  } else if (isRecord(value)) {
    pairs = Object.entries(value);
  } else {
    throw new TypeError("box-plot takes `groups`: a map, record, or array of samples or summaries");
  }
  return pairs.map(([key, group], index) => ({
    name: nameOf(key),
    stats: statsOf(group, nameOf(key)),
    select: partOf(path, keyed, Array.isArray(value), key, index),
  }));
}

/** The part of the drawn value a group is: an array's element or a
 * record's member, never a map's entry, which is no member. */
function partOf(path, keyed, array, key, index) {
  if (path === null || path === undefined || keyed) return;
  if (array) return `${path}[${index}]`;
  if (!/^[A-Za-z_]\w*$/u.test(key)) return;
  return path === "" ? key : `${path}.${key}`;
}

/** Whether `entry` is a map's [key, group] pair. */
function isEntry(entry) {
  return (
    Array.isArray(entry) &&
    entry.length === 2 &&
    !isSequence(entry[0]) &&
    (isSequence(entry[1]) || isRecord(entry[1]))
  );
}

function isSequence(value) {
  return Array.isArray(value) || (ArrayBuffer.isView(value) && !(value instanceof DataView));
}

function isRecord(value) {
  return typeof value === "object" && value !== null && !isSequence(value);
}

/** A group's quartiles, whiskers, and outliers, from its samples or as given. */
function statsOf(group, name) {
  if (isRecord(group)) {
    const read = (member) => {
      const v = group[member];
      if (typeof v !== "number" && typeof v !== "bigint") {
        throw new TypeError(`group ${name}'s summary needs \`${member}\`, a number`);
      }
      return Number(v);
    };
    const [min, q1, median, q3, max] = ["min", "q1", "median", "q3", "max"].map((member) =>
      read(member),
    );
    return {
      n: null,
      min,
      q1,
      median,
      q3,
      max,
      low: min,
      high: max,
      outliers: [],
      odd: 0,
      mean: Number.NaN,
      sd: Number.NaN,
    };
  }
  const raw = numbers(group, `group ${name}`);
  const samples = new Float64Array(raw.length);
  let n = 0;
  let odd = 0;
  let total = 0;
  for (let i = 0; i < raw.length; i++) {
    if (Number.isFinite(raw[i])) {
      samples[n++] = raw[i];
      total += raw[i];
    } else {
      odd += 1;
    }
  }
  if (n === 0) throw new RangeError(`group ${name} holds no finite samples`);
  const work = samples.subarray(0, n);
  const q1 = quantile(work, 0.25);
  const median = quantile(work, 0.5);
  const q3 = quantile(work, 0.75);
  const iqr = q3 - q1;
  let min = Number.POSITIVE_INFINITY;
  let max = Number.NEGATIVE_INFINITY;
  let low = Number.POSITIVE_INFINITY;
  let high = Number.NEGATIVE_INFINITY;
  const outliers = [];
  const mean = total / n;
  let squares = 0;
  for (const v of work) {
    min = Math.min(min, v);
    max = Math.max(max, v);
    squares += (v - mean) ** 2;
    if (v < q1 - 1.5 * iqr || v > q3 + 1.5 * iqr) {
      outliers.push(v);
    } else {
      low = Math.min(low, v);
      high = Math.max(high, v);
    }
  }
  return {
    n,
    min,
    q1,
    median,
    q3,
    max,
    low,
    high,
    outliers,
    odd,
    mean,
    sd: n > 1 ? Math.sqrt(squares / (n - 1)) : 0,
  };
}

/** Each group's error bar, if any, and what the bars are. */
function errorsOf(input, groups) {
  const kind = input.error;
  /** @type {({ mean: number, half: number } | null)[] & { label?: string }} */
  let errors = groups.map(() => null);
  let named = "";
  if (kind === "sd" || kind === "ci95") {
    errors = groups.map((group) => {
      if (group.stats.n === null)
        throw new TypeError(
          `\`error = "${kind}"\` needs samples, and group ${group.name} is a summary`,
        );
      const half =
        kind === "sd" ? group.stats.sd : (1.96 * group.stats.sd) / Math.sqrt(group.stats.n);
      return { mean: group.stats.mean, half };
    });
    named = kind === "sd" ? "±1 sd" : "95% CI";
  } else if (kind !== undefined && kind !== null) {
    const halves = numbers(kind, "error");
    const means = numbers(input.mean, "mean");
    if (halves.length !== groups.length || means.length !== groups.length) {
      throw new RangeError("`mean` and `error` hold one number per group");
    }
    errors = groups.map((_, k) => ({ mean: means[k], half: halves[k] }));
    named = "± error";
  }
  return Object.assign(errors, {
    label: typeof input.error_label === "string" ? input.error_label : named,
  });
}

/** The p-quantile by selection, linear between order statistics (type 7);
 * it reorders `values`. */
function quantile(values, p) {
  const n = values.length;
  const h = (n - 1) * p;
  const lo = Math.floor(h);
  const below = select(values, lo);
  if (lo + 1 >= n) return below;
  let above = Number.POSITIVE_INFINITY;
  for (let i = lo + 1; i < n; i++) if (values[i] < above) above = values[i];
  return below + (h - lo) * (above - below);
}

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
      throw new TypeError(`box-plot draws numbers, and \`${what}[${index}]\` is not one`);
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

function exact(v) {
  return Object.is(v, -0) ? "-0" : String(v);
}

function short(v) {
  return Number(v.toPrecision(4)).toString();
}

function truncate(text, room) {
  const fits = Math.max(2, Math.floor(room / 6.4));
  return text.length > fits ? `${text.slice(0, fits - 1)}…` : text;
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
  return short(v);
}
