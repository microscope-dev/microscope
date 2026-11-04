// @ts-check
// The built-in bitmap: bytes as an image.
//
//   pixels   bytes (as bytes(PTR, LEN) reads them) or numbers, row by row,
//            or an array of rows; `values` may hold them instead
//   columns  pixels a row, unless pixels are rows
//   format   optional: "gray8" (the default), "rgba8", "rgb565" (two bytes
//            a pixel, little-endian), or "bits" (a bit a pixel, the
//            highest bit of each byte first, rows padded to whole bytes)
//
// The image is zoomed by a whole number with smoothing off, so each pixel
// stays a sharp square; hovering one names its column and row, and the
// Table lists the bytes. An overlay outlines the pixels that changed since
// the stop before, so the image still shows where all of them did; pixels
// too small to outline are tinted instead.

const MOST_WIDTH = 1200;
const MOST_HEIGHT = 640;
const FORMATS = { gray8: 1, rgba8: 4, rgb565: 2, bits: 0 };
// The least zoom at which changed pixels are outlined, and how many marks
// an outline's image has each way to a pixel.
const OUTLINE = 4;
const MARKS = 4;

microscope.draw((input, { previous, width, theme: mode }) => {
  const theme = microscope.theme;
  const image = imageOf(input);
  const { columns, rows } = image;
  /** @type {ReturnType<typeof imageOf> | null} */
  let before = null;
  if (previous !== null) {
    try {
      const old = imageOf(previous);
      if (old.columns === columns && old.rows === rows && old.format === image.format) before = old;
    } catch {
      // The stop before had no image this chart reads.
    }
  }
  const room = Math.min(Math.max(width, 160), MOST_WIDTH);
  const zoom = Math.max(1, Math.floor(Math.min(room / columns, MOST_HEIGHT / rows)));
  const W = columns * zoom;
  const H = rows * zoom;
  const shapes = [
    microscope.image({
      x: 0,
      y: 0,
      width: W,
      height: H,
      pixels: image.rgba,
      columns,
      rows,
      title: "pixels",
    }),
  ];
  let changes = 0;
  // Outlines need a few screen pixels to each image pixel.
  const outlined = zoom >= OUTLINE;
  if (before !== null) {
    const changed = new Uint8Array(columns * rows);
    for (let p = 0; p < columns * rows; p++) {
      const a = p * 4;
      if (
        image.rgba[a] !== before.rgba[a] ||
        image.rgba[a + 1] !== before.rgba[a + 1] ||
        image.rgba[a + 2] !== before.rgba[a + 2] ||
        image.rgba[a + 3] !== before.rgba[a + 3]
      ) {
        changed[p] = 1;
        changes += 1;
      }
    }
    if (changes > 0) {
      const marks = outlined
        ? outline(changed, columns, rows, mode)
        : tint(changed, columns, rows, mode);
      shapes.push(
        microscope.image({
          x: 0,
          y: 0,
          width: W,
          height: H,
          pixels: marks.pixels,
          columns: marks.columns,
          rows: marks.rows,
        }),
      );
    }
  }
  shapes.push(
    microscope.rect({ x: 0, y: 0, width: W, height: H, fill: "none", stroke: theme.line }),
  );
  const caption = [`${columns} × ${rows} ${image.format}`, `at ${zoom}×`];
  if (before !== null) {
    caption.push(
      changes === 0
        ? "unchanged since the stop before"
        : `${changes} ${changes === 1 ? "pixel" : "pixels"} changed since the stop before, ${outlined ? "outlined" : "tinted"}`,
    );
  }
  if (W > room) caption.push("scaled down to fit");
  return microscope.picture({ width: W, height: H, shapes, caption: caption.join(" · ") });
});

