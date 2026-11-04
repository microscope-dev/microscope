// @ts-check
// The built-in heatmap: a grid of numbers as colors.
//
//   values         numbers row by row, with
//   columns        how many make a row; or values may be an array of rows
//   row_labels     optional, one per row
//   column_labels  optional, one per column
//   scale          optional: "sequential" or "diverging"; by default,
//                  diverging when the values cross zero
//   log            optional; true colors positive values by their logarithm
//
// A sequential scale is one blue, light to dark; a diverging one is blue
// below zero and red above, through gray at zero. A legend gives the ends
// and zero. NaN cells are hatched. Up to 10,000 cells, each is titled with
// its row, column, and value, and outlined when it changed since the stop
// before; past that, the grid is one image, where each pixel shows the
// largest of the cells it covers, and an overlay marks changed cells.

const MOST_CELLS = 10_000;

microscope.draw((input, { previous, width, paths, theme: mode }) => {
  const theme = microscope.theme;
  const grid = gridOf(input);
  const { rows, columns, values } = grid;
  const before = previous === null ? null : safeGrid(previous);
  const changed =
    before !== null && before.rows === rows && before.columns === columns ? before.values : null;
  const log = input.log === true;

  let lo = Number.POSITIVE_INFINITY;
  let hi = Number.NEGATIVE_INFINITY;
  let nans = 0;
  let infinite = 0;
  for (let i = 0; i < values.length; i++) {
    const v = values[i];
    if (Number.isNaN(v)) {
      nans += 1;
      continue;
    }
    if (!Number.isFinite(v)) {
      infinite += 1;
      continue;
    }
    if (log && v <= 0) continue;
    if (v < lo) lo = v;
    if (v > hi) hi = v;
  }
  if (lo > hi) {
    lo = 0;
    hi = 1;
  }
  const scale = input.scale ?? (lo < 0 && hi > 0 && !log ? "diverging" : "sequential");
  if (scale !== "sequential" && scale !== "diverging") {
    throw new TypeError('`scale` is "sequential" or "diverging"');
  }
  const dark = mode === "dark";
  const ramp = dark ? ["#1f2a3a", "#9ec3f2"] : ["#eef3fa", "#174a8b"];
  const diverging = dark ? ["#4f8fe0", "#3a3f47", "#e8615f"] : ["#2266c4", "#ececec", "#d23f3e"];
  const most = Math.max(Math.abs(lo), Math.abs(hi)) || 1;
  const color = (v) => {
    if (!Number.isFinite(v))
      return v > 0
        ? scale === "diverging"
          ? diverging[2]
          : ramp[1]
        : v < 0
          ? diverging[0]
          : theme.line;
    if (scale === "diverging") {
      return v < 0
        ? microscope.color.scale(-v / most, diverging[1], diverging[0])
        : microscope.color.scale(v / most, diverging[1], diverging[2]);
    }
    if (log)
      return v <= 0
        ? ramp[0]
        : microscope.color.scale(
            (Math.log10(v) - Math.log10(lo)) / (Math.log10(hi) - Math.log10(lo) || 1),
            ramp[0],
            ramp[1],
          );
    return microscope.color.scale((v - lo) / (hi - lo || 1), ramp[0], ramp[1]);
  };

  const rowLabels = labelsOf(input.row_labels, rows, "row_labels");
  const columnLabels = labelsOf(input.column_labels, columns, "column_labels");
  const W = Math.round(Math.min(Math.max(width, 320), 1600));
  const left =
    rowLabels === null
      ? 36
      : Math.min(120, 10 + Math.max(...rowLabels.map((name) => name.length)) * 6.4);
  const room = W - left - 12;
  const cell = Math.max(0.25, Math.min(28, room / columns, 480 / rows));
  const top = 8;
  const gridWidth = cell * columns;
  const gridHeight = cell * rows;
  const shapes = [];
  const part = (r, c) => {
    if (paths.values === null || paths.values === undefined) return undefined;
    return grid.nested ? `${paths.values}[${r}][${c}]` : `${paths.values}[${r * columns + c}]`;
  };
  let changes = 0;
  const imaged = rows * columns > MOST_CELLS;

  if (!imaged) {
    for (let r = 0; r < rows; r++) {
      for (let c = 0; c < columns; c++) {
        const i = r * columns + c;
        const v = values[i];
        const x = left + c * cell;
        const y = top + r * cell;
        const name = `${rowLabels?.[r] ?? `[${r}]`}${columnLabels !== null ? ` ${columnLabels[c]}` : `[${c}]`}`;
        const title = `${name}: ${exact(grid, i)}`;
        const inset = cell > 4 ? 0.5 : 0;
        if (Number.isNaN(v)) {
          shapes.push(
            microscope.rect({
              x: x + inset,
              y: y + inset,
              width: cell - inset * 2,
              height: cell - inset * 2,
              fill: theme.line,
              title,
              select: part(r, c),
            }),
          );
          shapes.push(
            microscope.line({
              x1: x + inset,
              y1: y + cell - inset,
              x2: x + cell - inset,
              y2: y + inset,
              stroke: theme.ink3,
            }),
          );
        } else {
          shapes.push(
            microscope.rect({
              x: x + inset,
              y: y + inset,
              width: cell - inset * 2,
              height: cell - inset * 2,
              fill: color(v),
              title,
              select: part(r, c),
            }),
          );
        }
        if (changed !== null && !Object.is(changed[i], v)) {
          changes += 1;
          shapes.push(
            microscope.rect({
              x: x + 0.75,
              y: y + 0.75,
              width: Math.max(0.5, cell - 1.5),
              height: Math.max(0.5, cell - 1.5),
              fill: "none",
              stroke: theme.ink,
              strokeWidth: 1.5,
            }),
          );
        }
      }
    }
  } else {
    // One pixel a cell, or each pixel the largest of the cells it covers.
    const across = Math.min(columns, Math.max(1, Math.floor(gridWidth)));
    const down = Math.min(rows, Math.max(1, Math.floor(gridHeight)));
    const pixels = new Uint8ClampedArray(across * down * 4);
    const overlay = changed === null ? null : new Uint8ClampedArray(across * down * 4);
    const largest = new Float64Array(across * down).fill(Number.NaN);
    const strongest = new Float64Array(across * down).fill(-1);
    const moved = new Uint8Array(across * down);
    for (let r = 0; r < rows; r++) {
      const pr = Math.min(down - 1, Math.floor((r * down) / rows));
      for (let c = 0; c < columns; c++) {
        const pc = Math.min(across - 1, Math.floor((c * across) / columns));
        const p = pr * across + pc;
        const i = r * columns + c;
        const v = values[i];
        // The largest magnitude keeps its sign, so hot spots of either kind survive.
        if (!Number.isNaN(v) && Math.abs(v) > strongest[p]) {
          strongest[p] = Math.abs(v);
          largest[p] = v;
        }
        if (changed !== null && !Object.is(changed[i], v)) {
          changes += 1;
          moved[p] = 1;
        }
      }
    }
    const [hr, hg, hb] = rgb(theme.line);
    for (let p = 0; p < largest.length; p++) {
      const v = largest[p];
      if (Number.isNaN(v)) {
        // Hatched: every other diagonal.
        const x = p % across;
        const y = Math.floor(p / across);
        const shade = (x + y) % 4 < 2 ? 255 : 200;
        pixels.set(
          [
            Math.round((hr * shade) / 255),
            Math.round((hg * shade) / 255),
            Math.round((hb * shade) / 255),
            255,
          ],
          p * 4,
        );
      } else {
        pixels.set([...rgb(color(v)), 255], p * 4);
      }
      if (overlay !== null && moved[p]) overlay.set([...rgb(theme.ink), 200], p * 4);
    }
    shapes.push(
      microscope.image({
        x: left,
        y: top,
        width: gridWidth,
        height: gridHeight,
        pixels,
        columns: across,
        rows: down,
        title: across < columns || down < rows ? "each pixel the largest of its cells" : "cells",
      }),
    );
    if (overlay !== null && changes > 0) {
      shapes.push(
        microscope.image({
          x: left,
          y: top,
          width: gridWidth,
          height: gridHeight,
          pixels: overlay,
          columns: across,
          rows: down,
        }),
      );
    }
  }

  // Row and column labels, as many as fit.
  const everyRow = roundStep(13 / cell);
  for (let r = 0; r < rows; r += everyRow) {
    shapes.push(
      microscope.text({
        x: left - 4,
        y: top + r * cell + cell / 2,
        text: truncate(rowLabels?.[r] ?? String(r), left - 6),
        size: 10,
        anchor: "end",
        baseline: "middle",
        fill: theme.ink3,
      }),
    );
  }
  const everyColumn = roundStep(
    ((columnLabels === null
      ? String(columns).length
      : Math.max(...columnLabels.map((name) => name.length))) *
      6 +
      10) /
      cell,
  );
  for (let c = 0; c < columns; c += everyColumn) {
    shapes.push(
      microscope.text({
        x: left + c * cell + cell / 2,
        y: top + gridHeight + 12,
        text: columnLabels?.[c] ?? String(c),
        size: 10,
        anchor: "middle",
        fill: theme.ink3,
      }),
    );
  }

  // The legend: the ramp as a smooth image, with its ends and zero.
  const ly = top + gridHeight + 24;
  const lw = Math.min(200, Math.max(100, gridWidth / 2));
  const steps = 64;
  const ramped = new Uint8ClampedArray(steps * 4);
  for (let k = 0; k < steps; k++) {
    const t = k / (steps - 1);
    const v =
      scale === "diverging"
        ? -most + t * 2 * most
        : log
          ? 10 ** (Math.log10(lo) + t * (Math.log10(hi) - Math.log10(lo)))
          : lo + t * (hi - lo);
    ramped.set([...rgb(color(v)), 255], k * 4);
  }
  shapes.push(
    microscope.image({
      x: left,
      y: ly,
      width: lw,
      height: 8,
      pixels: ramped,
      columns: steps,
      rows: 1,
      smooth: true,
    }),
  );
  const ends =
    scale === "diverging"
      ? [
          [0, `-${label(most)}`],
          [0.5, "0"],
          [1, label(most)],
        ]
      : [
          [0, label(lo)],
          [1, label(hi)],
        ];
  for (const [t, text] of ends) {
    shapes.push(
      microscope.text({
        x: left + Number(t) * lw,
        y: ly + 20,
        text: String(text),
        size: 10,
        anchor: t === 0 ? "start" : t === 1 ? "end" : "middle",
        fill: theme.ink3,
      }),
    );
  }
  let lx = left + lw + 16;
  if (nans > 0) {
    shapes.push(
      microscope.rect({ x: lx, y: ly, width: 10, height: 8, fill: theme.line }),
      microscope.line({ x1: lx, y1: ly + 8, x2: lx + 10, y2: ly, stroke: theme.ink3 }),
      microscope.text({ x: lx + 14, y: ly + 7, text: "NaN", size: 10, fill: theme.ink3 }),
    );
    lx += 48;
  }
  if (changes > 0) {
    shapes.push(
      microscope.rect({
        x: lx,
        y: ly - 0.5,
        width: 10,
        height: 9,
        fill: "none",
        stroke: theme.ink,
        strokeWidth: 1.5,
      }),
      microscope.text({ x: lx + 14, y: ly + 7, text: "changed", size: 10, fill: theme.ink3 }),
    );
  }

  const caption = [`${rows} × ${columns} cells`];
  caption.push(
    scale === "diverging" ? "diverging around 0" : log ? "sequential, log" : "sequential",
  );
  caption.push(`min ${label(lo)}`, `max ${label(hi)}`);
  if (nans > 0) caption.push(`${nans} NaN, hatched`);
  if (infinite > 0) caption.push(`${infinite} ±∞, at the ramp's ends`);
  if (imaged && (Math.floor(gridWidth) < columns || Math.floor(gridHeight) < rows))
    caption.push("each pixel the largest of its cells");
  if (changed !== null)
    caption.push(
      changes === 0
        ? "unchanged since the stop before"
        : `${changes} changed since the stop before`,
    );
  return microscope.picture({
    width: W,
    height: Math.ceil(ly + 30),
    shapes,
    caption: caption.join(" · "),
  });
});

