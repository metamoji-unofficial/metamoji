/**
 * Hit testing and selection handles.
 *
 * The handle vocabulary follows docs/13 §2's `RbConstants$HandleType`: a move
 * body, eight resize handles, and rotation. We drop the original's separate
 * `StepRotate_*` and `Scale_*` handle families — a modifier key on the existing
 * handles covers both, and every extra handle is another 12px target competing
 * for space around small units.
 */

import { distanceToSegment, distanceToStroke, rectContainsPoint } from "../model/stroke";
import type { Layer, Page, Point, Rect, Unit } from "../model/types";

export type HandleType =
  | "move"
  | "resize-nw"
  | "resize-n"
  | "resize-ne"
  | "resize-e"
  | "resize-se"
  | "resize-s"
  | "resize-sw"
  | "resize-w"
  | "rotate";

export const HANDLE_SIZE = 9;
export const ROTATE_OFFSET = 26;

/** Rotates a world point into a unit's un-rotated local frame. */
export function toUnitLocal(unit: Unit, x: number, y: number): Point {
  if (unit.rotation === 0) return { x, y };
  const cx = unit.x + unit.width / 2;
  const cy = unit.y + unit.height / 2;
  const cos = Math.cos(-unit.rotation);
  const sin = Math.sin(-unit.rotation);
  const dx = x - cx;
  const dy = y - cy;
  return { x: cx + dx * cos - dy * sin, y: cy + dx * sin + dy * cos };
}

export function unitRect(unit: Unit): Rect {
  return { x: unit.x, y: unit.y, width: unit.width, height: unit.height };
}

/**
 * Topmost unit at a world point, searching layers back-to-front so the unit
 * drawn last wins — the same order the user sees.
 */
export function hitTestUnit(page: Page, x: number, y: number): { layer: Layer; unit: Unit } | null {
  for (let li = page.layers.length - 1; li >= 0; li--) {
    const layer = page.layers[li];
    if (!layer.visible || layer.locked) continue;
    for (let ui = layer.units.length - 1; ui >= 0; ui--) {
      const unit = layer.units[ui];
      if (hitsUnit(unit, x, y)) return { layer, unit };
    }
  }
  return null;
}

const INK_HIT_SLOP = 6;

/**
 * Tolerance for grabbing a thin outline (a shape's stroke, a form's grid
 * lines) — the same idea as `INK_HIT_SLOP`, applied to vector edges instead
 * of ink centrelines.
 */
const OUTLINE_HIT_SLOP = 6;

export function hitsUnit(unit: Unit, x: number, y: number): boolean {
  const local = toUnitLocal(unit, x, y);

  if (unit.type === "$draw") {
    // An ink unit usually spans the whole page, so its frame is meaningless as
    // a hit target — test the strokes themselves.
    for (const stroke of unit.strokes) {
      if (!rectContainsPoint(stroke.bounds, local.x, local.y)) continue;
      if (distanceToStroke(stroke, local.x, local.y) <= stroke.pen.width / 2 + INK_HIT_SLOP) {
        return true;
      }
    }
    return false;
  }

  // A background image is scenery, not a target.
  if (unit.type === "$bgimage") return false;

  // The remaining unit kinds can leave real, visible gaps inside their frame
  // — an unfilled shape, a text box nobody has typed into yet, a grid with no
  // fill colour. `createShapeUnit`/`createFormUnit`/`createTextUnit` all
  // default to exactly that transparent state, so treating the whole frame as
  // solid is not a corner case: it is what every freshly placed shape or text
  // box looks like. Hit-testing the frame rect there lets an invisible corner
  // of the object steal clicks (and selection) from whatever is genuinely
  // visible underneath it — see issue #2/#3.
  switch (unit.type) {
    case "$shape":
      return hitsShape(unit, local.x, local.y);
    case "$text":
      return hitsText(unit, local.x, local.y);
    case "$form":
      return hitsForm(unit, local.x, local.y);
    default:
      return rectContainsPoint(unitRect(unit), local.x, local.y);
  }
}

function hitsText(unit: Extract<Unit, { type: "$text" }>, x: number, y: number): boolean {
  const rect = unitRect(unit);
  if (unit.backgroundColor || unit.borderColor) return rectContainsPoint(rect, x, y);
  // No fill and no border: the frame itself paints nothing. An empty text box
  // in that state is completely invisible and must not shadow anything.
  if (unit.text.trim() === "") return false;
  return rectContainsPoint(rect, x, y);
}