/** The image as RGBA pixels, from its bytes in its format. */
function imageOf(input) {
  const source = input.pixels ?? input.values;
  if (source === undefined || source === null) {
    throw new TypeError("bitmap takes `pixels`, bytes or numbers row by row, and `columns`");
  }
  const format = input.format ?? "gray8";
  if (!Object.hasOwn(FORMATS, format)) {
    throw new TypeError('`format` is "gray8", "rgba8", "rgb565", or "bits"');
  }
  let bytes;
  let columns;
  if (
    Array.isArray(source) &&
    source.length > 0 &&
    source.every((row) => Array.isArray(row) || ArrayBuffer.isView(row))
  ) {
    // oxlint-disable-next-line no-inline-comments -- a JSDoc type cast has to sit inline
    const lines = /** @type {ArrayLike<any>[]} */ (source);
    const width = lines[0].length;
    bytes = new Uint8Array(lines.length * width);
    lines.forEach((row, r) => {
      if (row.length !== width)
        throw new RangeError(`row ${r} holds ${row.length}, and row 0 holds ${width}`);
      bytes.set(byteRow(row, r), r * width);
    });
    const per = FORMATS[format];
    columns = format === "bits" ? width * 8 : width / per;
    if (!Number.isInteger(columns))
      throw new RangeError(`a row of ${width} bytes is not whole ${format} pixels`);
  } else {
    bytes = byteRow(source, null);
    columns = Number(input.columns);
    if (!Number.isInteger(columns) || columns < 1) {
      throw new TypeError("`columns` must be a positive whole number");
    }
  }
  const stride = format === "bits" ? Math.ceil(columns / 8) : columns * FORMATS[format];
  if (bytes.length % stride !== 0) {
    throw new RangeError(
      `${bytes.length} bytes are not whole rows of ${columns} ${format} pixels, ${stride} bytes each`,
    );
  }
  const rows = bytes.length / stride;
  if (rows === 0) throw new RangeError("the image has no rows");
  const rgba = new Uint8ClampedArray(columns * rows * 4);
  for (let r = 0; r < rows; r++) {
    for (let c = 0; c < columns; c++) {
      const p = (r * columns + c) * 4;
      if (format === "gray8") {
        const v = bytes[r * stride + c];
        rgba.set([v, v, v, 255], p);
      } else if (format === "rgba8") {
        rgba.set(bytes.subarray(r * stride + c * 4, r * stride + c * 4 + 4), p);
      } else if (format === "rgb565") {
        const v = bytes[r * stride + c * 2] | (bytes[r * stride + c * 2 + 1] << 8);
        rgba.set(
          [(((v >> 11) & 31) * 255) / 31, (((v >> 5) & 63) * 255) / 63, ((v & 31) * 255) / 31, 255],
          p,
        );
      } else {
        const on = (bytes[r * stride + (c >> 3)] >> (7 - (c & 7))) & 1;
        const v = on ? 0 : 255;
        rgba.set([v, v, v, 255], p);
      }
    }
  }
  return { columns, rows, format, rgba };
}

/** Bytes from bytes, or from numbers that are each a byte. */
function byteRow(value, row) {
  const where = row === null ? "`pixels`" : `row ${row}`;
  if (
    value instanceof Uint8Array ||
    value instanceof Uint8ClampedArray ||
    value instanceof Int8Array
  ) {
    return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  }
  if (Array.isArray(value) || (ArrayBuffer.isView(value) && !(value instanceof DataView))) {
    // oxlint-disable-next-line no-inline-comments -- a JSDoc type cast has to sit inline
    const list = /** @type {ArrayLike<any>} */ (/** @type {unknown} */ (value));
    return Uint8Array.from(list, (item, index) => {
      const v = Number(item);
      if (!Number.isInteger(v) || v < 0 || v > 255)
        throw new RangeError(`${where}[${index}] is ${String(item)}, not a byte`);
      return v;
    });
  }
  throw new TypeError(`${where} must be bytes or numbers`);
}

/** The changed pixels' color: orange, which no gray is. */
function markColor(mode) {
  return mode === "dark" ? [255, 170, 102, 255] : [196, 90, 0, 255];
}

/** An image of MARKS × MARKS marks a pixel, set along each edge a changed
 * pixel shares with an unchanged one or the image's border. */
function outline(changed, columns, rows, mode) {
  const across = columns * MARKS;
  const down = rows * MARKS;
  const pixels = new Uint8ClampedArray(across * down * 4);
  const color = markColor(mode);
  const at = (c, r) =>
    c >= 0 && c < columns && r >= 0 && r < rows && changed[r * columns + c] === 1;
  for (let r = 0; r < rows; r++) {
    for (let c = 0; c < columns; c++) {
      if (!at(c, r)) continue;
      const left = !at(c - 1, r);
      const right = !at(c + 1, r);
      const top = !at(c, r - 1);
      const bottom = !at(c, r + 1);
      for (let k = 0; k < MARKS; k++) {
        if (left) pixels.set(color, ((r * MARKS + k) * across + c * MARKS) * 4);
        if (right) pixels.set(color, ((r * MARKS + k) * across + c * MARKS + MARKS - 1) * 4);
        if (top) pixels.set(color, (r * MARKS * across + c * MARKS + k) * 4);
        if (bottom) pixels.set(color, ((r * MARKS + MARKS - 1) * across + c * MARKS + k) * 4);
      }
    }
  }
  return { pixels, columns: across, rows: down };
}

/** A light tint over each changed pixel, light enough to see through. */
function tint(changed, columns, rows, mode) {
  const pixels = new Uint8ClampedArray(changed.length * 4);
  const [red, green, blue] = markColor(mode);
  const mark = [red, green, blue, 96];
  for (let p = 0; p < changed.length; p++) {
    if (changed[p] === 1) pixels.set(mark, p * 4);
  }
  return { pixels, columns, rows };
}
