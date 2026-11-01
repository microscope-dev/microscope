# Visualizers

A visualizer draws program values in `microscope web`: chess boards,
framebuffer images, sample plots, or models you can rotate. A [view](views.md)
selects the data with `visualize`. A renderer, a small JavaScript file,
turns that data into a picture. The page runs the renderer in a sandbox,
validates the picture, and draws it. `visualize` does not affect terminal
or debug-adapter output; neither runs renderers.

Every renderer example on this page runs as one of microscope's tests.

## A first drawing

This Conway's Game of Life board uses one byte per cell:

```c
struct life {
    int generation;
    uint8_t cells[48][64];
};
```

Create `life.views` to select the drawing's inputs:

```text
microscope-views 1

# A life board, drawn from its cells' bytes by life.js beside this file.
extend c life {
    visualize "life" {
        cells = bytes(&cells[0][0], sizeof(cells))
        columns = 64
        generation = generation
    }
}
```

Save the renderer beside it as `life.js`, matching the drawing's name:

```js
// @ts-check
// Draws a life board: one byte a cell, 1 for alive, row by row.
microscope.draw(({ cells, columns, generation }, { previous }) => {
  const rows = cells.length / columns;
  const pixels = new Uint8ClampedArray(cells.length * 4);
  for (let i = 0; i < cells.length; i++) {
    const born = cells[i] === 1 && previous !== null && previous.cells[i] === 0;
    const [r, g, b] = cells[i] !== 1 ? [238, 241, 245] : born ? [176, 80, 10] : [26, 34, 48];
    pixels.set([r, g, b, 255], i * 4);
  }
  return microscope.picture({
    width: columns * 6, height: rows * 6, caption: `generation ${generation}`,
    shapes: [microscope.image({ x: 0, y: 0, width: columns * 6, height: rows * 6, pixels, columns, rows, title: "cells" })],
  });
});
```

`microscope web --views life.views ./life` loads both files. Stop with a
`struct life` or a pointer to one in scope, then press Alt+V. The Drawings
view updates at each stop, marking cells born since the previous stop in
orange. After editing `life.js`, run Reload views from the command palette
(Ctrl+K) to apply the changes without restarting.

`extend` adds a drawing without replacing the type's presenting view, so
`print life` in the terminal is unchanged. `cells = bytes(…)` reads the
board's 3,072 bytes at once and passes them as a `Uint8Array`.

## The Drawings view

Open Drawings with Alt+V or beside Source, Disassembly, and Memory. It shows
cards for arguments and locals in the selected frame whose views provide
drawings, followed by pinned values. Pointers use their targets' drawings:
a `&mut Board` or `struct life *` draws the board itself. The number beside
Drawings is the card count.

- **Draw** on a value row pins the value with the drawings its view
  offers. **Draw as…** on any value row, or on a card, pins it with any
  renderer; that renderer draws the value itself as its `values` input,
  as `visualize "NAME" { values = self }` would.
- Links preserve the view: `view=drawings` opens it, and `d=PATH` pins a
  value. `d=PATH~NAME` also selects a renderer.
- Multiple drawings appear as tabs on one card. The selected tab persists
  across stops.
- At each stop shown in the tab, the card redraws and passes the previous
  drawing's inputs as `previous` so the renderer can mark changes. While
  the program runs or the next drawing loads, the last drawing stays dimmed
  and labeled with its stop.
- If stops arrive faster than rendering, such as while holding F10, each
  card draws one stop at a time, at most once per frame, skipping to the
  latest. An earlier stop's drawing never replaces a later one.
- Off-screen cards draw when scrolled back into view. Cards also redraw
  when their width or the page's theme changes, including system dark mode.
- A shape's `select` opens part of the value as a row you can expand or
  watch. Its `title` is both tooltip and accessible name. Titled images
  also identify the pixel under the pointer: `cells: column 31, row 20`.
- Table lists each input value as a row, such as `cells[1311]` and `1`,
  rendering one screenful at a time even for a million values. Copy CSV
  copies `path,value` lines. Both preserve numbers exactly: integers in
  full, floats as the shortest text that round-trips in their format
  (`f32` as `f32`). Captions round to six digits; titles and Table do not.
