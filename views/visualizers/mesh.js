// @ts-check
// The built-in mesh viewer: vertices and indices, as a program keeps them,
// drawn in 3-D with WebGL2. Drag to turn it, use the wheel to zoom, and
// press W for wireframe.
//
//   vertices    the vertex bytes, as bytes(&vertices[0], len * sizeof(Vertex))
//   stride      the bytes of one vertex, as sizeof(Vertex)
//   position    where its position's three f32s sit, as offsetof(Vertex, position)
//   normal      optional: where its normal's three f32s sit; without one,
//               each face is shaded by its own normal
//   color       optional: where its four bytes of RGBA sit
//   indices     optional: the index bytes; without them, vertices are taken
//               in order
//   index_size  2 or 4, the bytes of one index (4 by default)
//   primitive   optional: "triangles" (the default), "lines", or "points"
//   transform   optional: 16 numbers, a column-major model matrix
//   up          optional: "y" (the default) or "z"
//
// Numbers are little-endian. A vertex whose position is not finite, an
// index past the last vertex, or indices that are not whole primitives are
// named in the caption, and nothing is drawn: a mesh drawn around its
// problem would look fine. Hover names the nearest vertex. The camera fits
// the first mesh and keeps its place across stops.

microscope.live((canvas, input) => {
  const context = canvas.getContext("webgl2", { antialias: true });
  if (context === null) {
    throw new Error("WebGL is unavailable in this browser");
  }
  const gl = context;
  const program = link(gl);
  const at = {
    position: gl.getAttribLocation(program, "position"),
    normal: gl.getAttribLocation(program, "normal"),
    color: gl.getAttribLocation(program, "color"),
  };
  const uniform = (name) => gl.getUniformLocation(program, name);
  const u = {
    model: uniform("model"),
    view: uniform("view"),
    projection: uniform("projection"),
    normalMatrix: uniform("normalMatrix"),
    hasNormal: uniform("hasNormal"),
    mode: uniform("mode"),
    eye: uniform("eye"),
    tint: uniform("tint"),
  };
  const buffers = {
    position: gl.createBuffer(),
    normal: gl.createBuffer(),
    color: gl.createBuffer(),
    indices: gl.createBuffer(),
    edges: gl.createBuffer(),
  };
  const vao = gl.createVertexArray();

  let mesh = read(input);
  /** @type {Camera | null} */
  let camera = null;
  let wireframe = false;
  /** @type {{ x: number, y: number } | null} */
  let hover = null;
  let edgeCount = -1;

  function upload() {
    gl.bindVertexArray(vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, buffers.position);
    gl.bufferData(gl.ARRAY_BUFFER, mesh.positions, gl.STATIC_DRAW);
    gl.enableVertexAttribArray(at.position);
    gl.vertexAttribPointer(at.position, 3, gl.FLOAT, false, 0, 0);
    if (mesh.normals === null) {
      gl.disableVertexAttribArray(at.normal);
    } else {
      gl.bindBuffer(gl.ARRAY_BUFFER, buffers.normal);
      gl.bufferData(gl.ARRAY_BUFFER, mesh.normals, gl.STATIC_DRAW);
      gl.enableVertexAttribArray(at.normal);
      gl.vertexAttribPointer(at.normal, 3, gl.FLOAT, false, 0, 0);
    }
    if (mesh.colors === null) {
      gl.disableVertexAttribArray(at.color);
    } else {
      gl.bindBuffer(gl.ARRAY_BUFFER, buffers.color);
      gl.bufferData(gl.ARRAY_BUFFER, mesh.colors, gl.STATIC_DRAW);
      gl.enableVertexAttribArray(at.color);
      gl.vertexAttribPointer(at.color, 4, gl.UNSIGNED_BYTE, true, 0, 0);
    }
    gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, buffers.indices);
    gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, mesh.indices, gl.STATIC_DRAW);
    gl.bindVertexArray(null);
    edgeCount = -1;
    if (camera === null && mesh.problem === null && mesh.count > 0) {
      camera = fit(mesh);
    }
    microscope.caption(mesh.caption);
  }

  /** The edges of every triangle, for the wireframe, made when first shown. */
  function edges() {
    if (edgeCount < 0) {
      const lines = new Uint32Array(mesh.indices.length * 2);
      for (let i = 0; i + 2 < mesh.indices.length; i += 3) {
        const [a, b, c] = [mesh.indices[i], mesh.indices[i + 1], mesh.indices[i + 2]];
        lines.set([a, b, b, c, c, a], i * 2);
      }
      gl.bindVertexArray(vao);
      gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, buffers.edges);
      gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, lines, gl.STATIC_DRAW);
      gl.bindVertexArray(null);
      edgeCount = lines.length;
    }
    return edgeCount;
  }

  /** @param {Camera} pose */
  function matrices(pose) {
    const width = canvas.width;
    const height = canvas.height;
    const { yaw, pitch, distance, center, radius } = pose;
    const eye = [
      center[0] + distance * Math.cos(pitch) * Math.sin(yaw),
      center[1] + distance * Math.sin(pitch),
      center[2] + distance * Math.cos(pitch) * Math.cos(yaw),
    ];
    const near = Math.max(distance - radius * 2, distance / 1000, 1e-6);
    const far = distance + radius * 2;
    const aspect = width / Math.max(1, height);
    // The narrower side sees FIELD, so the fitted mesh shows whole on a
    // tall canvas too.
    const field = aspect >= 1 ? FIELD : 2 * Math.atan(Math.tan(FIELD / 2) / aspect);
    return {
      eye,
      view: lookAt(eye, center),
      projection: perspective(field, aspect, near, far),
    };
  }

  function frame() {
    gl.viewport(0, 0, canvas.width, canvas.height);
    const [r, g, b] = rgb(microscope.theme.surface);
    gl.clearColor(r, g, b, 1);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
    if (camera === null || mesh.problem !== null || mesh.count === 0) {
      return;
    }
    const { eye, view, projection } = matrices(camera);
    if (hover !== null) {
      nearest(hover, multiply(projection, multiply(view, mesh.model)));
      hover = null;
    }
    gl.enable(gl.DEPTH_TEST);
    gl.useProgram(program);
    gl.bindVertexArray(vao);
    gl.uniformMatrix4fv(u.model, false, mesh.model);
    gl.uniformMatrix4fv(u.view, false, view);
    gl.uniformMatrix4fv(u.projection, false, projection);
    gl.uniformMatrix3fv(u.normalMatrix, false, normalMatrix(mesh.model));
    gl.uniform1i(u.hasNormal, mesh.normals === null ? 0 : 1);
    gl.uniform3fv(u.eye, eye);
    const tint = rgb(microscope.theme.series[0]);
    if (mesh.colors === null) {
      gl.vertexAttrib4f(at.color, tint[0], tint[1], tint[2], 1);
    }
    const type = mesh.wide ? gl.UNSIGNED_INT : gl.UNSIGNED_SHORT;
    if (mesh.primitive === "triangles" && !wireframe) {
      gl.uniform1i(u.mode, LIT);
      gl.drawElements(gl.TRIANGLES, mesh.indices.length, type, 0);
    } else if (mesh.primitive === "triangles") {
      gl.uniform1i(u.mode, INK);
      const ink = rgb(microscope.theme.ink);
      gl.uniform4f(u.tint, ink[0], ink[1], ink[2], 1);
      const count = edges();
      gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, buffers.edges);
      gl.drawElements(gl.LINES, count, gl.UNSIGNED_INT, 0);
      gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, buffers.indices);
    } else {
      gl.uniform1i(u.mode, COLORED);
      gl.drawElements(
        mesh.primitive === "lines" ? gl.LINES : gl.POINTS,
        mesh.indices.length,
        type,
        0,
      );
    }
    gl.bindVertexArray(null);
  }

  /** Names the vertex nearest the pointer, within a few pixels. */
  function nearest(pointer, transform) {
    const { positions, count } = mesh;
    const width = canvas.width;
    const height = canvas.height;
    let best = -1;
    let bestDistance = (12 * Math.max(1, width / 1000)) ** 2;
    let bestDepth = Number.POSITIVE_INFINITY;
    for (let i = 0; i < count; i++) {
      const x = positions[i * 3];
      const y = positions[i * 3 + 1];
      const z = positions[i * 3 + 2];
      const w = transform[3] * x + transform[7] * y + transform[11] * z + transform[15];
      if (w <= 0) continue;
      const sx =
        ((transform[0] * x + transform[4] * y + transform[8] * z + transform[12]) / w + 1) *
        0.5 *
        width;
      const sy =
        (1 - (transform[1] * x + transform[5] * y + transform[9] * z + transform[13]) / w) *
        0.5 *
        height;
      const d = (sx - pointer.x) ** 2 + (sy - pointer.y) ** 2;
      const depth = (transform[2] * x + transform[6] * y + transform[10] * z + transform[14]) / w;
      // Of the vertices about as near, the one in front.
      if (d < bestDistance - 4 || (d <= bestDistance + 4 && depth < bestDepth)) {
        best = i;
        bestDistance = d;
        bestDepth = depth;
      }
    }
    if (best < 0) {
      microscope.hint(null);
      return;
    }
    const [x, y, z] = positions.slice(best * 3, best * 3 + 3);
    microscope.hint(`vertex ${best}: (${short(x)}, ${short(y)}, ${short(z)})`);
  }

  upload();
  let dragging = false;
  return {
    frame,
    update(next) {
      mesh = read(next);
      upload();
    },
    pointer(event) {
      if (event.type === "down") {
        dragging = (event.buttons & 1) !== 0;
        microscope.hint(null);
      } else if (event.type === "up") {
        dragging = false;
      } else if (event.type === "leave") {
        hover = null;
      } else if (event.type === "wheel" && camera !== null) {
        camera.distance = Math.min(
          camera.radius * 100,
          Math.max(camera.radius * 0.05, camera.distance * Math.exp(event.wheel * 0.001)),
        );
        microscope.redraw();
      } else if (event.type === "move" && dragging && camera !== null) {
        camera.yaw -= event.dx * 0.01;
        camera.pitch = Math.min(1.55, Math.max(-1.55, camera.pitch + event.dy * 0.01));
        microscope.redraw();
      } else if (event.type === "move" && event.buttons === 0) {
        hover = { x: event.x, y: event.y };
        microscope.redraw();
      }
    },
    key(event) {
      if (event.key === "w" || event.key === "W") {
        wireframe = !wireframe;
        microscope.redraw();
      }
    },
  };
});

