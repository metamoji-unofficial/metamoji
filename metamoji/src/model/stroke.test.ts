import { describe, expect, it } from "vitest";

import { distanceToStroke, simplify, strokeBounds, strokeOutlineShapes, widthAt } from "./stroke";
import type { StrokeShape } from "./stroke";
import { screenToWorld, worldToScreen, zoomAbout, clampScale, fitRect } from "../render/viewport";
import type { InkPoint, PenAttributes, Stroke } from "./types";

/** Point-in-union test mirroring what a nonzero-rule Canvas fill computes. */
function isCovered(shapes: StrokeShape[], x: number, y: number): boolean {
  return shapes.some((shape) => {
    if (shape.kind === "circle") return Math.hypot(x - shape.cx, y - shape.cy) <= shape.r;
    const [a, b, c, d] = shape.pts;
    return pointInQuad(x, y, a, b, c, d);
  });
}

function pointInQuad(
  x: number, y: number,
  a: { x: number; y: number }, b: { x: number; y: number },
  c: { x: number; y: number }, d: { x: number; y: number },
): boolean {
  const cross = (p: typeof a, q: typeof a) => (q.x - p.x) * (y - p.y) - (q.y - p.y) * (x - p.x);
  const s1 = Math.sign(cross(a, b));
  const s2 = Math.sign(cross(b, c));
  const s3 = Math.sign(cross(c, d));
  const s4 = Math.sign(cross(d, a));
  const allNonNeg = s1 >= 0 && s2 >= 0 && s3 >= 0 && s4 >= 0;
  const allNonPos = s1 <= 0 && s2 <= 0 && s3 <= 0 && s4 <= 0;
  return allNonNeg || allNonPos;
}

const pen: PenAttributes = {
  color: "#000000",
  width: 4,
  penType: "ballpoint",
  opacity: 1,
  pressureSensitivity: 1,
};

function stroke(points: InkPoint[]): Stroke {
  return { id: "s", points, pen, bounds: strokeBounds(points, pen.width) };
}

