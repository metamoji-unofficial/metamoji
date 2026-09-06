/**
 * Stroke geometry: bounds, simplification, and the variable-width outline.
 *
 * docs/05 §8 is explicit that the original app's ink geometry lives inside a
 * closed native component and is not recoverable, so this is a clean-room
 * design. It follows the doc's own recommendation: a per-stroke nested model of
 * `{ points: coords + pressure, penAttributes, bounds }`.
 */

import type { InkPoint, PenAttributes, Rect, Stroke } from "./types";

export function strokeBounds(points: InkPoint[], nominalWidth: number): Rect {
  if (points.length === 0) return { x: 0, y: 0, width: 0, height: 0 };

  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const pt of points) {
    if (pt.x < minX) minX = pt.x;
    if (pt.y < minY) minY = pt.y;
    if (pt.x > maxX) maxX = pt.x;
    if (pt.y > maxY) maxY = pt.y;
  }

  // Half the widest the stroke can get, plus a little slack for the round caps.
  const pad = nominalWidth * 1.5;
  return {
    x: minX - pad,
    y: minY - pad,
    width: maxX - minX + pad * 2,
    height: maxY - minY + pad * 2,
  };
}

export function recomputeBounds(stroke: Stroke): Stroke {
  return { ...stroke, bounds: strokeBounds(stroke.points, stroke.pen.width) };
}

/**
 * Width at a sample, given the pen's pressure sensitivity.
 *
 * At sensitivity 0 the width is constant; at 1 it ranges over [0.25w, 1.25w].
 * Highlighters ignore pressure entirely — a highlighter with a pressure taper
 * looks like a mistake rather than a highlighter.
 */
export function widthAt(pen: PenAttributes, pressure: number): number {
  if (pen.penType === "highlighter") return pen.width;
  const s = clamp(pen.pressureSensitivity, 0, 1);
  const p = clamp(pressure, 0, 1);
  return pen.width * (1 - s + s * (0.25 + p));
}

export function clamp(v: number, lo: number, hi: number): number {
  return v < lo ? lo : v > hi ? hi : v;
}

/**
 * Radial-distance filter: drops samples closer than `minDist` to the previous
 * kept one. Pointer devices happily emit several samples per pixel; keeping
 * them all bloats the document and makes the outline self-intersect.
 * The first and last samples are always kept.
 */
export function simplify(points: InkPoint[], minDist = 0.7): InkPoint[] {
  if (points.length <= 2) return points.slice();

  const out: InkPoint[] = [points[0]];
  const minDistSq = minDist * minDist;
  for (let i = 1; i < points.length - 1; i++) {
    const prev = out[out.length - 1];
    const dx = points[i].x - prev.x;
    const dy = points[i].y - prev.y;
    if (dx * dx + dy * dy >= minDistSq) out.push(points[i]);
  }
  out.push(points[points.length - 1]);
  return out;
}

/** A circular pen dab, centred on one sample. */
export interface StrokeCircle {
  kind: "circle";
  cx: number;
  cy: number;
  r: number;
}

/** The quad bridging two consecutive dabs, tangent to both. */
export interface StrokeQuad {
  kind: "quad";
  pts: [
    { x: number; y: number },
    { x: number; y: number },
    { x: number; y: number },
    { x: number; y: number },
  ];
}

export type StrokeShape = StrokeCircle | StrokeQuad;

/**
 * The variable-width stroke as a set of shapes to union: a circle at every
 * sample plus a bridging quad between each consecutive pair. Kept separate
 * from `buildStrokePath` so the geometry can be checked without a real
 * Canvas backend — see stroke.test.ts's self-overlap regression test.
 */