/** The vertical field of view, in radians. */
const FIELD = Math.PI / 4;
// How a fragment is colored: lit by the eye, its vertex color, or ink.
const LIT = 0;
const COLORED = 1;
const INK = 2;
const Z_UP = [1, 0, 0, 0, 0, 0, -1, 0, 0, 1, 0, 0, 0, 0, 0, 1];

/**
 * @typedef {{ yaw: number, pitch: number, distance: number, center: number[], radius: number }} Camera
 */

/** The mesh in `input`, ready for the GPU, and what is wrong with it. */
function read(input) {
  const vertices = input.vertices ?? new Uint8Array();
  if (!(vertices instanceof Uint8Array)) {
    throw new TypeError("`vertices` is bytes, as bytes(&vertices[0], len * sizeof(Vertex))");
  }
  const stride = whole(input.stride, "stride");
  if (stride <= 0) {
    throw new RangeError("`stride` is the bytes of one vertex, more than 0");
  }
  const position = whole(input.position ?? 0, "position");
  const normal = input.normal === undefined ? null : whole(input.normal, "normal");
  const color = input.color === undefined ? null : whole(input.color, "color");
  for (const [name, offset, size] of [
    ["position", position, 12],
    ["normal", normal, 12],
    ["color", color, 4],
  ]) {
    if (offset !== null && offset + size > stride) {
      throw new RangeError(`\`${name}\` at ${offset} does not fit in a ${stride}-byte vertex`);
    }
  }
  const primitive = input.primitive ?? "triangles";
  if (!["triangles", "lines", "points"].includes(primitive)) {
    throw new TypeError('`primitive` is "triangles", "lines", or "points"');
  }
  const up = input.up ?? "y";
  if (up !== "y" && up !== "z") {
    throw new TypeError('`up` is "y" or "z"');
  }
  let model = up === "z" ? Z_UP : identity();
  if (input.transform !== undefined) {
    const numbers = Array.from(input.transform, Number);
    if (numbers.length !== 16 || !numbers.every((number) => Number.isFinite(number))) {
      throw new TypeError("`transform` is 16 finite numbers, a column-major matrix");
    }
    model = multiply(model, numbers);
  }

  const count = Math.floor(vertices.length / stride);
  const view = new DataView(vertices.buffer, vertices.byteOffset, vertices.byteLength);
  const positions = new Float32Array(count * 3);
  const normals = normal === null ? null : new Float32Array(count * 3);
  const colors = color === null ? null : new Uint8Array(count * 4);
  const problems = [];
  if (vertices.length % stride !== 0) {
    problems.push(`${vertices.length} bytes are not whole ${stride}-byte vertices`);
  }
  for (let i = 0; i < count; i++) {
    const base = i * stride;
    for (let k = 0; k < 3; k++) {
      positions[i * 3 + k] = view.getFloat32(base + position + k * 4, true);
      if (normals !== null && normal !== null) {
        normals[i * 3 + k] = view.getFloat32(base + normal + k * 4, true);
      }
    }
    if (colors !== null && color !== null) {
      colors.set(vertices.subarray(base + color, base + color + 4), i * 4);
    }
  }
  for (let i = 0; i < count; i++) {
    const [x, y, z] = positions.subarray(i * 3, i * 3 + 3);
    const at = `vertex ${i} is at (${short(x)}, ${short(y)}, ${short(z)})`;
    if (![x, y, z].every((coordinate) => Number.isFinite(coordinate))) {
      problems.push(at);
      break;
    }
    // The GPU places it in floats, which a large transform can overflow.
    const placed = [0, 1, 2].map(
      (row) => model[row] * x + model[4 + row] * y + model[8 + row] * z + model[12 + row],
    );
    if (!placed.every((value) => Number.isFinite(Math.fround(value)))) {
      problems.push(
        `${at}, which its transform carries to (${placed.map((value) => short(value)).join(", ")}), past a float's range`,
      );
      break;
    }
  }

  const size = input.index_size ?? 4;
  if (size !== 2 && size !== 4) {
    throw new TypeError("`index_size` is 2 or 4");
  }
  let indices;
  if (input.indices === undefined) {
    indices =
      count <= 0xffff
        ? Uint16Array.from({ length: count }, (_, i) => i)
        : Uint32Array.from({ length: count }, (_, i) => i);
  } else {
    const bytes = input.indices;
    if (!(bytes instanceof Uint8Array)) {
      throw new TypeError("`indices` is bytes, as bytes(&indices[0], len * index_size)");
    }
    const n = Math.floor(bytes.length / size);
    const data = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    indices = size === 2 ? new Uint16Array(n) : new Uint32Array(n);
    for (let i = 0; i < n; i++) {
      indices[i] = size === 2 ? data.getUint16(i * 2, true) : data.getUint32(i * 4, true);
    }
    if (bytes.length % size !== 0) {
      problems.push(`${bytes.length} bytes are not whole ${size}-byte indices`);
    }
    for (let i = 0; i < n; i++) {
      if (indices[i] >= count) {
        problems.push(`index ${i} is ${indices[i]}, past the last vertex, ${count - 1}`);
        break;
      }
    }
  }
  const per = primitive === "triangles" ? 3 : primitive === "lines" ? 2 : 1;
  if (indices.length % per !== 0) {
    problems.push(`${grouped(indices.length)} indices are not whole ${primitive}`);
  }
  const shapes = Math.floor(indices.length / per);
  const counted =
    primitive === "points"
      ? `${grouped(shapes)} points`
      : `${grouped(count)} vertices · ${grouped(shapes)} ${primitive}`;
  const problem = problems.length > 0 ? problems.join(" · ") : null;
  return {
    count,
    positions,
    normals,
    colors,
    indices,
    wide: indices instanceof Uint32Array,
    primitive,
    model,
    problem,
    caption: problem === null ? counted : `${counted} · not drawn: ${problem}`,
  };
}