- Pictures with more than 2,000 shapes use canvas instead of SVG, retaining
  the same tooltips and clicks.
- If drawing fails, the card shows the reason instead: an unreadable input
  and why it failed; a renderer error with file, line, and message; a
  renderer stopped after 2 seconds; a rejected picture with the invalid
  shape and reason; or a missing renderer.
- Live drawings, such as meshes, respond to the pointer while on screen
  (see Live renderers). Clicking the canvas gives it keyboard focus except
  for debugger function keys; Escape returns focus. A live renderer stops
  after 2 seconds without a response or on WebGL context loss. Its card
  offers to restart it.

## Inputs

Each `visualize` input is evaluated like a view field, applying views, and
converted to JavaScript according to its type, not its value. A `uint64_t`
is always a bigint, even when it is 3.

| In the program | In the renderer | For example |
|---|---|---|
| `bool` | boolean | `true` |
| integers of 32 bits or fewer, and integers the view writes, such as `64` | number | `-7` |
| integers of 64 and 128 bits, pointers, addresses | bigint | `65280n` |
| floats | number | `0.5`, `NaN` |
| characters | a string of one character | `"A"` |
| text: anything presented as text | string | `"e2e4"` |
| strings the view writes | string | `"bottom-left"` |
| enumerations | `{name, value}`; `name` is null when no enumerator matches | `{name: "White", value: 0}` |
| sums: Rust enums, optionals, a view's variants | `{variant, value?}`: the payload's one field, a record of several, or none | `{variant: "Some", value: {…}}` |
| records and a view's fields | an object | `{color: …, kind: …}` |
| tuples | an array | `[1, "two"]` |
| sequences of numbers: arrays, slices, and what views present as sequences | a typed array of the element's type | `Float64Array(1000000)` |
| other sequences | an array; an array of several dimensions is an array of rows | `[{variant: "None"}, …]` |
| maps | an array of `[key, value]` pairs, in the view's order | `[["GET", 3]]` |
| `bytes(PTR, LEN)` | `Uint8Array` | `Uint8Array(3072)` |

- If any input part is unreadable, the renderer is not called. The card
  identifies the part and reason; missing values never become zeros.
- Pointers remain addresses. Use `*p` to pass a pointer's target, or name
  a value presented by a view. View-presented values use their logical
  form even when stored as pointers, as Go maps are.
- `bytes(PTR, LEN)` reads memory once. Contiguous numeric sequences, such
  as arrays and the elements of `Vec`, `std::vector`, and numpy arrays,
  are also read at once. Both reach the page as bytes, not text.

## Renderers

A renderer is a script that registers a function with `microscope.draw`
once, or uses `microscope.live` for a live drawing (see Live renderers).
A draw function receives inputs and a context and returns a picture; it
may be `async`. Renderers use `microscope` and the sandbox's language and
drawing APIs, not page globals.

```text
microscope.draw((input, context) => picture)

context.previous   the inputs drawn for this value at the stop before, or null
context.paths      the part of the drawn value each input is, or null
context.width      the CSS pixels the card offers; a picture is scaled to fit
context.theme      "light" or "dark"
```

Use `context.paths` to make shapes open the parts they draw. An input
expressed as members and indices, such as `squares = mailbox`, has path
`"mailbox"`, so square 28 selects `${paths.squares}[28]`. The value itself
(`self` or the input from Draw as…) has path `""`. Other inputs, such as
`bytes(…)`, `len * 2`, or a string, have path null and no parts to open.

```microscope-renderer-example
// A bar per value, from a zero baseline, with each value in its title.
microscope.draw(({ values }, { paths }) => {
  const most = Math.max(1, ...Array.from(values, Math.abs));
  const shapes = Array.from(values, (value, index) =>
    microscope.rect({
      x: index * 22, y: 100 - (Math.max(0, value) / most) * 100,
      width: 20, height: (Math.abs(value) / most) * 100,
      fill: microscope.theme.series[0], title: `[${index}] = ${value}`,
      select: paths.values === null ? undefined : `${paths.values}[${index}]`,
    }),
  );
  return microscope.picture({ width: values.length * 22, height: 100, shapes, caption: `${values.length} values` });
});
---
({ values: new Float64Array([3, 1, 4, 1, 5]) })
```