/** The grid's numbers row by row, its size, and its exact values. */
function gridOf(input) {
  const { values } = input;
  if (values === undefined || values === null) {
    throw new TypeError("heatmap takes `values`: numbers with `columns`, or an array of rows");
  }
  if (
    Array.isArray(values) &&
    values.length > 0 &&
    values.every((row) => Array.isArray(row) || ArrayBuffer.isView(row))
  ) {
    const lines = /** @type {ArrayLike<any>[]} */ (values);
    const columns = lines[0].length;
    const flat = new Float64Array(lines.length * columns);
    /** @type {ArrayLike<any>[]} */
    const raw = [];
    lines.forEach((row, r) => {
      if (row.length !== columns)
        throw new RangeError(`row ${r} holds ${row.length}, and row 0 holds ${columns}`);
      const numbers = toNumbers(row, `values[${r}]`);
      flat.set(numbers, r * columns);
      raw.push(row);
    });
    return {
      rows: values.length,
      columns,
      values: flat,
      nested: true,
      raw: (i) => raw[Math.floor(i / columns)][i % columns],
    };
  }
  const flat = toNumbers(values, "values");
  const columns = Number(input.columns);
  if (!Number.isInteger(columns) || columns < 1) {
    throw new TypeError("`columns` must be a positive whole number when `values` is not rows");
  }
  if (flat.length % columns !== 0) {
    throw new RangeError(`\`values\` holds ${flat.length}, which is not rows of ${columns}`);
  }
  return {
    rows: flat.length / columns,
    columns,
    values: flat,
    nested: false,
    raw: (i) => values[i],
  };
}

