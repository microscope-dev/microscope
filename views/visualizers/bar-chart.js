// @ts-check
// The built-in bar-chart: a bar per value, from a zero baseline.
//
//   values       numbers, with labels, or a map or record of labels to numbers
//   labels       optional, one per value
//   entries      a map or record of labels to numbers, as values may be
//   orientation  optional: "horizontal" (the default) or "vertical"
//   sort         optional: "value", "label", or "none"
//
// A map sorts by value, since a map's order may be random, and an array
// keeps the program's order. Past 40 sorted entries, the rest fold into
// one "Other" bar that says how many and their total. Negative values
// extend the other way from zero. A tick marks each bar's length at the
// stop before.

const MOST = 40;
const MOST_IN_ORDER = 400;

microscope.draw((input, { previous, width, paths }) => {
  const theme = microscope.theme;
  const { bars, keyed } = barsOf(input, paths);
  const before = new Map();
  if (previous !== null) {
    try {
      for (const bar of barsOf(previous, {}).bars)
        before.set(keyed ? bar.label : bar.index, bar.value);
    } catch {
      // The stop before had nothing this chart reads.
    }
  }
  const order = input.sort ?? (keyed ? "value" : "none");
  if (order !== "value" && order !== "label" && order !== "none") {
    throw new TypeError('`sort` is "value", "label", or "none"');
  }
  const sorted = bars.slice();
  if (order === "value") sorted.sort((a, b) => b.value - a.value || compare(a.label, b.label));
  if (order === "label") sorted.sort((a, b) => compare(a.label, b.label));

  // Sorted bars fold past the top 40; bars in the program's order are cut,
  // and the caption says so.
  let shown = sorted;
  /** @type {{ count: number, total: number | bigint } | null} */
  let other = null;
  let cut = 0;
  if (order === "value" && sorted.length > MOST) {
    shown = sorted.slice(0, MOST);
    const rest = sorted.slice(MOST);
    other = { count: rest.length, total: sum(rest.map((bar) => bar.raw)) };
  } else if (sorted.length > MOST_IN_ORDER) {
    shown = sorted.slice(0, MOST_IN_ORDER);
    cut = sorted.length - MOST_IN_ORDER;
  }

  const finite = shown.filter((bar) => Number.isFinite(bar.value));
  let lo = Math.min(
    0,
    ...finite.map((bar) => bar.value),
    ...[...before.values()].filter((value) => Number.isFinite(value)),
  );
  let hi = Math.max(
    0,
    ...finite.map((bar) => bar.value),
    ...[...before.values()].filter((value) => Number.isFinite(value)),
  );
  if (other !== null) hi = Math.max(hi, Number(other.total));
  if (lo === hi) hi = 1;
  const axis = nice(lo, hi);
  const vertical = (input.orientation ?? "horizontal") === "vertical";
  if (!vertical && input.orientation !== undefined && input.orientation !== "horizontal") {
    throw new TypeError('`orientation` is "horizontal" or "vertical"');
  }
  const rows = shown.length + (other === null ? 0 : 1);
  const W = Math.round(Math.min(Math.max(width, 320), 1600));
  const shapes = [];
  const color = theme.series[0];

  if (!vertical) {
    const labelWidth = Math.min(
      180,
      Math.max(48, ...shown.map((bar) => bar.label.length * 6.4 + 8)),
    );
    const left = labelWidth + 8;
    const right = W - 72;
    const ROW = 22;
    const H = 30 + rows * ROW;
    const toX = (v) => left + ((v - axis.lo) / (axis.hi - axis.lo)) * (right - left);
    for (const tick of axis.ticks) {
      shapes.push(
        microscope.line({
          x1: toX(tick),
          y1: 4,
          x2: toX(tick),
          y2: H - 22,
          stroke: tick === 0 ? theme.ink3 : theme.line,
        }),
        microscope.text({
          x: toX(tick),
          y: H - 8,
          text: label(tick),
          size: 11,
          anchor: "middle",
          fill: theme.ink3,
        }),
      );
    }
    const row = (k, bar, fill, title, select) => {
      const y = 8 + k * ROW;
      const zero = toX(0);
      const end = Number.isFinite(bar.value) ? toX(bar.value) : zero;
      shapes.push(
        microscope.rect({
          x: Math.min(zero, end),
          y,
          width: Math.max(1, Math.abs(end - zero)),
          height: 14,
          radius: 2,
          fill,
          title,
          select,
        }),
        microscope.text({
          x: left - 8,
          y: y + 7,
          text: truncate(bar.label, labelWidth),
          size: 11,
          anchor: "end",
          baseline: "middle",
          fill: theme.ink2,
          title: bar.label,
        }),
        microscope.text({
          x: bar.value < 0 ? Math.min(zero, end) - 5 : Math.max(zero, end) + 5,
          y: y + 7,
          text: bar.text,
          size: 11,
          family: "mono",
          anchor: bar.value < 0 ? "end" : "start",
          baseline: "middle",
          fill: theme.ink,
        }),
      );
      const was = before.get(bar.key);
      if (was !== undefined && Number.isFinite(was) && was !== bar.value) {
        shapes.push(
          microscope.line({
            x1: toX(was),
            y1: y - 2,
            x2: toX(was),
            y2: y + 16,
            stroke: theme.ink,
            strokeWidth: 1.5,
            title: `${bar.label} at the stop before: ${was}`,
          }),
        );
      }
    };
    shown.forEach((bar, k) => row(k, bar, color, titleOf(bar), bar.select));
    if (other !== null) {
      const total = other.total;
      row(
        shown.length,
        {
          label: `Other (${other.count} more)`,
          value: Number(total),
          text: String(total),
          key: null,
        },
        theme.ink3,
        `Other: ${other.count} more entries, total ${total}`,
      );
    }
    return microscope.picture({ width: W, height: H, shapes, caption: captionOf() });
  }

  const left = 56;
  const right = W - 12;
  const top = 18;
  const bottom = 236;
  const H = 262;
  const slot = (right - left) / Math.max(1, rows);
  const thickness = Math.max(1, Math.min(24, slot - 2));
  const toY = (v) => bottom - ((v - axis.lo) / (axis.hi - axis.lo)) * (bottom - top);
  for (const tick of axis.ticks) {
    shapes.push(
      microscope.line({
        x1: left,
        y1: toY(tick),
        x2: right,
        y2: toY(tick),
        stroke: tick === 0 ? theme.ink3 : theme.line,
      }),
      microscope.text({
        x: left - 6,
        y: toY(tick),
        text: label(tick),
        size: 11,
        anchor: "end",
        baseline: "middle",
        fill: theme.ink3,
      }),
    );
  }
  const every = Math.max(1, Math.ceil(rows / Math.max(1, Math.floor((right - left) / 44))));
  const column = (k, item, fill, title, select) => {
    const x = left + k * slot + (slot - thickness) / 2;
    const zero = toY(0);
    const end = Number.isFinite(item.value) ? toY(item.value) : zero;
    shapes.push(
      microscope.rect({
        x,
        y: Math.min(zero, end),
        width: thickness,
        height: Math.max(1, Math.abs(end - zero)),
        radius: Math.min(2, thickness / 2),
        fill,
        title,
        select,
      }),
    );
    if (k % every === 0) {
      shapes.push(
        microscope.text({
          x: x + thickness / 2,
          y: bottom + 14,
          text: truncate(item.label, slot * every),
          size: 11,
          anchor: "middle",
          fill: theme.ink3,
          title: item.label,
        }),
      );
    }
    const was = before.get(item.key);
    if (was !== undefined && Number.isFinite(was) && was !== item.value) {
      shapes.push(
        microscope.line({
          x1: x - 2,
          y1: toY(was),
          x2: x + thickness + 2,
          y2: toY(was),
          stroke: theme.ink,
          strokeWidth: 1.5,
          title: `${item.label} at the stop before: ${was}`,
        }),
      );
    }
  };
  shown.forEach((item, k) => column(k, item, color, titleOf(item), item.select));
  if (other !== null) {
    column(
      shown.length,
      { label: "Other", value: Number(other.total), key: null },
      theme.ink3,
      `Other: ${other.count} more entries, total ${other.total}`,
    );
  }
  // The largest bar's value, over it.
  const largest = finite.reduce((best, item) => (item.value > best.value ? item : best), finite[0]);
  if (largest !== undefined && largest.value > 0) {
    const k = shown.indexOf(largest);
    shapes.push(
      microscope.text({
        x: left + k * slot + slot / 2,
        y: toY(largest.value) - 5,
        text: largest.text,
        size: 11,
        family: "mono",
        anchor: "middle",
        fill: theme.ink,
      }),
    );
  }
  return microscope.picture({ width: W, height: H, shapes, caption: captionOf() });

  function captionOf() {
    const parts = [
      `${bars.length} ${bars.length === 1 ? "entry" : "entries"}`,
      `total ${sum(bars.map((item) => item.raw))}`,
    ];
    const counted = bars.filter((item) => Number.isFinite(item.value));
    const most = counted.reduce(
      (best, item) => (item.value > best.value ? item : best),
      counted[0],
    );
    if (most !== undefined) parts.push(`max ${most.text} at ${most.label}`);
    const negative = bars.filter((item) => item.value < 0).length;
    if (negative > 0) parts.push(`${negative} negative`);
    const odd = bars.filter((item) => !Number.isFinite(item.value)).length;
    if (odd > 0) parts.push(`${odd} NaN or ±∞, drawn as nothing`);
    parts.push(
      order === "value"
        ? keyed
          ? "sorted by value (a map's order may be random)"
          : "sorted by value"
        : order === "label"
          ? "sorted by label"
          : "in the program's order",
    );
    if (other !== null)
      parts.push(`top ${MOST} + Other (${other.count} more, total ${other.total})`);
    if (cut > 0) parts.push(`the first ${MOST_IN_ORDER}; ${cut} more in the Table`);
    if (before.size > 0) parts.push("| = the stop before");
    return parts.join(" · ");
  }
});