export function strokeOutlineShapes(stroke: Stroke): StrokeShape[] {
  const pts = stroke.points;
  const shapes: StrokeShape[] = [];

  for (const pt of pts) {
    shapes.push({ kind: "circle", cx: pt.x, cy: pt.y, r: widthAt(stroke.pen, pt.p) / 2 });
  }

  for (let i = 0; i < pts.length - 1; i++) {
    const a = pts[i];
    const b = pts[i + 1];
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    const len = Math.hypot(dx, dy);
    if (len < 1e-6) continue;
    dx /= len;
    dy /= len;
    // Perpendicular unit vector, scaled to each endpoint's half-width.
    const px = -dy;
    const py = dx;
    const ra = widthAt(stroke.pen, a.p) / 2;
    const rb = widthAt(stroke.pen, b.p) / 2;
    shapes.push({
      kind: "quad",
      pts: [
        { x: a.x + px * ra, y: a.y + py * ra },
        { x: b.x + px * rb, y: b.y + py * rb },
        { x: b.x - px * rb, y: b.y - py * rb },
        { x: a.x - px * ra, y: a.y - py * ra },
      ],
    });
  }

  return shapes;
}

/**
 * Builds the filled outline of a variable-width stroke.
 *
 * The pen tip is a circle at each sample, bridged to its neighbour by a quad
 * so fast strokes don't band between sparse samples. Each circle and quad is
 * its own closed subpath inside one Path2D, and a single nonzero-rule fill
 * unions them — so retracing the same spot (a tight scribble, a dot held in
 * place) always fills solid, with no seams from the overlap.
 *
 * An earlier version walked one offset side of the centreline and back down
 * the other, producing a single closed polygon. That's cheaper per sample,
 * but when the centreline loops back over itself tighter than the stroke's
 * own width, the two offset sides cross and the nonzero fill rule reads the
 * crossing as a hole — exactly the "line doesn't fill in" bug this avoids.
 */
export function buildStrokePath(stroke: Stroke): Path2D {
  const path = new Path2D();

  for (const shape of strokeOutlineShapes(stroke)) {
    if (shape.kind === "circle") {
      path.moveTo(shape.cx + shape.r, shape.cy);
      path.arc(shape.cx, shape.cy, shape.r, 0, Math.PI * 2);
    } else {
      path.moveTo(shape.pts[0].x, shape.pts[0].y);
      path.lineTo(shape.pts[1].x, shape.pts[1].y);
      path.lineTo(shape.pts[2].x, shape.pts[2].y);
      path.lineTo(shape.pts[3].x, shape.pts[3].y);
    }
    path.closePath();
  }

  return path;
}

/**
 * Distance from a point to the stroke's centreline, used for eraser hit tests
 * and lasso selection. Returns Infinity for an empty stroke.
 */
export function distanceToStroke(stroke: Stroke, x: number, y: number): number {
  const pts = stroke.points;
  if (pts.length === 0) return Infinity;
  if (pts.length === 1) return Math.hypot(pts[0].x - x, pts[0].y - y);

  let best = Infinity;
  for (let i = 0; i < pts.length - 1; i++) {
    const d = distanceToSegment(x, y, pts[i].x, pts[i].y, pts[i + 1].x, pts[i + 1].y);
    if (d < best) best = d;
  }
  return best;
}

function distanceToSegment(
  px: number, py: number,
  ax: number, ay: number,
  bx: number, by: number,
): number {
  const dx = bx - ax;
  const dy = by - ay;
  const lenSq = dx * dx + dy * dy;
  if (lenSq < 1e-9) return Math.hypot(px - ax, py - ay);
  let t = ((px - ax) * dx + (py - ay) * dy) / lenSq;
  t = t < 0 ? 0 : t > 1 ? 1 : t;
  return Math.hypot(px - (ax + t * dx), py - (ay + t * dy));
}

export function rectIntersects(a: Rect, b: Rect): boolean {
  return !(
    a.x + a.width < b.x ||
    b.x + b.width < a.x ||
    a.y + a.height < b.y ||
    b.y + b.height < a.y
  );
}

export function rectContainsPoint(r: Rect, x: number, y: number): boolean {
  return x >= r.x && x <= r.x + r.width && y >= r.y && y <= r.y + r.height;
}

export function unionRect(a: Rect, b: Rect): Rect {
  const x = Math.min(a.x, b.x);
  const y = Math.min(a.y, b.y);
  return {
    x,
    y,
    width: Math.max(a.x + a.width, b.x + b.width) - x,
    height: Math.max(a.y + a.height, b.y + b.height) - y,
  };
}