describe("stroke geometry", () => {
  it("bounds cover every point plus room for the stroke width", () => {
    const points: InkPoint[] = [
      { x: 10, y: 10, p: 0.5, t: 0 },
      { x: 50, y: 30, p: 0.5, t: 10 },
    ];
    const b = strokeBounds(points, 4);
    expect(b.x).toBeLessThan(10);
    expect(b.y).toBeLessThan(10);
    expect(b.x + b.width).toBeGreaterThan(50);
    expect(b.y + b.height).toBeGreaterThan(30);
  });

  it("bounds of an empty stroke are empty rather than infinite", () => {
    expect(strokeBounds([], 4)).toEqual({ x: 0, y: 0, width: 0, height: 0 });
  });

  it("width tracks pressure, and stops doing so at zero sensitivity", () => {
    expect(widthAt(pen, 1)).toBeGreaterThan(widthAt(pen, 0));
    const flat = { ...pen, pressureSensitivity: 0 };
    expect(widthAt(flat, 0)).toBe(widthAt(flat, 1));
  });

  it("a highlighter ignores pressure entirely", () => {
    const highlighter: PenAttributes = {
      ...pen,
      penType: "highlighter",
      pressureSensitivity: 1,
    };
    expect(widthAt(highlighter, 0)).toBe(highlighter.width);
    expect(widthAt(highlighter, 1)).toBe(highlighter.width);
  });

  it("simplify drops near-duplicate samples but keeps the endpoints", () => {
    const dense: InkPoint[] = Array.from({ length: 50 }, (_, i) => ({
      x: i * 0.1,
      y: 0,
      p: 0.5,
      t: i,
    }));
    const out = simplify(dense, 1);
    expect(out.length).toBeLessThan(dense.length);
    expect(out[0]).toEqual(dense[0]);
    expect(out[out.length - 1]).toEqual(dense[dense.length - 1]);
  });

  it("simplify leaves short strokes alone", () => {
    const two: InkPoint[] = [
      { x: 0, y: 0, p: 0.5, t: 0 },
      { x: 1, y: 1, p: 0.5, t: 1 },
    ];
    expect(simplify(two)).toEqual(two);
  });

  it("distance to a stroke is zero on the line and grows away from it", () => {
    const s = stroke([
      { x: 0, y: 0, p: 0.5, t: 0 },
      { x: 100, y: 0, p: 0.5, t: 10 },
    ]);
    expect(distanceToStroke(s, 50, 0)).toBeCloseTo(0);
    expect(distanceToStroke(s, 50, 10)).toBeCloseTo(10);
    // Past the end, distance is measured to the endpoint, not the infinite line.
    expect(distanceToStroke(s, 130, 0)).toBeCloseTo(30);
  });

  it("distance to an empty stroke is infinite rather than NaN", () => {
    expect(distanceToStroke(stroke([]), 0, 0)).toBe(Infinity);
  });

  it("fills solid when the pen loops back over itself tighter than its own width", () => {
    // A tiny scribble: the loop's radius (1) is smaller than the pen's half-width
    // (2 for this 4-wide pen), so the whole disc the tip sweeps should be covered,
    // including dead centre. The old offset-outline approach left a hole there
    // because the two offset sides crossed and the fill rule read it as empty.
    const points: InkPoint[] = [];
    for (let i = 0; i <= 120; i++) {
      const t = (i / 40) * Math.PI * 2;
      points.push({ x: 30 + 1 * Math.cos(t), y: 30 + 1 * Math.sin(t), p: 0.5, t: i });
    }
    const shapes = strokeOutlineShapes(stroke(points));
    expect(isCovered(shapes, 30, 30)).toBe(true);
  });

  it("a loop wider than the pen still leaves its centre unpainted", () => {
    // Sanity check for the test above: a loop bigger than the pen never sweeps
    // its own centre, so that hole is physically correct and must stay a hole.
    const points: InkPoint[] = [];
    for (let i = 0; i <= 120; i++) {
      const t = (i / 40) * Math.PI * 2;
      points.push({ x: 30 + 12 * Math.cos(t), y: 30 + 12 * Math.sin(t), p: 0.5, t: i });
    }
    const shapes = strokeOutlineShapes(stroke(points));
    expect(isCovered(shapes, 30, 30)).toBe(false);
  });
});

describe("viewport", () => {
  it("screenToWorld inverts worldToScreen", () => {
    const vp = { scale: 2.5, tx: -130, ty: 64 };
    const screen = worldToScreen(vp, 42, 17);
    const world = screenToWorld(vp, screen.x, screen.y);
    expect(world.x).toBeCloseTo(42);
    expect(world.y).toBeCloseTo(17);
  });

  it("zooming about a point keeps that point fixed on screen", () => {
    const vp = { scale: 1, tx: 20, ty: 30 };
    const anchor = { x: 400, y: 300 };
    const before = screenToWorld(vp, anchor.x, anchor.y);

    const zoomed = zoomAbout(vp, anchor.x, anchor.y, 1.8);
    const after = screenToWorld(zoomed, anchor.x, anchor.y);

    expect(after.x).toBeCloseTo(before.x);
    expect(after.y).toBeCloseTo(before.y);
  });

  it("scale is clamped to a usable range", () => {
    expect(clampScale(1000)).toBeLessThanOrEqual(8);
    expect(clampScale(0.0001)).toBeGreaterThanOrEqual(0.1);
  });

  it("fitRect centres the rect within the view", () => {
    const vp = fitRect({ x: 0, y: 0, width: 1000, height: 500 }, 800, 600, 0);
    const topLeft = worldToScreen(vp, 0, 0);
    const bottomRight = worldToScreen(vp, 1000, 500);

    // Equal margins on both axes means it is centred.
    expect(topLeft.x).toBeCloseTo(800 - bottomRight.x);
    expect(topLeft.y).toBeCloseTo(600 - bottomRight.y);
  });
});