### Pictures and shapes

`microscope.picture({width, height, shapes, caption?})` is a picture of
`width` by `height` units, scaled to fit its card and never cropped. Its
shapes are drawn in order, later ones on top.

| Shape | Properties |
|---|---|
| `microscope.rect` | `x`, `y`, `width`, `height`, `radius?` |
| `microscope.circle` | `x`, `y`, `r` |
| `microscope.line` | `x1`, `y1`, `x2`, `y2` |
| `microscope.polyline`, `microscope.polygon` | `points`: x0, y0, x1, y1, …, an array or a `Float32Array` or `Float64Array` |
| `microscope.path` | `d`: SVG path data |
| `microscope.text` | `x`, `y`, `text`, `size?`, `weight?` (`"normal"`, `"bold"`, or 100 to 900), `family?` (`"sans"` or `"mono"`), `anchor?` (`"start"`, `"middle"`, `"end"`), `baseline?` (`"auto"`, `"alphabetic"`, `"middle"`, `"central"`, `"hanging"`, `"ideographic"`) |
| `microscope.group` | `shapes`; `x?` and `y?` translate them; `rotate?` (degrees clockwise) and `scale?` rotate and scale around that point |
| `microscope.image` | `x`, `y`, `width`, `height`, and either `pixels` (RGBA, a `Uint8ClampedArray`) with `columns` and `rows`, or `canvas` (an `OffscreenCanvas` drawn by the renderer); `smooth?` blends scaled pixels, which stay sharp otherwise |

Every shape also takes:

- `fill` and `stroke`: `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb()`, `hsl()`,
  or a CSS color name. Shapes default to the page's ink. Lines and
  polylines use their stroke.
- `strokeWidth`, `opacity` (0 to 1), and `dash`, the lengths of dashes and
  gaps.
- `title`: hover text and the shape's accessible name, so a screen reader
  reads `e4: White Pawn`.
- `select`: a path within the drawn value using member names, `.`, and
  indices, such as `mailbox[21]`, `[3]`, or `a.b[2].c`. Click the shape
  or press Enter on it to open that part as a row. Paths are not
  expressions and cannot trigger arbitrary evaluation.

A shape without `title` or `select`, in a group without either, passes
pointer events to shapes beneath it. A piece drawn over a square leaves
the square clickable; marks over a titled image preserve pixel tooltips.

```microscope-renderer-example
// Every kind of shape, in the page's colors.
microscope.draw(() => {
  const { series, ink2, changed } = microscope.theme;
  const pixels = new Uint8ClampedArray([255, 0, 0, 255, 0, 0, 255, 255]);
  const canvas = new OffscreenCanvas(8, 8);
  const pen = canvas.getContext("2d");
  if (pen) {
    pen.fillStyle = series[1];
    pen.fillRect(0, 0, 8, 4);
  }
  return microscope.picture({
    width: 200, height: 60, caption: "shapes",
    shapes: [
      microscope.rect({ x: 0, y: 0, width: 20, height: 20, radius: 3, fill: series[0], title: "a rect" }),
      microscope.circle({ x: 35, y: 10, r: 9, stroke: changed, strokeWidth: 2, fill: "none" }),
      microscope.line({ x1: 50, y1: 0, x2: 70, y2: 20, dash: [3, 2] }),
      microscope.polyline({ points: new Float64Array([75, 20, 80, 0, 85, 20]), stroke: series[2] }),
      microscope.polygon({ points: [90, 20, 100, 0, 110, 20], fill: "rgb(40 120 200 / 50%)" }),
      microscope.path({ d: "M115 20 Q125 0 135 20 Z", fill: "hsl(30 80% 50%)" }),
      microscope.text({ x: 0, y: 40, text: "mono", family: "mono", size: 12, fill: ink2 }),
      microscope.group({ x: 60, y: 30, rotate: 10, scale: 0.5, shapes: [microscope.rect({ x: 0, y: 0, width: 20, height: 20 })] }),
      microscope.image({ x: 140, y: 0, width: 20, height: 10, pixels, columns: 2, rows: 1 }),
      microscope.image({ x: 165, y: 0, width: 20, height: 20, canvas, smooth: true }),
    ],
  });
});
---
({})
```

