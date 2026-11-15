```js
// @ts-check

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

```text
microscope-views 1


extend c life {
    visualize "life" {
        cells = bytes(&cells[0][0], sizeof(cells))
        columns = 64
        generation = generation
    }
}
```

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
