# Visualizers

A visualizer draws a value in the web page, `microscope web`: a chess position
as a board, a framebuffer as an image, samples as a plot, a mesh as a
model you can turn. A view says which
of a value's data the drawing needs, with `visualize` ([Views](views.md)),
and a **renderer**, a small JavaScript file, turns that data into a
picture. The page runs the renderer in a sandbox, checks the picture, and
draws it. The terminal and debug adapters never draw: a `visualize`
changes nothing they show.

Every renderer example on this page runs as one of microscope's tests.

## A first drawing

Conway's game of life keeps its board as bytes, one a cell:

```c
struct life {
    int generation;
    uint8_t cells[48][64];
};
```

Next to a view file, `life.views`, that names what a drawing of a `struct
life` needs:

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

goes the renderer that draws it, `life.js`, named for the drawing:

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

`microscope web --views life.views ./life` loads both. Stop anywhere a `struct
life` is in scope, or a pointer to one, and press Alt+V: the Drawings view
draws it, and draws it again at each stop, with the cells born since the
stop before in orange. Edit `life.js` and run Reload views from the
command palette (Ctrl+K) to see the change at once; nothing restarts.

`extend` adds the drawing to whatever view presents the type, so `print
life` in the terminal is unchanged. `cells = bytes(…)` hands over the
board's 3,072 bytes in one read, as a `Uint8Array`.

## The Drawings view

The Drawings view (Alt+V, or Drawings beside Source, Disassembly, and
Memory) shows a card for each argument and local of the frame shown that a
view draws, then for each value pinned to it. A pointer draws as what it
points to, so a `&mut Board` or a `struct life *` has its target's
drawings. The number beside Drawings counts the cards.

- **Draw** on a value row pins the value with the drawings its view
  offers. **Draw as…** on any value row, or on a card, pins it with any
  renderer; that renderer draws the value itself as its `values` input,
  as `visualize "NAME" { values = self }` would.
- **The link holds the view.** `view=drawings` shows it, and each pinned
  value is `d=PATH` or `d=PATH~NAME` for a chosen renderer, so a link
  shows the same drawings.
- **A value with several drawings** shows each as a tab of its card, and
  the tab chosen stays chosen at later stops.
- **Across stops,** a card draws again at each stop the tab shows, and
  hands the renderer what it drew at the stop before as `previous`, so it
  can mark what changed. While the program runs, and until the next stop's
  drawing is ready, the last drawing stays, dimmed, and says which stop it
  is of.
- **The latest stop wins.** When stops come faster than a card draws, as
  while F10 is held, the card draws one stop at a time, at most once a
  frame, skipping to the latest, and never replaces a drawing with one of
  an earlier stop.
- **Only cards on screen draw.** A card scrolled away draws when it comes
  back. A card draws again when its width changes, or the page's colors
  do, the system's switch to dark included.
- **Parts.** A shape with `select` opens that part of the value as a row,
  which can be expanded and watched, and its `title` is its tooltip and
  accessible name. Over a titled image, the tooltip also names the pixel
  under the pointer: `cells: column 31, row 20`.
- **Table and Copy CSV.** Table lists every value the drawing's inputs
  hold, a row each, as `cells[1311]` and `1`, a screenful at a time even
  for a million. Copy CSV copies them as `path,value` lines. Both write
  each number exactly: integers whole, and floats as the shortest text
  that reads back as the same number, an `f32` as an `f32`. Captions
  round to six digits; titles and the Table never do.
- **Many shapes.** A picture of more than 2,000 shapes is drawn on a
  canvas rather than as SVG, with the same tooltips and clicks.
- **A card that cannot draw says why,** and draws nothing: an input that
  could not be read, with which and why; a renderer that threw, with its
  file, line, and message; one that took longer than 2 seconds, which is
  stopped; a picture the page refuses, with which shape and why; and a
  renderer nobody provides.
