/**
 * The WebGL layer: buffers in, picture out.
 *
 * Deliberately the thinnest part of the package. It owns GPU resources and two
 * draw commands and nothing else — no interaction, no layout, no opinion about
 * what a value means. Everything it needs has already been decided by
 * `channels.ts` (what colour, what size) and `camera.ts` (what is on screen), so
 * this file stays small enough to be checked by reading it, which matters for
 * the one piece that cannot be unit-tested without a GPU.
 *
 * Two decisions worth knowing:
 *
 * - **Nodes are instanced quads, not `gl.POINTS`.** Point sprites are less code,
 *   but `gl_PointSize` is capped by the implementation (often 64–255px), so a
 *   hub would silently stop growing. A quad has no such ceiling.
 * - **Node radii are screen pixels, held constant under zoom.** Zooming a graph
 *   should reveal structure, not inflate the dots into overlapping blobs. It also
 *   keeps hit-testing honest: a node's clickable area is what you can see.
 */

import createREGL, { type Regl } from 'regl';

import { hexToRgbFloat } from './channels';
import { backgroundRgba, normalizeHex, type Theme } from './theme';

export interface RenderBuffers {
  readonly nodeCount: number;
  /** Interleaved world coordinates, 2 per node. */
  readonly positions: Float32Array;
  /** Screen-pixel radius, 1 per node. */
  readonly sizes: Float32Array;
  /** RGB in 0..1, 3 per node. */
  readonly colors: Float32Array;
  /** Node index pairs, 2 per edge. */
  readonly edges: Uint32Array;
  /** Node to ring as the hover affordance, or -1 for none. */
  readonly highlight: number;
}

export interface CameraUniforms {
  readonly cx: number;
  readonly cy: number;
  readonly scale: number;
  readonly width: number;
  readonly height: number;
}

/**
 * Everything both draw commands read, set once per `draw()`.
 *
 * Held here and closed over rather than threaded through regl's props, because
 * regl's prop generics do not flow into uniform callbacks without annotating
 * every one of them — and a zero-argument closure satisfies the same signature
 * with no types to fight. It is safe to share: `draw()` assigns this and issues
 * both commands synchronously, in one call, on one thread.
 */
interface Frame {
  center: [number, number];
  scale: number;
  /** Viewport in CSS pixels — the space the projection works in. */
  viewport: [number, number];
  /** Drawing buffer in device pixels — the space `gl.viewport` works in. */
  buffer: [number, number];
  edgeColor: [number, number, number];
  edgeAlpha: number;
  instances: number;
  /** Hovered node: world position and screen radius, for the ring. */
  hi: [number, number];
  hiRadius: number;
  hiColor: [number, number, number];
}

/** Screen-space geometry shared by both shaders. */
const PROJECT = `
  uniform vec2 uCenter;
  uniform float uScale;
  uniform vec2 uViewport;

  // World -> clip. No y flip: clip space is y-up like world space, so the two
  // agree directly. The flip lives only in camera.ts, where world meets *screen*
  // pixels — which is the space pointer events arrive in.
  vec2 project(vec2 world) {
    vec2 screen = (world - uCenter) * uScale;
    return vec2(screen.x / (uViewport.x * 0.5), screen.y / (uViewport.y * 0.5));
  }
`;

export interface Renderer {
  draw(buffers: RenderBuffers, cam: CameraUniforms, theme: Theme): void;
  destroy(): void;
}

export interface RendererOptions {
  /**
   * Keep the drawing buffer readable after compositing.
   *
   * Off by default because it costs the driver a copy every frame. Anything that
   * needs to *capture* the canvas has to turn it on: `toDataURL`, `readPixels`
   * and `canvas.toBlob` all come back blank without it, since the buffer is
   * cleared once the frame is composited. That covers the render tests and the
   * static-image escape hatch (`save_png`).
   */
  readonly preserveDrawingBuffer?: boolean;
}