/** A camera that shows the whole mesh, looking a little down at it. */
function fit(mesh) {
  const lo = [Infinity, Infinity, Infinity];
  const hi = [-Infinity, -Infinity, -Infinity];
  const p = mesh.positions;
  const m = mesh.model;
  for (let i = 0; i < mesh.count; i++) {
    const x = p[i * 3];
    const y = p[i * 3 + 1];
    const z = p[i * 3 + 2];
    const w = [
      m[0] * x + m[4] * y + m[8] * z + m[12],
      m[1] * x + m[5] * y + m[9] * z + m[13],
      m[2] * x + m[6] * y + m[10] * z + m[14],
    ];
    for (let k = 0; k < 3; k++) {
      lo[k] = Math.min(lo[k], w[k]);
      hi[k] = Math.max(hi[k], w[k]);
    }
  }
  const center = lo.map((low, k) => (low + hi[k]) / 2);
  const radius = Math.max(1e-6, Math.hypot(hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]) / 2);
  return { yaw: 0.6, pitch: 0.5, distance: (radius / Math.sin(FIELD / 2)) * 1.05, center, radius };
}

function whole(value, name) {
  const number = typeof value === "bigint" ? Number(value) : value;
  if (!Number.isInteger(number) || number < 0) {
    throw new TypeError(`\`${name}\` is a byte count or offset, a whole number`);
  }
  return number;
}

