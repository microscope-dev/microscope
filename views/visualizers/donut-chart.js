// @ts-check
// The built-in donut-chart: each value's share of the total.
//
//   values   numbers, with labels, or a map or record of labels to numbers
//   labels   optional, one per value
//   entries  a map or record of labels to numbers, as values may be
//
// Slices run largest first, clockwise from twelve o'clock, with gaps
// between them; past seven, the rest fold into "Other". Each slice is
// labelled outside with its value and share when it has room, and titled
// with both and how its share changed since the stop before. A negative or
// non-finite value is a problem, never a wedge: a bar chart shows those.

const HEIGHT = 260;
const MOST = 7;
const GAP = 2;

microscope.draw((input, { previous, width, paths }) => {
  const theme = microscope.theme;
  const slices = slicesOf(input, paths);
  const shares = new Map();
  if (previous !== null) {
    try {
      const before = slicesOf(previous, {});
      const total = before.reduce((sum, slice) => sum + slice.value, 0);
      if (total > 0) for (const slice of before) shares.set(slice.label, slice.value / total);
    } catch {
      // The stop before had nothing this chart reads.
    }
  }
  const total = slices.reduce((sum, slice) => sum + slice.value, 0);
  const sorted = slices.slice().sort((a, b) => b.value - a.value || (a.label < b.label ? -1 : 1));
  const drawn = sorted.slice(0, sorted.length > MOST + 1 ? MOST : MOST + 1);
  const rest = sorted.slice(drawn.length);
  // Hues follow names: the drawn labels in name order.
  const names = drawn.map((slice) => slice.label).sort();
  const W = Math.round(Math.min(Math.max(width, 320), 1200));
  const cx = W / 2;
  const cy = HEIGHT / 2;
  const outer = Math.min(92, HEIGHT / 2 - 34);
  const inner = outer * 0.62;
  const shapes = [];
  const all =
    rest.length > 0
      ? [
          ...drawn,
          {
            label: `Other (${rest.length})`,
            value: rest.reduce((sum, slice) => sum + slice.value, 0),
            text: exactSum(rest),
            select: undefined,
            other: true,
          },
        ]
      : drawn;

  let angle = -Math.PI / 2;
  const labels = [];
  for (const slice of all) {
    const share = total > 0 ? slice.value / total : 0;
    const sweep = share * Math.PI * 2;
    const start = angle;
    const end = angle + sweep;
    angle = end;
    if (slice.value <= 0) continue;
    // A gap of GAP pixels at the outer edge, less at the inner.
    const pad = Math.min(sweep / 4, GAP / 2 / outer);
    const padIn = Math.min(sweep / 4, GAP / 2 / inner);
    const was = shares.get(slice.label);
    const change = was === undefined ? "" : `\n${points(share - was)} since the stop before`;
    const isOther = "other" in slice;
    const fill = isOther ? theme.ink3 : theme.series[names.indexOf(slice.label)];
    const title = `${slice.label}: ${slice.text} · ${percent(share)}${change}`;
    if (sorted.length === 1) {
      shapes.push(
        microscope.path({ d: ring(cx, cy, outer, inner), fill, title, select: slice.select }),
      );
    } else {
      shapes.push(
        microscope.path({
          d: wedge(cx, cy, outer, inner, start + pad, end - pad, start + padIn, end - padIn),
          fill,
          title,
          select: slice.select,
        }),
      );
    }
    if (share >= 0.04) labels.push({ slice, share, middle: (start + end) / 2 });
  }
  // Labels outside, each side's stacked so none overlap.
  for (const side of [1, -1]) {
    const here = labels
      .filter((item) => (Math.cos(item.middle) >= 0 ? 1 : -1) === side)
      .map((item) => ({ ...item, y: cy + (outer + 14) * Math.sin(item.middle) }));
    here.sort((a, b) => a.y - b.y);
    for (let k = 1; k < here.length; k++) here[k].y = Math.max(here[k].y, here[k - 1].y + 26);
    for (const item of here) {
      const x = cx + (outer + 14) * Math.cos(item.middle);
      const anchor = side > 0 ? "start" : "end";
      shapes.push(
        microscope.line({
          x1: cx + (outer + 2) * Math.cos(item.middle),
          y1: cy + (outer + 2) * Math.sin(item.middle),
          x2: x - side * 3,
          y2: item.y,
          stroke: theme.ink3,
        }),
        microscope.text({
          x,
          y: item.y - 2,
          text: truncate(item.slice.label, W / 2 - outer - 30),
          size: 11,
          anchor,
          fill: theme.ink2,
          title: item.slice.label,
        }),
        microscope.text({
          x,
          y: item.y + 11,
          text: `${item.slice.text} · ${percent(item.share)}`,
          size: 11,
          family: "mono",
          anchor,
          fill: theme.ink,
        }),
      );
    }
  }
  shapes.push(
    microscope.text({
      x: cx,
      y: cy - 2,
      text: exactSum(slices),
      size: 15,
      family: "mono",
      anchor: "middle",
      fill: theme.ink,
      title: `total ${exactSum(slices)}`,
    }),
    microscope.text({
      x: cx,
      y: cy + 14,
      text: "total",
      size: 11,
      anchor: "middle",
      fill: theme.ink3,
    }),
  );
  const caption = [
    `${slices.length} ${slices.length === 1 ? "slice" : "slices"}`,
    `total ${exactSum(slices)}`,
  ];
  if (rest.length > 0)
    caption.push(
      `${drawn.length} largest + Other (${rest.length}, ${percent(rest.reduce((sum, slice) => sum + slice.value, 0) / (total || 1))})`,
    );
  if (labels.length < all.filter((slice) => slice.value > 0).length)
    caption.push("slices under 4%: hover or the Table");
  if (shares.size > 0) caption.push("titles give each share's change");
  return microscope.picture({ width: W, height: HEIGHT, shapes, caption: caption.join(" · ") });
});

