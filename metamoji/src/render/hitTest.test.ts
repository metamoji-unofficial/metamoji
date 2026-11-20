/**
 * Regression coverage for issue #2/#3: a transparent (unfilled) shape or an
 * empty text box sitting over a pen stroke must not steal the hit, since
 * neither one paints anything where the stroke actually is. `hitsUnit` used
 * to test every non-`$draw` unit against its full bounding rect, so any
 * default (fill-less) shape or text box placed above ink made the ink
 * unselectable underneath it.
 */

import { describe, expect, it } from "vitest";

import { createDrawUnit, createLayer, createPage, createShapeUnit, createTextUnit } from "../model/factory";
import { strokeBounds } from "../model/stroke";
import type { InkPoint, PenAttributes, Page, Stroke, Unit } from "../model/types";
import { hitsUnit, hitTestUnit, resizeUnit } from "./hitTest";

const pen: PenAttributes = {
  color: "#000000",
  width: 4,
  penType: "ballpoint",
  opacity: 1,
  pressureSensitivity: 1,
};

function strokeFrom(points: InkPoint[]): Stroke {
  return { id: "s", points, pen, bounds: strokeBounds(points, pen.width) };
}

/** A $draw unit holding one horizontal-ish stroke, framed like `commitStroke` does. */
function drawUnitAt(points: InkPoint[]) {
  const stroke = strokeFrom(points);
  const unit = createDrawUnit();
  unit.x = stroke.bounds.x;
  unit.y = stroke.bounds.y;
  unit.width = stroke.bounds.width;
  unit.height = stroke.bounds.height;
  unit.strokes = [stroke];
  return unit;
}

function pageWith(units: Unit[]): Page {
  const page = createPage();
  const layer = createLayer();
  layer.units = units;
  page.layers = [layer];
  page.currentLayerId = layer.id;
  return page;
}

describe("hitsUnit: ink", () => {
  it("hits near the stroke centreline", () => {
    const unit = drawUnitAt([
      { x: 10, y: 50, p: 0.5, t: 0 },
      { x: 100, y: 50, p: 0.5, t: 10 },
    ]);
    expect(hitsUnit(unit, 50, 50)).toBe(true);
  });

  it("misses the padded bbox corner near a stroke endpoint, diagonally past the hit radius", () => {
    const unit = drawUnitAt([
      { x: 10, y: 50, p: 0.5, t: 0 },
      { x: 100, y: 50, p: 0.5, t: 10 },
    ]);
    // Bounds pad by 1.5x the pen width (6px here) in every direction, so the
    // bbox corner at (4, 44) sits diagonally ~8.49px from the stroke's left
    // endpoint (10, 50) — just past the pen-width/2 + slop (2 + 6 = 8) radius
    // that actually counts as "on the ink".
    expect(hitsUnit(unit, 4, 44)).toBe(false);
  });
});

describe("hitsUnit: transparent shapes no longer shadow what's under them", () => {
  it("an unfilled rect's empty interior does not hit, but its border does", () => {
    const shape = createShapeUnit(0, 0, 200, 100, "rect"); // fillColor defaults to ""
    expect(hitsUnit(shape, 100, 50)).toBe(false); // dead centre: nothing painted there
    expect(hitsUnit(shape, 0, 50)).toBe(true); // left edge: the stroke is there
  });

  it("a filled rect still hits anywhere inside", () => {
    const shape = createShapeUnit(0, 0, 200, 100, "rect", "#000000", "#ff0000");
    expect(hitsUnit(shape, 100, 50)).toBe(true);
  });

  it("an unfilled ellipse's corner (outside the ring, inside the bbox) does not hit", () => {
    const shape = createShapeUnit(0, 0, 200, 100, "ellipse");
    // Near the bbox corner, well outside the ellipse's ring.
    expect(hitsUnit(shape, 4, 4)).toBe(false);
    // On the ellipse boundary itself (left-most point).
    expect(hitsUnit(shape, 0, 50)).toBe(true);
  });

  it("a diagonal line only hits along the segment, not across its bounding box", () => {
    const shape = createShapeUnit(0, 0, 100, 100, "line");
    expect(hitsUnit(shape, 90, 10)).toBe(false); // top-right corner of the bbox
    expect(hitsUnit(shape, 50, 50)).toBe(true); // on the diagonal itself
  });

  it("a transparent shape placed over a pen stroke no longer blocks selecting the pen", () => {
    const ink = drawUnitAt([
      { x: 20, y: 50, p: 0.5, t: 0 },
      { x: 180, y: 50, p: 0.5, t: 10 },
    ]);
    const cover = createShapeUnit(0, 0, 200, 100, "rect"); // unfilled, drawn after -> on top
    const page = pageWith([ink, cover]);

    // Clicking the ink itself finds the ink, even though an unfilled shape's
    // bounding box fully covers it.
    const hit = hitTestUnit(page, 100, 50);
    expect(hit?.unit.id).toBe(ink.id);
  });
});