### Helpers

- `microscope.theme` provides the page's theme colors as `#rrggbb`:
  `ink`, `ink2`, `ink3`, `paper`, `surface`, `line`, `accent`, `changed`,
  `good`, and `bad`. `series` contains eight categorical colors ordered
  and checked for color-blind readers. Cards redraw when the theme changes.
- `microscope.same(a, b)` deeply compares inputs, including typed arrays
  and bigints, to help mark changes since `previous`.
- `microscope.color.scale(t, from, to)` blends two `#rgb` or `#rrggbb`
  colors by fraction `t`, for heatmaps.

```microscope-renderer-example
// The squares that changed since the stop before, outlined.
microscope.draw(({ cells }, { previous }) => {
  const shapes = Array.from(cells, (cell, index) =>
    microscope.rect({
      x: index * 12, y: 0, width: 10, height: 10,
      fill: microscope.color.scale(cell / 255, "#eef1f5", "#1a2230"),
      stroke: previous !== null && !microscope.same(previous.cells[index], cell) ? microscope.theme.changed : undefined,
    }),
  );
  return microscope.picture({ width: cells.length * 12, height: 10, shapes });
});
---
({ cells: new Uint8Array([0, 128, 255]) })
```

### Errors

In Chromium and Firefox, renderer errors and rejected promises include the
file and line: `life.js:12:5: TypeError: …`. The page also reports syntax
errors and scripts that call neither `microscope.draw` nor `microscope.live`.
It rejects invalid pictures in full, naming the first faulty shape, such
as `shape 3 (rect): fill is not a color`. Invalid inputs include unknown
properties, nonfinite numbers, invalid colors or SVG path data, and pictures
exceeding the limits below.

For editor type checking, start the renderer with `// @ts-check` and provide
`sdk/web/microscope-visualizer.d.ts`, which declares `microscope`.

## Live renderers

A picture normally draws once per stop. A live renderer keeps its own
canvas while its card is on screen and requests frames as needed, for
example to rotate a model, fly through a level, or inspect bones. Register
it with `microscope.live` instead of `microscope.draw`. Its function receives
an `OffscreenCanvas` sized to the card in device pixels, the first stop's
inputs, and a context. It returns event handlers:

```text
microscope.live((canvas, input, context) => ({ frame, update, pointer, key, resize }))

context            previous, paths, and theme, as a picture's, and width and
                   height, the canvas's size in CSS pixels
frame(time)        draw; the page shows the canvas when it returns
update(input, context)   a new stop's inputs; a frame follows
pointer(event)     {type, x, y, dx, dy, buttons, wheel, shift, ctrl, alt}, in
                   the canvas's pixels; type is "down", "move", "up", "wheel",
                   or "leave", and a wheel turned down is positive
key(event)         {key, shift, ctrl, alt}, while the canvas has focus
resize(width, height)    the canvas's new size in device pixels; a frame follows

microscope.redraw()          ask for one more frame, as after a drag
microscope.animate(on)       ask for a frame every display frame, or stop asking
microscope.caption(text)     the card's caption
microscope.hint(text)        the tooltip beside the pointer, or none with null
microscope.select(path)      open a part of the value, as a shape's select does
```

These five `microscope` calls work only in live renderers and do nothing in
`draw`. Each sends its latest value after the handler returns, so calling
`caption` in a loop sends one message.

Each function is optional, and each may be `async`. The canvas takes
`getContext("webgl2")` or `getContext("2d")`; WebGPU is not offered.

- The page calls `frame` from its animation frame, one at a time, after
  startup, a new stop, a resize, or `microscope.redraw()`. It also calls
  every frame while `microscope.animate(true)` is active, but never while
  the card is off screen or the tab is hidden. Frames reach the page as
  `ImageBitmap`s containing only pixels.
- Each card keeps one worker while on screen. New stops call `update` in
  that worker, preserving state such as the camera position. Scrolling
  away, closing the view, or switching programs ends the worker. Scrolling
  back starts a new worker with the latest inputs.
