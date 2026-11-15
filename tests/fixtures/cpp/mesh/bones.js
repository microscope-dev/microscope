// @ts-check




microscope.live((canvas, input) => {
  const context = canvas.getContext("2d");
  if (context === null) {
    throw new Error("no 2-D context");
  }
  const draw = context;
  let bones = input.bones.slice(0, Number(input.count));
  let zoom = 1;
  /** @type {number | null} */
  let hovered = null;


  function place(bone) {
    const unit = (Math.min(canvas.width, canvas.height) / 5) * zoom;
    return [canvas.width / 2 + bone.x * unit, canvas.height / 2 - bone.y * unit];
  }

  function nearest(x, y) {
    let best = null;
    let bestDistance = (16 * Math.max(1, canvas.width / 800)) ** 2;
    bones.forEach((bone, index) => {
      const [bx, by] = place(bone);
      const distance = (bx - x) ** 2 + (by - y) ** 2;
      if (distance < bestDistance) {
        best = index;
        bestDistance = distance;
      }
    });
    return best;
  }

  function caption() {
    microscope.caption(`${bones.length} bones · zoom ${zoom.toFixed(2)}`);
  }
  caption();

  return {
    frame() {
      const theme = microscope.theme;
      draw.fillStyle = theme.surface;
      draw.fillRect(0, 0, canvas.width, canvas.height);
      draw.lineWidth = Math.max(2, canvas.width / 300);
      draw.strokeStyle = theme.ink2;
      for (const bone of bones) {
        const parent = bones[bone.parent];
        if (parent !== undefined) {
          const [x1, y1] = place(parent);
          const [x2, y2] = place(bone);
          draw.beginPath();
          draw.moveTo(x1, y1);
          draw.lineTo(x2, y2);
          draw.stroke();
        }
      }
      bones.forEach((bone, index) => {
        const [x, y] = place(bone);
        draw.fillStyle = index === hovered ? theme.series[1] : theme.series[0];
        draw.beginPath();
        draw.arc(x, y, Math.max(5, canvas.width / 120), 0, Math.PI * 2);
        draw.fill();
      });
    },
    update(next) {
      bones = next.bones.slice(0, Number(next.count));
      caption();
    },
    pointer(event) {
      if (event.type === "wheel") {
        zoom = Math.min(8, Math.max(0.25, zoom * Math.exp(-event.wheel * 0.001)));
        caption();
        microscope.redraw();
        return;
      }
      const found = event.type === "leave" ? null : nearest(event.x, event.y);
      if (found !== hovered) {
        hovered = found;
        microscope.redraw();
      }
      const bone = found === null ? undefined : bones[found];
      microscope.hint(bone === undefined ? null : `${bone.name}: (${bone.x}, ${bone.y})`);
      if (event.type === "down" && found !== null) {
        microscope.select(`bones[${found}]`);
      }
    },
  };
});