describe("hitsUnit: empty text boxes no longer shadow what's under them", () => {
  it("an empty, backgroundless text box never hits", () => {
    const text = createTextUnit(0, 0);
    expect(hitsUnit(text, text.x + 10, text.y + 10)).toBe(false);
  });

  it("a text box with actual content still hits its frame", () => {
    const text = createTextUnit(0, 0);
    text.text = "hello";
    expect(hitsUnit(text, text.x + 10, text.y + 10)).toBe(true);
  });

  it("a text box with an explicit background hits even while empty", () => {
    const text = createTextUnit(0, 0);
    text.backgroundColor = "#ffffff";
    expect(hitsUnit(text, text.x + 10, text.y + 10)).toBe(true);
  });

  it("an empty text box over a pen stroke no longer blocks selecting the pen", () => {
    const ink = drawUnitAt([
      { x: 20, y: 20, p: 0.5, t: 0 },
      { x: 100, y: 20, p: 0.5, t: 10 },
    ]);
    const cover = createTextUnit(0, 0); // 320x64 frame, empty, transparent by default
    const page = pageWith([ink, cover]);

    const hit = hitTestUnit(page, 60, 20);
    expect(hit?.unit.id).toBe(ink.id);
  });
});

/**
 * Regression coverage for `resizeUnit`: every `HandleType` is the literal
 * string "resize-" plus a compass suffix, and "resize" itself contains both
 * an "e" and an "s". Checking the *whole* handle string with
 * `.includes("e")`/`.includes("s")` therefore matched every single handle,
 * not just the east/south ones, silently cancelling out the real west/north
 * resize on every handle but the bottom-right corner.
 */
describe("resizeUnit", () => {
  const base = createShapeUnit(100, 100, 200, 150, "rect");

  it("resize-se grows width and height without moving the origin", () => {
    const next = resizeUnit(base, "resize-se", 20, 10, false);
    expect(next).toEqual({ x: 100, y: 100, width: 220, height: 160 });
  });

  it("resize-nw grows the box to the upper-left instead of just dragging it", () => {
    const next = resizeUnit(base, "resize-nw", -20, -10, false);
    expect(next).toEqual({ x: 80, y: 90, width: 220, height: 160 });
  });

  it("resize-w only changes width and x, leaving height untouched", () => {
    const next = resizeUnit(base, "resize-w", -20, 0, false);
    expect(next).toEqual({ x: 80, y: 100, width: 220, height: 150 });
  });

  it("resize-n only changes height and y, leaving width untouched", () => {
    const next = resizeUnit(base, "resize-n", 0, -10, false);
    expect(next).toEqual({ x: 100, y: 90, width: 200, height: 160 });
  });

  it("resize-e does not perturb height on a purely horizontal drag", () => {
    const next = resizeUnit(base, "resize-e", 20, 0, false);
    expect(next).toEqual({ x: 100, y: 100, width: 220, height: 150 });
  });

  it("resize-s does not perturb width on a purely vertical drag", () => {
    const next = resizeUnit(base, "resize-s", 0, 10, false);
    expect(next).toEqual({ x: 100, y: 100, width: 200, height: 160 });
  });
});