- At most four live renderers run per tab; later cards wait for one to
  leave the screen.
- The page checks worker responsiveness every second, after handlers
  return. A worker that fails to respond within 2 seconds, whether looping
  or awaiting an unsettled promise, is stopped. Errors and WebGL context
  loss also stop it. The card explains why and offers Restart.
- Headless Firefox has no WebGL, so WebGL renderers report failure there.
  For example, `mesh` reports that WebGL is unavailable.

```microscope-live-example
// A bar per value that follows the pointer: the bar under it is lit,
// and named beside it.
microscope.live((canvas, { values }) => {
  const draw = canvas.getContext("2d");
  let shown = values;
  let lit = -1;
  const width = () => canvas.width / Math.max(1, shown.length);
  return {
    frame() {
      draw.fillStyle = microscope.theme.surface;
      draw.fillRect(0, 0, canvas.width, canvas.height);
      const most = Math.max(1, ...Array.from(shown, Math.abs));
      shown.forEach((value, index) => {
        const height = (Math.abs(value) / most) * canvas.height;
        draw.fillStyle = index === lit ? microscope.theme.accent : microscope.theme.series[0];
        draw.fillRect(index * width(), canvas.height - height, width() - 2, height);
      });
    },
    update(next) {
      shown = next.values;
    },
    pointer({ type, x }) {
      const at = type === "leave" ? -1 : Math.floor(x / width());
      if (at !== lit) {
        lit = at;
        microscope.hint(at >= 0 && at < shown.length ? `[${at}] = ${shown[at]}` : null);
        microscope.redraw();
      }
    },
  };
});
---
({ values: new Float64Array([3, 1, 4, 1, 5]) })
```

## Built-in renderers

microscope includes eleven renderers in `views/visualizers/`. Use them in
`visualize` or choose any of them through Draw as… on a value. They use only
the API on this page and serve as examples for your own renderers.

| Renderer | Inputs | Draws |
|---|---|---|
| `line-plot` | `values`, or `series`, a map or record of up to 8 names to numeric sequences; `x?`, `log?` | Above one value per pixel column, retains each column's first, lowest, highest, and last values (M4), with a range band to preserve spikes. NaN creates gaps; ±∞ uses arrows. A ghost line shows the previous stop. |
| `bar-chart` | `values` with `labels?`, or `entries`, a map or record (as `values` may be); `orientation?` (`"horizontal"` or `"vertical"`), `sort?` (`"value"`, `"label"`, or `"none"`) | Maps sorted by value, since their order may be random; entries beyond 40 fold into Other with their count and total. Arrays keep program order. Ticks mark previous bar values. |
| `scatter-plot` | `x` and `y`, or `values`, pairs or records of two numbers; `group?`, `labels?` | Dots with distinct hues for three groups. Above 10,000 points, uses a density image, darker where more points fall. Up to 2,000 points, moved points trail lines from their previous positions. |
| `histogram` | `values`, raw samples, or `counts` and `edges`; `bins?`, `log?` | Freedman–Diaconis bins, exact p50, p90, and p99 (type 7), and an outline of the previous counts. |
| `box-plot` | `groups` (or `values`), a map, record, or array of samples or of `{min, q1, median, q3, max}`; `labels?`; `error?`: `"sd"`, `"ci95"`, or numbers with `mean`; `error_label?` | Quartiles (type 7), Tukey whiskers, outliers, error bars beside each box, and ticks at previous medians. |
| `donut-chart` | `values` with `labels?`, or `entries` (as `values` may be); none negative | The seven largest shares and Other, with titles showing changes since the previous stop. |
| `heatmap` | `values` and `columns`, or an array of rows; `row_labels?`, `column_labels?`, `scale?` (`"sequential"` or `"diverging"`), `log?` | Blue through gray to red when values cross zero. NaN is hatched; changed cells are outlined. Above 10,000 cells, uses one image, each pixel showing its largest cell value. |
| `flame-graph` | `nodes`, records of `name`, `value`, and `parent`, or `stacks`, `"a;b;c"` to counts as pairs or a map; `values` for either | An icicle with the root on top, frame widths proportional to totals, and colors by name. Changed totals have outlines. |
| `bitmap` | `pixels` (or `values`) and `columns`, or an array of rows; `format?` (`"gray8"`, `"rgba8"`, `"rgb565"`, `"bits"`) | A sharp image at integer zoom, with changed pixels outlined or tinted when too small to outline. |
| `bits` | `values`, one integer or up to 4,096; `columns?`, `origin?` (`"top-left"` or `"bottom-left"`), `labels?`, `width?` | A bit grid per integer, using its type's width, with flipped bits outlined and values in hexadecimal. |
| `mesh` (live) | `vertices` (bytes), `stride`, `position`; `normal?`, `color?` (offsets in a vertex), `indices?` (bytes) with `index_size?` (2 or 4), `primitive?` (`"triangles"`, `"lines"`, `"points"`), `transform?` (16 numbers, column-major), `up?` (`"y"` or `"z"`) | A WebGL2 model lit from the viewer, so flipped normals appear dark; without normals, uses face shading. Drag to rotate, scroll to zoom, press W for wireframe, or hover to identify the nearest vertex. Nonfinite positions, transformed positions outside float range, and indices beyond the last vertex prevent drawing and are reported in the caption. |