function safeGrid(previous) {
  try {
    return gridOf(previous);
  } catch {
    return null;
  }
}

function exact(grid, i) {
  const held = grid.raw(i);
  if (typeof held === "bigint") return String(held);
  const v = grid.values[i];
  if (Object.is(v, -0)) return "-0";
  return String(v);
}

function labelsOf(value, count, what) {
  if (value === undefined || value === null) return null;
  const names = Array.from(value, (item) =>
    typeof item === "object" && item !== null && typeof item.name === "string"
      ? item.name
      : String(item),
  );
  if (names.length !== count)
    throw new RangeError(`\`${what}\` holds ${names.length}, and there are ${count}`);
  return names;
}

/** @returns {ArrayLike<number>} */
function toNumbers(value, what) {
  if (value instanceof BigInt64Array || value instanceof BigUint64Array) {
    return Float64Array.from(value, Number);
  }
  if (ArrayBuffer.isView(value) && !(value instanceof DataView)) {
    return /** @type {ArrayLike<number>} */ (/** @type {unknown} */ (value));
  }
  if (Array.isArray(value)) {
    return Float64Array.from(value, (item, index) => {
      if (typeof item === "number") return item;
      if (typeof item === "bigint") return Number(item);
      throw new TypeError(`heatmap draws numbers, and \`${what}[${index}]\` is not one`);
    });
  }
  throw new TypeError(`\`${what}\` must be numbers`);
}

function rgb(color) {
  const hex = /^#([0-9a-f]{6})$/i.exec(color)?.[1];
  if (hex === undefined) return [128, 128, 128];
  return [0, 2, 4].map((at) => Number.parseInt(hex.slice(at, at + 2), 16));
}

/** The smallest step of 1, 2, or 5 times a power of ten that is at least
 * `least`, so labelled rows and columns fall on round indices. */
function roundStep(least) {
  for (let power = 1; ; power *= 10) {
    for (const step of [1, 2, 5]) if (step * power >= least) return step * power;
  }
}

function truncate(text, room) {
  const fits = Math.max(2, Math.floor(room / 6));
  return text.length > fits ? `${text.slice(0, fits - 1)}…` : text;
}

function label(v) {
  if (Math.abs(v) >= 1e4) {
    return new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: 2 }).format(v);
  }
  return Number(v.toPrecision(4)).toString();
}