function hitsForm(unit: Extract<Unit, { type: "$form" }>, x: number, y: number): boolean {
  const { x: ux, y: uy, width: w, height: h } = unit;
  if (unit.backgroundColor) return rectContainsPoint(unitRect(unit), x, y);
  if (unit.lineWidth <= 0) return false;

  const rows = Math.max(1, unit.rows);
  const cols = Math.max(1, unit.columns);
  const segs: number[][] = [];
  for (let r = 1; r < rows; r++) {
    const ly = uy + (h / rows) * r;
    segs.push([ux, ly, ux + w, ly]);
  }
  if (unit.form !== "ruled") {
    for (let c = 1; c < cols; c++) {
      const lx = ux + (w / cols) * c;
      segs.push([lx, uy, lx, uy + h]);
    }
  }
  if (unit.form === "table") {
    segs.push(
      [ux, uy, ux + w, uy],
      [ux + w, uy, ux + w, uy + h],
      [ux + w, uy + h, ux, uy + h],
      [ux, uy + h, ux, uy],
    );
  }

  const tol = unit.lineWidth / 2 + OUTLINE_HIT_SLOP;
  return segs.some(([ax, ay, bx, by]) => distanceToSegment(x, y, ax, ay, bx, by) <= tol);
}

function hitsShape(unit: Extract<Unit, { type: "$shape" }>, x: number, y: number): boolean {
  const { x: ux, y: uy, width: w, height: h, strokeWidth } = unit;
  const tol = strokeWidth / 2 + OUTLINE_HIT_SLOP;

  // Open shapes have no interior at all — the frame bounding a diagonal line
  // is mostly empty space, so only the drawn segments themselves count.
  if (unit.shape === "line" || unit.shape === "arrow") {
    if (strokeWidth <= 0) return false;
    const segs: number[][] = [[ux, uy, ux + w, uy + h]];
    if (unit.shape === "arrow") {
      const angle = Math.atan2(h, w);
      const head = Math.max(8, strokeWidth * 4);
      const tipX = ux + w;
      const tipY = uy + h;
      for (const spread of [Math.PI - 0.4, Math.PI + 0.4]) {
        segs.push([
          tipX, tipY,
          tipX + head * Math.cos(angle + spread), tipY + head * Math.sin(angle + spread),
        ]);
      }
    }
    return segs.some(([ax, ay, bx, by]) => distanceToSegment(x, y, ax, ay, bx, by) <= tol);
  }

  const hasFill = unit.fillColor !== "";

  if (unit.shape === "ellipse") {
    const cx = ux + w / 2;
    const cy = uy + h / 2;
    const rx = Math.abs(w / 2);
    const ry = Math.abs(h / 2);
    if (rx <= 0 || ry <= 0) return false;
    if (hasFill) {
      const nx = (x - cx) / rx;
      const ny = (y - cy) / ry;
      return nx * nx + ny * ny <= 1;
    }
    if (strokeWidth <= 0) return false;
    // Nearest point on the ellipse along the ray from its centre through the
    // test point — a good approximation of true boundary distance for the
    // mild eccentricities these shapes actually have.
    const t = Math.atan2((y - cy) / ry, (x - cx) / rx);
    const ex = cx + rx * Math.cos(t);
    const ey = cy + ry * Math.sin(t);
    return Math.hypot(x - ex, y - ey) <= tol;
  }

  // rect, roundRect, triangle, diamond: a closed polygon. The corner radius on
  // roundRect is ignored here — close enough for hit-testing, and not worth a
  // separate rounded-corner distance routine.
  const pts: Point[] =
    unit.shape === "triangle"
      ? [{ x: ux + w / 2, y: uy }, { x: ux + w, y: uy + h }, { x: ux, y: uy + h }]
      : unit.shape === "diamond"
        ? [
            { x: ux + w / 2, y: uy },
            { x: ux + w, y: uy + h / 2 },
            { x: ux + w / 2, y: uy + h },
            { x: ux, y: uy + h / 2 },
          ]
        : [
            { x: ux, y: uy },
            { x: ux + w, y: uy },
            { x: ux + w, y: uy + h },
            { x: ux, y: uy + h },
          ];

  if (hasFill) return pointInPolygon(pts, x, y);
  if (strokeWidth <= 0) return false;
  return polygonEdges(pts).some(([ax, ay, bx, by]) => distanceToSegment(x, y, ax, ay, bx, by) <= tol);
}

function polygonEdges(pts: Point[]): number[][] {
  const segs: number[][] = [];
  for (let i = 0; i < pts.length; i++) {
    const a = pts[i];
    const b = pts[(i + 1) % pts.length];
    segs.push([a.x, a.y, b.x, b.y]);
  }
  return segs;
}

function pointInPolygon(pts: Point[], x: number, y: number): boolean {
  let inside = false;
  for (let i = 0, j = pts.length - 1; i < pts.length; j = i++) {
    const { x: xi, y: yi } = pts[i];
    const { x: xj, y: yj } = pts[j];
    const intersect = yi > y !== yj > y && x < ((xj - xi) * (y - yi)) / (yj - yi) + xi;
    if (intersect) inside = !inside;
  }
  return inside;
}