const short = (value) =>
  Number.isFinite(value) ? String(Number(value.toPrecision(5))) : String(value);
const grouped = (value) => value.toLocaleString("en-US");

/** A `#rrggbb` color as three numbers from 0 to 1. */
function rgb(color) {
  return [1, 3, 5].map((at) => Number.parseInt(color.slice(at, at + 2), 16) / 255);
}

function identity() {
  return [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1];
}

/** a × b, both column-major. */
function multiply(a, b) {
  const out = Array.from({ length: 16 }, () => 0);
  for (let column = 0; column < 4; column++) {
    for (let row = 0; row < 4; row++) {
      let sum = 0;
      for (let k = 0; k < 4; k++) {
        sum += a[k * 4 + row] * b[column * 4 + k];
      }
      out[column * 4 + row] = sum;
    }
  }
  return out;
}

function perspective(field, aspect, near, far) {
  const f = 1 / Math.tan(field / 2);
  return [
    f / aspect,
    0,
    0,
    0,
    0,
    f,
    0,
    0,
    0,
    0,
    (far + near) / (near - far),
    -1,
    0,
    0,
    (2 * far * near) / (near - far),
    0,
  ];
}

function sub(a, b) {
  return [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
}

function cross(a, b) {
  return [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
}

function unit(a) {
  const length = Math.hypot(a[0], a[1], a[2]) || 1;
  return [a[0] / length, a[1] / length, a[2] / length];
}

function dot(a, b) {
  return a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
}

function lookAt(eye, center) {
  const z = unit(sub(eye, center));
  const x = unit(cross([0, 1, 0], z));
  const y = cross(z, x);
  return [
    x[0],
    y[0],
    z[0],
    0,
    x[1],
    y[1],
    z[1],
    0,
    x[2],
    y[2],
    z[2],
    0,
    -dot(x, eye),
    -dot(y, eye),
    -dot(z, eye),
    1,
  ];
}

/** The inverse transpose of the model's upper 3×3, for normals. */
function normalMatrix(m) {
  const [a, b, c, d, e, f, g, h, i] = [m[0], m[1], m[2], m[4], m[5], m[6], m[8], m[9], m[10]];
  const A = e * i - f * h;
  const B = -(d * i - f * g);
  const C = d * h - e * g;
  const determinant = a * A + b * B + c * C || 1;
  // The inverse's transpose is the cofactors over the determinant.
  return [
    A,
    B,
    C,
    -(b * i - c * h),
    a * i - c * g,
    -(a * h - b * g),
    b * f - c * e,
    -(a * f - c * d),
    a * e - b * d,
  ].map((value) => value / determinant);
}

/** The program every primitive draws with. */
function link(gl) {
  const compile = (type, source) => {
    // oxlint-disable-next-line no-inline-comments -- a JSDoc type cast has to sit inline
    const shader = /** @type {WebGLShader} */ (gl.createShader(type));
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
      throw new Error(`a shader did not compile: ${gl.getShaderInfoLog(shader)}`);
    }
    return shader;
  };
  // oxlint-disable-next-line no-inline-comments -- a JSDoc type cast has to sit inline
  const program = /** @type {WebGLProgram} */ (gl.createProgram());
  gl.attachShader(
    program,
    compile(
      gl.VERTEX_SHADER,
      `#version 300 es
in vec3 position;
in vec3 normal;
in vec4 color;
uniform mat4 model;
uniform mat4 view;
uniform mat4 projection;
uniform mat3 normalMatrix;
out vec3 world;
out vec3 turned;
out vec4 shade;
void main() {
  vec4 placed = model * vec4(position, 1.0);
  world = placed.xyz;
  turned = normalMatrix * normal;
  shade = color;
  gl_Position = projection * view * placed;
  gl_PointSize = 3.0;
}`,
    ),
  );
  gl.attachShader(
    program,
    compile(
      gl.FRAGMENT_SHADER,
      `#version 300 es
precision highp float;
in vec3 world;
in vec3 turned;
in vec4 shade;
uniform bool hasNormal;
uniform int mode;
uniform vec3 eye;
uniform vec4 tint;
out vec4 color;
void main() {
  if (mode != ${LIT}) {
    color = mode == ${INK} ? tint : shade;
    return;
  }
  vec4 base = shade;
  vec3 toEye = normalize(eye - world);
  // A given normal that points away from the eye shades its face dark, so
  // flipped normals show; a face's own normal always faces the eye.
  float facing = hasNormal
    ? dot(normalize(turned), toEye)
    : abs(dot(normalize(cross(dFdx(world), dFdy(world))), toEye));
  color = vec4(base.rgb * (0.2 + 0.8 * max(facing, 0.0)), base.a);
}`,
    ),
  );
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    throw new Error(`the shaders did not link: ${gl.getProgramInfoLog(program)}`);
  }
  return program;
}