/** The slices: each value with its label and exact text. */
function slicesOf(input, paths) {
  const source = input.entries ?? input.values;
  if (source === undefined || source === null) {
    throw new TypeError("donut-chart takes `values`, or `entries`: labels and their numbers");
  }
  const path = input.entries !== undefined ? paths.entries : paths.values;
  const pairs = asEntries(source);
  const listed =
    pairs !== null
      ? pairs.map(([key, value]) => [nameOf(key), value, undefined])
      : Array.from(sequence(source), (value, index) => [
          input.labels === undefined || input.labels === null
            ? `[${index}]`
            : nameOf(input.labels[index]),
          value,
          path === null || path === undefined ? undefined : `${path}[${index}]`,
        ]);
  return listed.map(([label, value, select]) => {
    if (typeof value !== "number" && typeof value !== "bigint") {
      throw new TypeError(`donut-chart draws numbers, and ${label} is not one`);
    }
    const number = Number(value);
    if (!Number.isFinite(number) || number < 0) {
      throw new RangeError(
        `a donut shows shares, and ${label} is ${String(value)}; Draw as… bar-chart shows any number`,
      );
    }
    return { label: String(label), value: number, raw: value, text: String(value), select };
  });
}

/** An annular wedge from `a0` to `a1` outside and `b0` to `b1` inside. */
function wedge(cx, cy, outer, inner, a0, a1, b0, b1) {
  const point = (r, a) =>
    `${(cx + r * Math.cos(a)).toFixed(2)} ${(cy + r * Math.sin(a)).toFixed(2)}`;
  const large = a1 - a0 > Math.PI ? 1 : 0;
  const largeIn = b1 - b0 > Math.PI ? 1 : 0;
  return `M${point(outer, a0)} A${outer} ${outer} 0 ${large} 1 ${point(outer, a1)} L${point(inner, b1)} A${inner} ${inner} 0 ${largeIn} 0 ${point(inner, b0)} Z`;
}

/** A whole ring, for a single slice. */
function ring(cx, cy, outer, inner) {
  return `M${cx - outer} ${cy} A${outer} ${outer} 0 1 1 ${cx + outer} ${cy} A${outer} ${outer} 0 1 1 ${cx - outer} ${cy} Z M${cx - inner} ${cy} A${inner} ${inner} 0 1 0 ${cx + inner} ${cy} A${inner} ${inner} 0 1 0 ${cx - inner} ${cy} Z`;
}

function exactSum(slices) {
  if (slices.every((slice) => typeof slice.raw === "bigint" || Number.isSafeInteger(slice.raw))) {
    return String(slices.reduce((sum, slice) => sum + BigInt(slice.raw), 0n));
  }
  return String(Number(slices.reduce((sum, slice) => sum + slice.value, 0).toPrecision(6)));
}

function percent(share) {
  return `${Number((share * 100).toPrecision(3))}%`;
}

function points(change) {
  const value = Number((change * 100).toPrecision(3));
  return `${value > 0 ? "+" : ""}${value} points`;
}

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
function sequence(value) {
  if (Array.isArray(value) || (ArrayBuffer.isView(value) && !(value instanceof DataView))) {
    return /** @type {ArrayLike<any>} */ (/** @type {unknown} */ (value));
  }
  throw new TypeError("donut-chart's values must be a sequence, map, or record");
}

function nameOf(key) {
  if (typeof key === "string") return key;
  if (typeof key === "object" && key !== null) {
    if (typeof key.name === "string") return key.name;
    if (typeof key.variant === "string") return key.variant;
  }
  return String(key);
}

function truncate(text, room) {
  const fits = Math.max(2, Math.floor(room / 6.4));
  return text.length > fits ? `${text.slice(0, fits - 1)}…` : text;
}