export function createRenderer(
  canvas: HTMLCanvasElement,
  options: RendererOptions = {},
): Renderer {
  const regl: Regl = createREGL({
    canvas,
    attributes: {
      antialias: true,
      alpha: false,
      premultipliedAlpha: false,
      preserveDrawingBuffer: options.preserveDrawingBuffer ?? false,
    },
    // Required, not optional: the node quads are instanced, so without this regl
    // refuses the divisor and nothing draws. Universal in practice (WebGL 1 has
    // shipped it for a decade), but it has to be *asked for* — regl does not
    // enable extensions it was not told about, and the failure without it is an
    // exception at command-compile time rather than a missing picture.
    extensions: ['angle_instanced_arrays'],
    // Node counts pass 65 535 quickly, so 32-bit element indices stop being
    // optional past a few thousand nodes. Requested rather than required: a
    // context without it still renders, up to that ceiling.
    optionalExtensions: ['oes_element_index_uint'],
  });

  // One unit quad, reused for every node via instancing.
  const corners = regl.buffer([
    [-1, -1],
    [1, -1],
    [-1, 1],
    [1, 1],
  ]);

  // Re-assigned (not `subdata`-ed) on every draw: the buffer objects are callable
  // and reallocate when the data grows, which `subdata` will not do — so a graph
  // that gains nodes would otherwise write past the end and draw stale geometry.
  const positions = regl.buffer({ usage: 'dynamic', type: 'float' });
  const sizes = regl.buffer({ usage: 'dynamic', type: 'float' });
  const colors = regl.buffer({ usage: 'dynamic', type: 'float' });
  const elements = regl.elements({ usage: 'dynamic', primitive: 'lines' });

  const frame: Frame = {
    center: [0, 0],
    scale: 1,
    viewport: [1, 1],
    buffer: [1, 1],
    edgeColor: [0, 0, 0],
    // Faint by design: at any real density the picture is made by how many edges
    // overlap, not by any one of them.
    edgeAlpha: 0.35,
    instances: 0,
    hi: [0, 0],
    hiRadius: 0,
    hiColor: [0, 0, 0],
  };
  const glViewport = () => ({ x: 0, y: 0, width: frame.buffer[0], height: frame.buffer[1] });

  const drawEdges = regl({
    vert: `
      precision highp float;
      attribute vec2 aPosition;
      ${PROJECT}
      void main() {
        gl_Position = vec4(project(aPosition), 0.0, 1.0);
      }
    `,
    frag: `
      precision highp float;
      uniform vec3 uColor;
      uniform float uAlpha;
      void main() { gl_FragColor = vec4(uColor, uAlpha); }
    `,
    attributes: { aPosition: { buffer: positions, size: 2 } },
    uniforms: {
      uCenter: () => frame.center,
      uScale: () => frame.scale,
      uViewport: () => frame.viewport,
      uColor: () => frame.edgeColor,
      uAlpha: () => frame.edgeAlpha,
    },
    elements,
    primitive: 'lines',
    // Set explicitly from the drawing buffer every draw. regl samples the canvas
    // size when the context is created and again inside `regl.frame`; these
    // commands are invoked directly from an animation frame we own, so without
    // this the viewport stays at the canvas element's 300x150 default — content
    // lands in the bottom-left corner (GL's origin) at the wrong aspect.
    viewport: glViewport,
    blend: {
      enable: true,
      func: { srcRGB: 'src alpha', srcAlpha: 1, dstRGB: 'one minus src alpha', dstAlpha: 1 },
    },
    depth: { enable: false },
  });

  const drawNodes = regl({
    vert: `
      precision highp float;
      attribute vec2 aCorner;
      attribute vec2 aPosition;
      attribute float aSize;
      attribute vec3 aColor;
      varying vec2 vCorner;
      varying vec3 vColor;
      ${PROJECT}
      void main() {
        vCorner = aCorner;
        vColor = aColor;
        // aSize is a screen-pixel radius, so the quad is offset in clip units
        // derived from the viewport rather than scaled with the camera.
        vec2 offset = aCorner * aSize / (uViewport * 0.5);
        gl_Position = vec4(project(aPosition) + offset, 0.0, 1.0);
      }
    `,
    frag: `
      precision highp float;
      varying vec2 vCorner;
      varying vec3 vColor;
      void main() {
        // Round the quad into a disc with a feathered edge. Without the feather
        // every node reads as a tiny aliased square at small radii.
        float alpha = 1.0 - smoothstep(0.85, 1.0, length(vCorner));
        if (alpha <= 0.0) discard;
        gl_FragColor = vec4(vColor, alpha);
      }
    `,
    attributes: {
      aCorner: { buffer: corners, size: 2 },
      aPosition: { buffer: positions, size: 2, divisor: 1 },
      aSize: { buffer: sizes, size: 1, divisor: 1 },
      aColor: { buffer: colors, size: 3, divisor: 1 },
    },
    uniforms: {
      uCenter: () => frame.center,
      uScale: () => frame.scale,
      uViewport: () => frame.viewport,
    },
    count: 4,
    instances: () => frame.instances,
    primitive: 'triangle strip',
    viewport: glViewport,
    blend: {
      enable: true,
      func: { srcRGB: 'src alpha', srcAlpha: 1, dstRGB: 'one minus src alpha', dstAlpha: 1 },
    },
    depth: { enable: false },
  });

  // The hover affordance: a ring standing off the node rather than a fill, so it
  // marks the node without repainting it — the value the colour encodes stays
  // readable while hovered. Echoes the detection ellipse the site's SkyField uses.
  const drawHighlight = regl({
    vert: `
      precision highp float;
      attribute vec2 aCorner;
      uniform vec2 uNode;
      uniform float uRadius;
      varying vec2 vCorner;
      ${PROJECT}
      void main() {
        vCorner = aCorner;
        vec2 offset = aCorner * uRadius / (uViewport * 0.5);
        gl_Position = vec4(project(uNode) + offset, 0.0, 1.0);
      }
    `,
    frag: `
      precision highp float;
      varying vec2 vCorner;
      uniform vec3 uColor;
      void main() {
        float d = length(vCorner);
        // An annulus: opaque in a narrow band, transparent inside and out, with
        // both edges feathered so it does not alias into a polygon.
        float outer = 1.0 - smoothstep(0.86, 1.0, d);
        float inner = smoothstep(0.62, 0.76, d);
        float alpha = outer * inner;
        if (alpha <= 0.01) discard;
        gl_FragColor = vec4(uColor, alpha);
      }
    `,
    attributes: { aCorner: { buffer: corners, size: 2 } },
    uniforms: {
      uCenter: () => frame.center,
      uScale: () => frame.scale,
      uViewport: () => frame.viewport,
      uNode: () => frame.hi,
      uRadius: () => frame.hiRadius,
      uColor: () => frame.hiColor,
    },
    count: 4,
    primitive: 'triangle strip',
    viewport: glViewport,
    blend: {
      enable: true,
      func: { srcRGB: 'src alpha', srcAlpha: 1, dstRGB: 'one minus src alpha', dstAlpha: 1 },
    },
    depth: { enable: false },
  });

  return {
    draw(buffers, cam, theme) {
      positions(buffers.positions);
      sizes(buffers.sizes);
      colors(buffers.colors);

      // Refresh regl's cached context state — including the drawing buffer size —
      // after any canvas resize that happened outside a regl frame.
      regl.poll();

      frame.center = [cam.cx, cam.cy];
      frame.scale = cam.scale;
      frame.viewport = [cam.width, cam.height];
      frame.buffer = [canvas.width, canvas.height];
      frame.edgeColor = hexToRgbFloat(normalizeHex(theme.edge));
      frame.instances = buffers.nodeCount;

      regl.clear({ color: backgroundRgba(theme), depth: 1 });

      // Edges first: nodes sit on top of the structure that connects them.
      if (buffers.edges.length > 0) {
        elements({ data: buffers.edges, primitive: 'lines' });
        drawEdges();
      }
      if (buffers.nodeCount > 0) {
        drawNodes();
      }
      const h = buffers.highlight;
      if (h >= 0 && h < buffers.nodeCount) {
        frame.hi = [buffers.positions[h * 2]!, buffers.positions[h * 2 + 1]!];
        // Stand off the node so the ring reads as an annotation around it rather
        // than as a thicker node.
        frame.hiRadius = buffers.sizes[h]! + 6;
        frame.hiColor = hexToRgbFloat(normalizeHex(theme.ink));
        drawHighlight();
      }
    },
    destroy() {
      regl.destroy();
    },
  };
}