function titleOf(item) {
  return `${item.label}: ${item.text}`;
}

/** The bars: each value with its label, exact text, and the part it is. */
function barsOf(input, paths) {
  const source = input.entries ?? input.values;
  if (source === undefined || source === null) {
    throw new TypeError("bar-chart takes `values`, or `entries`: labels and their numbers");
  }
  const path = input.entries === undefined ? paths.values : paths.entries;
  const pairs = asEntries(source);
  if (pairs !== null) {
    return {
      keyed: true,
      bars: pairs.map(([key, value], index) =>
        barOf(nameOf(key), value, index, `entries[${index}]`, nameOf(key)),
      ),
    };
  }
  const values = sequence(source, input.entries === undefined ? "values" : "entries");
  const labels =
    input.labels === undefined || input.labels === null ? null : sequence(input.labels, "labels");
  if (labels !== null && labels.length !== values.length) {
    throw new RangeError(
      `\`labels\` holds ${labels.length}, and the values number ${values.length}`,
    );
  }
  return {
    keyed: false,
    bars: Array.from(values, (value, index) =>
      barOf(
        labels === null ? `[${index}]` : nameOf(labels[index]),
        value,
        index,
        `values[${index}]`,
        index,
        path === null || path === undefined ? undefined : `${path}[${index}]`,
      ),
    ),
  };
}