- **Live drawings,** such as a mesh, draw on a canvas that follows the
  pointer while the card is on screen (see Live renderers). Clicking the
  canvas gives it the keys, all but the debugger's function keys, and
  Escape gives them back. A live renderer that stops answering for 2
  seconds, or whose WebGL context is lost, is stopped, and its card
  offers to restart it.

## Inputs

Each input of a `visualize` is evaluated as a view's field is, through
every view, and arrives as a plain JavaScript value decided by its type
alone, never by its value: a `uint64_t` is always a bigint, even when it
is 3.

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

- **Never partial.** When any part of any input cannot be read, the
  renderer is not called, and the card says which part and why. A renderer
  never has to tell a real 0 from a missing one.
- **Pointers stay addresses.** An input that wants what a pointer points
  to says `*p`, or names a value a view presents. A value a view
  presents is what the view presents, even when it is stored as a
  pointer, as a Go map is.
- **Bulk data is read at once.** `bytes(PTR, LEN)` is one read of memory,
  and a sequence of numbers that lies in one run of memory, as an array's,
  a `Vec`'s, a `std::vector`'s, or a numpy array's elements do, is read
  at once too. Both
  travel to the page as bytes, never as text.

## Renderers

A renderer is one script that calls `microscope.draw` once with a function,
or `microscope.live` for a live drawing (see Live renderers). The function
gets the inputs and a context, and returns a picture; it may be `async`. The only global it can use beyond the language and drawing is
`microscope`.

```text
microscope.draw((input, context) => picture)

context.previous   the inputs drawn for this value at the stop before, or null
context.paths      the part of the drawn value each input is, or null
context.width      the CSS pixels the card offers; a picture is scaled to fit
context.theme      "light" or "dark"
```

`context.paths` lets a renderer's shapes open the parts they draw
wherever its inputs come from. An input written as members and indices of
the value, such as `squares = mailbox`, has the path `"mailbox"`, so
square 28 selects `${paths.squares}[28]`; the value itself, as `self` or
as Draw as… hands it over, has `""`; anything else, such as `bytes(…)`,
`len * 2`, or a string, has null, and has no parts to open.

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
| `microscope.group` | `shapes`; `x?` and `y?` move them, and `rotate?` (degrees, clockwise) and `scale?` turn and size them about that point |
| `microscope.image` | `x`, `y`, `width`, `height`, and either `pixels` (RGBA, a `Uint8ClampedArray`), `columns`, and `rows`, or a `canvas`, an `OffscreenCanvas` the renderer drew; `smooth?` blends pixels when scaled, which stay sharp otherwise |

Every shape also takes:

- `fill` and `stroke`: a color, `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb()`,
  `hsl()`, or a CSS color name. A shape with neither is drawn in the
  page's ink, and lines and polylines by their stroke.
- `strokeWidth`, `opacity` (0 to 1), and `dash`, the lengths of dashes and
  gaps.
- `title`: hover text, and the shape's accessible name, so a screen reader
  reads `e4: White Pawn`.
- `select`: a part of the drawn value, as member names, `.`, and indices:
  `mailbox[21]`, `[3]`, or `a.b[2].c`. Clicking the shape, or pressing
  Enter on it, opens that part as a row. It is a path, never an
  expression: a renderer can never make the debugger evaluate anything
  else.

A shape with neither a `title` nor a `select`, in a group with neither,
lets the pointer through to the shapes beneath it, so a piece drawn on a
square leaves the square clickable, and marks drawn over a titled image
leave its pixels named.

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

- `microscope.theme` holds the page's colors in the theme it shows, each
  `#rrggbb`: `ink`,
  `ink2`, `ink3`, `paper`, `surface`, `line`, `accent`, `changed`,
  `good`, `bad`, and `series`, eight categorical colors in an order
  checked for color-blind readers. A card draws again when the theme
  changes.
- `microscope.same(a, b)` compares two input values deeply, typed arrays and
  bigints included, for marking what changed since `previous`.