`mesh` draws mesh data from program memory without custom JavaScript.
For a C++ engine's `std::vector<Vertex>` and indices:

```text
extend c++ engine::Mesh {
    visualize "mesh" {
        vertices = bytes(&vertices[0], len(vertices) * sizeof(engine::Vertex))
        stride = sizeof(engine::Vertex)
        position = offsetof(engine::Vertex, position)
        normal = offsetof(engine::Vertex, normal)
        indices = bytes(&indices[0], len(indices) * 4)
        index_size = 4
    }
}
```

Only data in process memory can be drawn. If a mesh has been uploaded to
the GPU and freed from process memory, use a copy retained by the loader.

## Where renderers come from

A view looks for renderers in its own source before using built-ins:

- View files use adjacent `NAME.js` files, loaded with the view file and
  reread by Reload views.
- Module-carried views use renderers from the module's
  `.debug_microscope_views` section. In C or C++, embed one with
  `MICROSCOPE_VISUALIZER("board", "views/board.js");` from `microscope_views.h`.
  In Rust, use `microscope_views::microscope_visualizer!("board", PATH);`.
- Built-ins are the fallback.

`microscope views check PROGRAM` lists the JavaScript for every renderer
referenced by the views. It fails if a drawing cannot bind, such as when an
input names a nonexistent member. Renderers run only in the page;
microscope has no JavaScript engine of its own.

## What a renderer cannot do

A program you receive may carry a hostile renderer. Each renderer runs in
its own worker, started by a hidden frame with an opaque origin and a policy
that blocks all requests. Before running the renderer, the worker removes
all globals except language, timer, and drawing APIs. A renderer cannot:

- send anything anywhere, nor load a script, font, or image;
- reach the debugger, read memory, or ask for any value beyond its inputs;
- read the page, its storage, or other tabs, or run code in the page;
- make the debugger evaluate anything: `select` is a path;
- hang the page: a draw that takes longer than 2 seconds is stopped, and
  so is a live renderer that stops answering for 2 seconds.

Live renderers have the same restrictions. WebGL loads no external data;
events arrive as numbers and flags, and frames reach the page as pixels.

A renderer can still misrepresent its input data. Each card identifies
the renderer and its source.

## Limits

| What | Limit |
|---|---|
| A renderer | 256 KiB |
| A `visualize` | 64 inputs |
| A drawing's inputs | 65,536 values, nested 16 deep |
| `bytes(PTR, LEN)` and numbers read at once | 64 MiB for one drawing |
| Time to draw | 2 seconds |
| A picture | 100,000 shapes, groups nested 32 deep, 4,096 characters of text each, 16 Mpx of images, 2,000,000 points |
| Draws at once | 4 per tab |
| Live renderers | 4 at once per tab, only on screen; ended after 2 seconds without an answer |
| A live canvas | the card's width, at most 2 device pixels a CSS pixel and 4,096 pixels a side |