function barOf(name, value, index, what, key, select) {
  if (typeof value !== "number" && typeof value !== "bigint") {
    throw new TypeError(`bar-chart draws numbers, and \`${what}\` is ${describe(value)}`);
  }
  return { label: name, value: Number(value), raw: value, text: exact(value), index, key, select };
}

/** A map's [key, value] pairs, or a record's members, or null. */
function asEntries(value) {
  if (
    Array.isArray(value) &&
    value.length > 0 &&
    value.every((entry) => Array.isArray(entry) && entry.length === 2)
  ) {
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
  return null;
}

/** @returns {ArrayLike<any>} */
function sequence(value, what) {
  if (Array.isArray(value) || (ArrayBuffer.isView(value) && !(value instanceof DataView))) {
    // oxlint-disable-next-line no-inline-comments -- a JSDoc type cast has to sit inline
    return /** @type {ArrayLike<any>} */ (/** @type {unknown} */ (value));
  }
  throw new TypeError(`\`${what}\` must be a sequence, not ${describe(value)}`);
}

function nameOf(key) {
  if (typeof key === "string") return key;
  if (typeof key === "object" && key !== null) {
    if (typeof key.name === "string") return key.name;
    if (typeof key.variant === "string") return key.variant;
  }
  return String(key);
}

function describe(value) {
  if (value === null) return "null";
  if (Array.isArray(value)) return "an array";
  return typeof value === "object" ? "a record" : `a ${typeof value}`;
}

function exact(v) {
  return Object.is(v, -0) ? "-0" : String(v);
}

/** The exact total: whole when every value is an integer, else a double. */
function sum(values) {
  if (values.every((v) => typeof v === "bigint" || Number.isSafeInteger(v))) {
    return values.reduce((total, v) => total + BigInt(v), 0n);
  }
  return values.reduce((total, v) => total + Number(v), 0);
}

function compare(a, b) {
  return a < b ? -1 : a > b ? 1 : 0;
}

function truncate(text, room) {
  const fits = Math.max(2, Math.floor(room / 6.4));
  return text.length > fits ? `${text.slice(0, fits - 1)}…` : text;
}

function nice(lo, hi, count = 5) {
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