- `microscope.color.scale(t, from, to)` blends from `from` to `to`, two `#rgb`
  or `#rrggbb` colors, `t` of the way, for heatmaps.

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

The page names a renderer's own file and line when it fails, in Chromium
and Firefox alike: a thrown error or a rejected promise shows as
`life.js:12:5: TypeError: …`. A renderer that does not parse, or calls
neither `microscope.draw` nor `microscope.live`, says so. A picture the page cannot show exactly as
described is refused whole, naming the first shape at fault, such as
`shape 3 (rect): fill is not a color`: unknown properties, numbers that
are not finite, colors that are not colors, path data outside SVG's
grammar, and pictures over the limits below.

To have an editor check a renderer, start it with `// @ts-check` and give
the editor `sdk/web/microscope-visualizer.d.ts`, which declares `microscope`.

## Live renderers

A picture is drawn once a stop. A **live** renderer instead keeps a
canvas of its own while its card is on screen, and draws into it whenever
it asks to: a model to turn, a level to fly through, bones to inspect. It
calls `microscope.live` in place of `microscope.draw`, with a function that gets
the canvas, an `OffscreenCanvas` sized to the card in device pixels, the
first stop's inputs, and a context, and returns what it does next:

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

Each function is optional, and each may be `async`. The canvas takes
`getContext("webgl2")` or `getContext("2d")`; WebGPU is not offered.

- **The page drives frames.** It calls `frame` from its own animation
  frame, one at a time, only after the renderer starts, gets a new stop,
  or is resized, after `microscope.redraw()`, and every frame while
  `microscope.animate(true)` holds, and never while the card is off screen or
  the tab hidden. Each frame reaches the page as an `ImageBitmap`, only
  pixels.
- **One worker for as long as the card is on screen.** A new stop calls
  `update` in the same worker, so a camera stays where it was. Scrolling
  the card away, closing the view, or switching programs ends the worker;
  scrolling back starts a new one with the inputs of then.
- **At most four run at once** in a tab; later cards wait for one to
  leave the screen.
- **A watchdog.** The page asks each live worker every second whether it
  still answers, once its functions return. One that does not within 2
  seconds, looping or awaiting what never settles, is ended, as is one
  that throws or loses its WebGL context, and its card says why and
  offers Restart.
- Headless Firefox has no WebGL at all, so a WebGL renderer fails there
  with its own message, as `mesh` says WebGL is unavailable.

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

microscope builds in eleven renderers, in `views/visualizers/`. Any `visualize`
can name one, and Draw as… offers every one for any value. They are
ordinary renderers, written against this page's API alone, so each is
also an example to copy.