/** Handle positions in world space, in the unit's un-rotated frame. */
export function handlePositions(unit: Unit): Record<HandleType, Point> {
  const { x, y, width: w, height: h } = unit;
  const mx = x + w / 2;
  const my = y + h / 2;
  return {
    "resize-nw": { x, y },
    "resize-n": { x: mx, y },
    "resize-ne": { x: x + w, y },
    "resize-e": { x: x + w, y: my },
    "resize-se": { x: x + w, y: y + h },
    "resize-s": { x: mx, y: y + h },
    "resize-sw": { x, y: y + h },
    "resize-w": { x, y: my },
    rotate: { x: mx, y: y },
    move: { x: mx, y: my },
  };
}

/**
 * Which handle a world point grabs, or null. `scale` is the viewport scale, so
 * the grab radius stays a constant size on screen at any zoom.
 */
export function hitTestHandle(unit: Unit, x: number, y: number, scale: number): HandleType | null {
  const local = toUnitLocal(unit, x, y);
  const grab = (HANDLE_SIZE + 3) / scale;
  const positions = handlePositions(unit);

  const rot = positions.rotate;
  if (Math.hypot(local.x - rot.x, local.y - (rot.y - ROTATE_OFFSET / scale)) <= grab) {
    return "rotate";
  }

  const order: HandleType[] = [
    "resize-nw", "resize-ne", "resize-se", "resize-sw",
    "resize-n", "resize-e", "resize-s", "resize-w",
  ];
  for (const h of order) {
    const p = positions[h];
    if (Math.abs(local.x - p.x) <= grab && Math.abs(local.y - p.y) <= grab) return h;
  }

  if (rectContainsPoint(unitRect(unit), local.x, local.y)) return "move";
  return null;
}

const MIN_UNIT_SIZE = 16;

/** Applies a resize-handle drag, keeping the opposite edge or corner pinned. */
export function resizeUnit(
  unit: Unit,
  handle: HandleType,
  dx: number,
  dy: number,
  keepAspect: boolean,
): Pick<Unit, "x" | "y" | "width" | "height"> {
  let { x, y, width, height } = unit;

  if (handle.includes("w")) {
    const nw = width - dx;
    if (nw >= MIN_UNIT_SIZE) {
      x += dx;
      width = nw;
    }
  }
  if (handle.includes("e")) {
    width = Math.max(MIN_UNIT_SIZE, width + dx);
  }
  if (handle.includes("n")) {
    const nh = height - dy;
    if (nh >= MIN_UNIT_SIZE) {
      y += dy;
      height = nh;
    }
  }
  if (handle.includes("s")) {
    height = Math.max(MIN_UNIT_SIZE, height + dy);
  }

  if (keepAspect && unit.width > 0 && unit.height > 0) {
    const ratio = unit.width / unit.height;
    const byWidth = Math.abs(width - unit.width) > Math.abs(height - unit.height);
    if (byWidth) height = Math.max(MIN_UNIT_SIZE, width / ratio);
    else width = Math.max(MIN_UNIT_SIZE, height * ratio);
    // Corners anchored at the top or left must move so the opposite one stays put.
    if (handle.includes("n")) y = unit.y + unit.height - height;
    if (handle.includes("w")) x = unit.x + unit.width - width;
  }

  return { x, y, width, height };
}

/** Units fully inside a world-space rubber-band rectangle. */
export function unitsInRect(page: Page, rect: Rect): Unit[] {
  const norm = normalizeRect(rect);
  const out: Unit[] = [];
  for (const layer of page.layers) {
    if (!layer.visible || layer.locked) continue;
    for (const unit of layer.units) {
      if (unit.type === "$bgimage") continue;
      const r = unitRect(unit);
      if (
        r.x >= norm.x &&
        r.y >= norm.y &&
        r.x + r.width <= norm.x + norm.width &&
        r.y + r.height <= norm.y + norm.height
      ) {
        out.push(unit);
      }
    }
  }
  return out;
}

export function normalizeRect(rect: Rect): Rect {
  return {
    x: rect.width < 0 ? rect.x + rect.width : rect.x,
    y: rect.height < 0 ? rect.y + rect.height : rect.y,
    width: Math.abs(rect.width),
    height: Math.abs(rect.height),
  };
}

export const HANDLE_CURSORS: Record<HandleType, string> = {
  move: "move",
  "resize-nw": "nwse-resize",
  "resize-n": "ns-resize",
  "resize-ne": "nesw-resize",
  "resize-e": "ew-resize",
  "resize-se": "nwse-resize",
  "resize-s": "ns-resize",
  "resize-sw": "nesw-resize",
  "resize-w": "ew-resize",
  rotate: "grab",
};
