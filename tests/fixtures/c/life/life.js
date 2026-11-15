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