| Renderer | Inputs | Draws |
|---|---|---|
| `line-plot` | `values`, or `series`, a map or record of up to 8 names to numbers; `x?`, `log?` | Past one value a pixel column, each column's first, lowest, highest, and last (M4), with a band for its range, so one spike in a million shows. NaN is a gap, ±∞ an arrow. A ghost is the line at the stop before. |
| `bar-chart` | `values` with `labels?`, or `entries`, a map or record (as `values` may be); `orientation?` (`"horizontal"` or `"vertical"`), `sort?` (`"value"`, `"label"`, or `"none"`) | A map sorted by value, since its order may be random, past 40 entries folding into Other with their count and total; an array in the program's order. A tick marks each bar at the stop before. |
| `scatter-plot` | `x` and `y`, or `values`, pairs or records of two numbers; `group?`, `labels?` | A dot per point, three groups in hues of their own; past 10,000, one image darker where more points fall. Up to 2,000 points, those that moved trail a line. |
| `histogram` | `values`, raw samples, or `counts` and `edges`; `bins?`, `log?` | Freedman–Diaconis bins, exact p50, p90, and p99 (type 7), and an outline of the stop before's counts. |
| `box-plot` | `groups` (or `values`), a map, record, or array of samples or of `{min, q1, median, q3, max}`; `labels?`; `error?`: `"sd"`, `"ci95"`, or numbers with `mean`; `error_label?` | Quartiles (type 7), Tukey whiskers, outliers, error bars beside each box, and a tick at each median of the stop before. |
| `donut-chart` | `values` with `labels?`, or `entries` (as `values` may be); none negative | The seven largest shares and Other, each titled with its change since the stop before. |
| `heatmap` | `values` and `columns`, or an array of rows; `row_labels?`, `column_labels?`, `scale?` (`"sequential"` or `"diverging"`), `log?` | Blue to red through gray when the values cross zero, NaN hatched, changed cells outlined; past 10,000 cells, one image of each pixel's largest cell. |
| `flame-graph` | `nodes`, records of `name`, `value`, and `parent`, or `stacks`, `"a;b;c"` to counts as pairs or a map; `values` for either | An icicle, root on top, each frame as wide as its total, colored by name, with frames whose total changed outlined. |
| `bitmap` | `pixels` (or `values`) and `columns`, or an array of rows; `format?` (`"gray8"`, `"rgba8"`, `"rgb565"`, `"bits"`) | The image at a whole zoom, sharp, with changed pixels outlined, or tinted when too small to outline. |
| `bits` | `values`, one integer or up to 4,096; `columns?`, `origin?` (`"top-left"` or `"bottom-left"`), `labels?`, `width?` | Each integer as a grid of its bits, its width from its type, with flipped bits outlined and its value in hexadecimal. |
| `mesh` (live) | `vertices` (bytes), `stride`, `position`; `normal?`, `color?` (offsets in a vertex), `indices?` (bytes) with `index_size?` (2 or 4), `primitive?` (`"triangles"`, `"lines"`, `"points"`), `transform?` (16 numbers, column-major), `up?` (`"y"` or `"z"`) | A model in WebGL2, lit from the eye, so flipped normals show dark, or shaded by its faces without normals. Drag turns it, the wheel zooms, and W shows its wireframe; hover names the nearest vertex. A position that is not finite, or that its transform carries past a float's range, or an index past the last vertex, is named in the caption and nothing is drawn. |

`mesh` reads a mesh where the program keeps it, with no JavaScript of
your own. For a C++ engine's `std::vector<Vertex>` and indices:

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

Only memory the process holds can be drawn: a mesh uploaded to the GPU
and freed exists only there, so draw the copy the loader keeps.

## Where renderers come from

A view calls the renderers of its own source before the built-in ones:

- a view file's are `NAME.js` files beside it, read with it, and read
  again by Reload views;
- a module's own views call the renderers the module carries in its
  `.debug_microscope_views` section. A C or C++ program carries one with
  `MICROSCOPE_VISUALIZER("board", "views/board.js");` from `microscope_views.h`,
  and a Rust program with `microscope_views::microscope_visualizer!("board",
  PATH);`;
- the built-ins come last.

`microscope views check PROGRAM` lists every renderer the views call, each as
its JavaScript, and fails when a drawing does not bind, such as one whose
input names no member. A renderer is never run outside the page: microscope
has no JavaScript engine of its own.

## What a renderer cannot do

A renderer may be hostile, as one carried by a program someone sent you
may be. The page runs each in a worker of its own, started by a hidden
frame whose origin is opaque and whose policy allows no request at all,
and the worker deletes every global but the language, timers, and drawing
before it runs the renderer. So a renderer cannot:

- send anything anywhere, nor load a script, font, or image;
- reach the debugger, read memory, or ask for any value beyond its inputs;
- read the page, its storage, or other tabs, or run code in the page;
- make the debugger evaluate anything: `select` is a path;
- hang the page: a draw that takes longer than 2 seconds is stopped, and
  so is a live renderer that stops answering for 2 seconds.

A live renderer is no different: its WebGL loads nothing, events reach it
as numbers and flags, and its frames reach the page as pixels alone.

What it can do is draw a misleading picture of data you are already
looking at; each card names its renderer and where it came from.

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
