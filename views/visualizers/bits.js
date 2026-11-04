// @ts-check
// The built-in bits: integers as grids of their set and clear bits.
//
//   values   integers: one, or a sequence of up to 4,096
//   columns  optional: bits a row, 8 by default
//   origin   optional: "top-left" (the default) puts bit 0 at the top left;
//            "bottom-left" puts it at the bottom left, so a u64 bitboard
//            reads as a chess board from white's side
//   labels   optional, one per integer, or one string for one integer
//   width    optional: the bits an integer has, when its type does not
//            say, as a single integer's does not; 32, or 64 for bigints
//
// Each integer's width comes from its type when it arrives as an array of
// them: a u64 has 64 bits, a u8 has 8.
// Bits that changed since the stop before are outlined, each bit's title
// names its index, and each grid gives its integer in hexadecimal.

const MOST = 4_096;
const CELL = 14;

microscope.draw((input, { previous, width, paths }) => {
  const theme = microscope.theme;
  const { words, bits, raw } = wordsOf(input.values, input.width);
  if (words.length > MOST)
    throw new RangeError(`bits draws up to ${MOST} integers, and \`values\` holds ${words.length}`);
  const columns =
    input.columns === undefined || input.columns === null
      ? Math.min(8, bits)
      : Number(input.columns);
  if (!Number.isInteger(columns) || columns < 1 || columns > bits) {
    throw new RangeError(`\`columns\` must be a whole number from 1 to ${bits}`);
  }
  const origin = input.origin ?? "top-left";
  if (origin !== "top-left" && origin !== "bottom-left")
    throw new TypeError('`origin` is "top-left" or "bottom-left"');
  // One label may name one integer.
  const given = typeof input.labels === "string" ? [input.labels] : input.labels;
  const labels =
    given === undefined || given === null
      ? null
      : Array.from(given, (label) =>
          typeof label === "object" && label !== null && typeof label.name === "string"
            ? label.name
            : String(label),
        );
  /** @type {bigint[] | null} */
  let before = null;
  if (previous !== null) {
    try {
      const old = wordsOf(previous.values, previous.width);
      if (old.words.length === words.length && old.bits === bits) before = old.words;
    } catch {
      // The stop before had no integers this chart reads.
    }
  }
  const rows = Math.ceil(bits / columns);
  const gridWidth = columns * CELL;
  const gridHeight = rows * CELL;
  const blockWidth = Math.max(gridWidth, 16 + Math.ceil(bits / 4) * 7.2) + 20;
  const blockHeight = gridHeight + 38;
  const W = Math.round(Math.min(Math.max(width, 320), 1600));
  const across = Math.max(1, Math.floor((W - 8) / blockWidth));
  const shapes = [];
  let changes = 0;
  const single = !Array.isArray(input.values) && !ArrayBuffer.isView(input.values);
  words.forEach((word, k) => {
    const ox = 8 + (k % across) * blockWidth;
    const oy = 6 + Math.floor(k / across) * blockHeight;
    const name = labels?.[k] ?? (single ? "value" : `[${k}]`);
    const cells = [];
    for (let bit = 0; bit < bits; bit++) {
      const column = bit % columns;
      const row = Math.floor(bit / columns);
      const x = column * CELL;
      const y = (origin === "bottom-left" ? rows - 1 - row : row) * CELL;
      const set = (word >> BigInt(bit)) & 1n;
      cells.push(
        microscope.rect({
          x,
          y,
          width: CELL - 2,
          height: CELL - 2,
          radius: 2,
          fill: set ? theme.series[0] : theme.line,
          title: `${name} bit ${bit}: ${set}`,
        }),
      );
      if (before !== null && ((before[k] >> BigInt(bit)) & 1n) !== set) {
        changes += 1;
        cells.push(
          microscope.rect({
            x: x - 0.75,
            y: y - 0.75,
            width: CELL - 0.5,
            height: CELL - 0.5,
            radius: 2.5,
            fill: "none",
            stroke: theme.ink,
            strokeWidth: 1.5,
          }),
        );
      }
    }
    const hex = `0x${word.toString(16).padStart(Math.ceil(bits / 4), "0")}`;
    shapes.push(
      microscope.group({
        x: ox,
        y: oy,
        shapes: cells,
        select:
          paths.values === null || paths.values === undefined
            ? undefined
            : single
              ? paths.values === ""
                ? undefined
                : paths.values
              : `${paths.values}[${k}]`,
      }),
      microscope.text({ x: ox, y: oy + gridHeight + 12, text: name, size: 11, fill: theme.ink2 }),
      microscope.text({
        x: ox,
        y: oy + gridHeight + 26,
        text: hex,
        size: 11,
        family: "mono",
        fill: theme.ink,
        title: `${name} = ${raw(k)} = ${hex}`,
      }),
    );
  });
  const caption = [`${words.length} × ${bits}-bit`, `origin ${origin}`, `${columns} bits a row`];
  if (words.length <= 4) caption.push(words.map((word) => `0x${word.toString(16)}`).join(", "));
  if (before !== null) {
    caption.push(
      changes === 0
        ? "unchanged since the stop before"
        : `${changes} ${changes === 1 ? "bit" : "bits"} changed since the stop before, outlined`,
    );
  }
  const lines = Math.ceil(words.length / across);
  return microscope.picture({
    width: W,
    height: 6 + lines * blockHeight,
    shapes,
    caption: caption.join(" · "),
  });
});

/** The integers as unsigned bigints, their width, and their exact text. */
function wordsOf(values, width) {
  /** @type {[Function, number][]} */
  const widths = [
    [Uint8Array, 8],
    [Int8Array, 8],
    [Uint8ClampedArray, 8],
    [Uint16Array, 16],
    [Int16Array, 16],
    [Uint32Array, 32],
    [Int32Array, 32],
    [BigUint64Array, 64],
    [BigInt64Array, 64],
  ];
  if (ArrayBuffer.isView(values) && !(values instanceof DataView)) {
    const bits = widths.find(([kind]) => values instanceof kind)?.[1];
    if (bits === undefined) throw new TypeError("bits draws integers, not floats");
    const list = /** @type {ArrayLike<number | bigint>} */ (/** @type {unknown} */ (values));
    return {
      words: Array.from(list, (v) => BigInt.asUintN(bits, BigInt(v))),
      bits,
      raw: (k) => String(list[k]),
    };
  }
  const list = Array.isArray(values) ? values : [values];
  const words = list.map((v, k) => {
    const value = typeof v === "object" && v !== null && "value" in v ? v.value : v;
    if (typeof value === "bigint") return value;
    if (typeof value === "number" && Number.isInteger(value)) return BigInt(value);
    throw new TypeError(
      `bits draws integers, and \`values${Array.isArray(values) ? `[${k}]` : ""}\` is not one`,
    );
  });
  const bits =
    width === undefined || width === null
      ? words.some((_, k) => typeof list[k] === "bigint")
        ? 64
        : 32
      : Number(width);
  if (!Number.isInteger(bits) || bits < 1 || bits > 128)
    throw new RangeError("`width` must be a whole number of bits from 1 to 128");
  return {
    words: words.map((word) => BigInt.asUintN(bits, word)),
    bits,
    raw: (k) => String(list[k]),
  };
}
