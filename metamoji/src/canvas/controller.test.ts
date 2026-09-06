/**
 * Regression tests for the two failures found while driving the real editor.
 *
 * Both were silent: the app looked like it worked, but the page was at the
 * wrong zoom and every placement landed at the wrong coordinates.
 */

import { beforeEach, describe, expect, it, vi } from "vitest";

import { CanvasController, type ToolMode } from "./controller";
import { EditSession } from "../editor/session";
import { A4_WIDTH, A4_HEIGHT, createDocument, createDrawUnit, createTextUnit } from "../model/factory";
import { strokeBounds } from "../model/stroke";
import type { InkPoint, PenAttributes, Point, Rect, Unit } from "../model/types";

import { beforeEach, describe, expect, it, vi } from "vitest";

import { CanvasController, type ToolMode } from "./controller";
import { EditSession } from "../editor/session";
import { A4_WIDTH, A4_HEIGHT, createDocument, createDrawUnit, createTextUnit } from "../model/factory";
import { strokeBounds } from "../model/stroke";

/**
 * jsdom has no canvas backend, so the drawing calls need somewhere to go. The
 * controller's geometry — which is what these tests are about — does not care
 * what the context does with them.
 */
function stubContext(): CanvasRenderingContext2D {
  return new Proxy(
    {
      canvas: null,
      setTransform: () => {},
      measureText: () => ({ width: 0 }),
    } as unknown as CanvasRenderingContext2D,
    {
      get(target, prop) {
        const existing = Reflect.get(target, prop);
        if (existing !== undefined) return existing;
        return () => undefined;
      },
      set: () => true,
    },
  );
}

/** A canvas whose parent reports the size we choose. */
function makeCanvas(parentWidth: number, parentHeight: number) {
  const parent = document.createElement("div");
  parent.getBoundingClientRect = () =>
    ({ width: parentWidth, height: parentHeight, left: 0, top: 0 }) as DOMRect;

  const canvas = document.createElement("canvas");
  canvas.getContext = (() => stubContext()) as unknown as HTMLCanvasElement["getContext"];
  canvas.getBoundingClientRect = () =>
    ({ width: parentWidth, height: parentHeight, left: 0, top: 0 }) as DOMRect;
  Object.defineProperty(canvas, "clientWidth", {
    get: () => (canvas.style.width ? parseFloat(canvas.style.width) : 0),
    configurable: true,
  });
  Object.defineProperty(canvas, "clientHeight", {
    get: () => (canvas.style.height ? parseFloat(canvas.style.height) : 0),
    configurable: true,
  });
  parent.appendChild(canvas);
  return { canvas, parent };
}

function setup(width: number, height: number) {
  const placements: { tool: ToolMode; world: Point }[] = [];
  const frames: { tool: ToolMode; frame: Rect }[] = [];
  const toolRequests: ToolMode[] = [];
  const controller = new CanvasController({
    onSelectionChange: () => {},
    onViewportChange: () => {},
    onPlace: (tool, world) => placements.push({ tool, world }),
    onPlaceFrame: (tool, frame) => frames.push({ tool, frame }),
    onEditText: () => {},
    onRequestTool: (tool) => toolRequests.push(tool),
  });

  const scene = makeCanvas(width, height);
  const overlayEl = makeCanvas(width, height);
  // Both canvases must share a parent for the size lookup to be consistent.
  scene.parent.appendChild(overlayEl.canvas);

  return {
    controller,
    scene: scene.canvas,
    overlay: overlayEl.canvas,
    placements,
    frames,
    toolRequests,
  };
}

/** jsdom has no 2D canvas backend, so it never defines the Path2D global either. */
class StubPath2D {
  moveTo(): void {}
  lineTo(): void {}
  arc(): void {}
  quadraticCurveTo(): void {}
  closePath(): void {}
}

beforeEach(() => {
  vi.stubGlobal("requestAnimationFrame", () => 1);
  vi.stubGlobal("cancelAnimationFrame", () => {});
  vi.stubGlobal("devicePixelRatio", 2);
  vi.stubGlobal("Path2D", StubPath2D);
});

describe("CanvasController viewport", () => {
  it("fits the page once the canvas is given a real size", () => {
    // The container starts collapsed — what happens when a note opens while the
    // window is still laying out, or while the view is hidden.
    const { controller, scene, overlay } = setup(0, 0);
    const doc = createDocument();
    const session = new EditSession(doc);

    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, session);

    // Nothing to fit against yet, so the viewport must stay untouched rather
    // than be computed from the 300x150 default canvas.
    expect(controller.getViewport()).toEqual({ scale: 1, tx: 0, ty: 0 });

    // The ResizeObserver reports a real size.
    scene.parentElement!.getBoundingClientRect = () =>
      ({ width: 984, height: 642, left: 0, top: 0 }) as DOMRect;
    controller.resize();

    const vp = controller.getViewport();
    expect(vp.scale).toBeGreaterThan(0.1);
    expect(vp.scale).toBeLessThan(1);
    // Centred horizontally, with the page fully inside the view.
    expect(vp.tx).toBeCloseTo((984 - A4_WIDTH * vp.scale) / 2, 1);
    expect(A4_HEIGHT * vp.scale).toBeLessThanOrEqual(642);
  });

  it("fits when the document arrives after the canvas was already sized", () => {
    const { controller, scene, overlay } = setup(984, 642);
    controller.attach(scene, overlay);
    expect(controller.getViewport().scale).toBe(1);

    const doc = createDocument();
    controller.setDocument(doc, 0, new EditSession(doc));
    expect(controller.getViewport().scale).toBeLessThan(1);
  });

  it("does not re-fit on later edits, so the user's zoom is not thrown away", () => {
    const { controller, scene, overlay } = setup(984, 642);
    const doc = createDocument();
    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, new EditSession(doc));

    controller.zoomBy(2);
    const zoomed = controller.getViewport().scale;

    // Every edit produces a new document object; that must not re-fit.
    controller.setDocument({ ...doc, revision: 1 }, 0, new EditSession(doc));
    expect(controller.getViewport().scale).toBe(zoomed);
  });

  it("converts a tap into world coordinates, not screen coordinates", () => {
    const { controller, scene, overlay, placements } = setup(984, 642);
    const doc = createDocument();
    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, new EditSession(doc));
    controller.setTool("text");

    const vp = controller.getViewport();
    const screen = { x: 300, y: 260 };

    overlay.dispatchEvent(
      new PointerEvent("pointerdown", {
        pointerId: 1,
        button: 0,
        buttons: 1,
        clientX: screen.x,
        clientY: screen.y,
        bubbles: true,
      }),
    );

    expect(placements).toHaveLength(1);
    // The placement must be the inverse viewport transform of the tap, which is
    // only the same as the tap itself at identity zoom.
    expect(placements[0].world.x).toBeCloseTo((screen.x - vp.tx) / vp.scale, 3);
    expect(placements[0].world.y).toBeCloseTo((screen.y - vp.ty) / vp.scale, 3);
    expect(placements[0].world.x).not.toBeCloseTo(screen.x, 1);
  });

  it("starts the gesture even when pointer capture is refused", () => {
    const { controller, scene, overlay, placements } = setup(984, 642);
    const doc = createDocument();
    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, new EditSession(doc));
    controller.setTool("text");

    // A pointer that is no longer active makes setPointerCapture throw. Letting
    // that propagate would abort pointerdown and the tap would do nothing.
    overlay.setPointerCapture = () => {
      throw new DOMException("No active pointer with the given id", "NotFoundError");
    };

    expect(() =>
      overlay.dispatchEvent(
        new PointerEvent("pointerdown", {
          pointerId: 99,
          button: 0,
          buttons: 1,
          clientX: 200,
          clientY: 200,
          bubbles: true,
        }),
      ),
    ).not.toThrow();

    expect(placements).toHaveLength(1);
  });

  it("reports a dragged frame in world coordinates", () => {
    const { controller, scene, overlay, frames } = setup(984, 642);
    const doc = createDocument();
    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, new EditSession(doc));
    controller.setTool("shape");

    const vp = controller.getViewport();
    const send = (type: string, x: number, y: number, shiftKey = false) =>
      overlay.dispatchEvent(
        new PointerEvent(type, {
          pointerId: 3,
          button: 0,
          buttons: type === "pointerup" ? 0 : 1,
          clientX: x,
          clientY: y,
          shiftKey,
          bubbles: true,
        }),
      );

    send("pointerdown", 200, 200);
    send("pointermove", 400, 320);
    send("pointerup", 400, 320);

    expect(frames).toHaveLength(1);
    expect(frames[0].tool).toBe("shape");
    expect(frames[0].frame.x).toBeCloseTo((200 - vp.tx) / vp.scale, 3);
    expect(frames[0].frame.width).toBeCloseTo(200 / vp.scale, 3);
  });

  it("ignores a frame drag too small to be intentional", () => {
    const { controller, scene, overlay, frames } = setup(984, 642);
    const doc = createDocument();
    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, new EditSession(doc));
    controller.setTool("shape");

    const send = (type: string, x: number, y: number) =>
      overlay.dispatchEvent(
        new PointerEvent(type, {
          pointerId: 4,
          button: 0,
          buttons: type === "pointerup" ? 0 : 1,
          clientX: x,
          clientY: y,
          bubbles: true,
        }),
      );

    send("pointerdown", 200, 200);
    send("pointermove", 201, 201);
    send("pointerup", 201, 201);
    expect(frames).toHaveLength(0);
  });

  it("hands over to the select tool once a lasso catches something", () => {
    const { controller, scene, overlay, toolRequests } = setup(984, 642);
    const doc = createDocument();
    const unit = createTextUnit(0, 0);
    unit.width = 200;
    unit.height = 100;
    doc.pages[0].layers[0].units.push(unit);

    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, new EditSession(doc));
    controller.setTool("lasso");
    controller.setLassoMode("overlap");

    const vp = controller.getViewport();
    const toScreen = (wx: number, wy: number) => ({
      x: wx * vp.scale + vp.tx,
      y: wy * vp.scale + vp.ty,
    });
    const send = (type: string, x: number, y: number) =>
      overlay.dispatchEvent(
        new PointerEvent(type, {
          pointerId: 5,
          button: 0,
          buttons: type === "pointerup" ? 0 : 1,
          clientX: x,
          clientY: y,
          bubbles: true,
        }),
      );

    // A loop comfortably around the unit.
    const loop = [
      toScreen(-50, -50),
      toScreen(300, -50),
      toScreen(300, 200),
      toScreen(-50, 200),
    ];
    send("pointerdown", loop[0].x, loop[0].y);
    for (const p of loop.slice(1)) send("pointermove", p.x, p.y);
    send("pointerup", loop[0].x, loop[0].y);

    expect(toolRequests).toContain("select");
  });

  it("moves a stroke's own points (and keeps its outline in sync) when its $draw unit is dragged", () => {
    // Each stroke now has its own $draw unit, so it can be individually
    // selected and moved (see stroke.ts and commitStroke). A lasso in
    // "overlap" mode can still bundle that unit's id together with another
    // unit sitting on top of it, and dragging the bundle must move both
    // consistently: the stroke's actual points translate by the same delta as
    // the other unit, and the $draw unit's x/y/width/height (only used for
    // its selection outline) stay in sync with the stroke's new bounds —
    // rather than the old bug where the frame drifted away from ink that
    // never actually moved, hiding it once the frame scrolled off-screen.
    const { controller, scene, overlay } = setup(984, 642);
    const doc = createDocument();

    const shape = createTextUnit(0, 0);
    shape.width = 200;
    shape.height = 100;

    const pen: PenAttributes = {
      color: "#000000",
      width: 4,
      penType: "ballpoint",
      opacity: 1,
      pressureSensitivity: 0,
    };
    const points: InkPoint[] = [{ x: 500, y: 500, p: 0.5, t: 0 }];
    const draw = createDrawUnit();
    draw.strokes = [{ id: "s", points, pen, bounds: strokeBounds(points, pen.width) }];

    doc.pages[0].layers[0].units.push(draw, shape);

    const session = new EditSession(doc);
    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, session);
    controller.setTool("lasso");
    controller.setLassoMode("overlap");

    const vp = controller.getViewport();
    const toScreen = (wx: number, wy: number) => ({
      x: wx * vp.scale + vp.tx,
      y: wy * vp.scale + vp.ty,
    });
    const send = (type: string, x: number, y: number, pointerId: number) =>
      overlay.dispatchEvent(
        new PointerEvent(type, {
          pointerId,
          button: 0,
          buttons: type === "pointerup" ? 0 : 1,
          clientX: x,
          clientY: y,
          bubbles: true,
        }),
      );

    // A loop wide enough to catch both the shape and the stroke at (500, 500).
    const loop = [toScreen(-50, -50), toScreen(600, -50), toScreen(600, 600), toScreen(-50, 600)];
    send("pointerdown", loop[0].x, loop[0].y, 10);
    for (const p of loop.slice(1)) send("pointermove", p.x, p.y, 10);
    send("pointerup", loop[0].x, loop[0].y, 10);

    // Drag from inside the shape (away from the stroke) — the select tool
    // keeps the whole bundled selection since the shape was already in it.
    const dragStart = toScreen(100, 50);
    const dragEnd = toScreen(300, 250);
    send("pointerdown", dragStart.x, dragStart.y, 11);
    send("pointermove", dragEnd.x, dragEnd.y, 11);
    send("pointerup", dragEnd.x, dragEnd.y, 11);

    const units = session.document.pages[0].layers[0].units;
    const movedShape = units.find((u) => u.id === shape.id)!;
    const movedDraw = units.find((u) => u.id === draw.id)! as typeof draw;
    const movedStroke = movedDraw.strokes[0];

    expect(movedShape.x).toBeCloseTo(200);
    expect(movedShape.y).toBeCloseTo(200);
    // The stroke's own points moved by the same delta as the shape...
    expect(movedStroke.points[0].x).toBeCloseTo(700);
    expect(movedStroke.points[0].y).toBeCloseTo(700);
    // ...and the unit's outline was refreshed to match, not left stale.
    expect(movedDraw.x).toBeCloseTo(movedStroke.bounds.x);
    expect(movedDraw.y).toBeCloseTo(movedStroke.bounds.y);
  });

  it("selects and drags a single stroke by itself, leaving an untouched one in place", () => {
    // The feature this covers: previously every stroke on a layer shared one
    // $draw unit, so there was nothing to individually select. Now each
    // stroke has its own unit, so clicking one with the select tool and
    // dragging it should move only that stroke.
    const { controller, scene, overlay } = setup(984, 642);
    const doc = createDocument();

    const pen: PenAttributes = {
      color: "#000000",
      width: 4,
      penType: "ballpoint",
      opacity: 1,
      pressureSensitivity: 0,
    };
    const makeDraw = (x: number, y: number) => {
      const points: InkPoint[] = [{ x, y, p: 0.5, t: 0 }];
      const unit = createDrawUnit();
      unit.strokes = [{ id: `s-${x}-${y}`, points, pen, bounds: strokeBounds(points, pen.width) }];
      return unit;
    };
    const strokeA = makeDraw(100, 100);
    const strokeB = makeDraw(400, 400);
    doc.pages[0].layers[0].units.push(strokeA, strokeB);

    const session = new EditSession(doc);
    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, session);
    controller.setTool("select");

    const vp = controller.getViewport();
    const toScreen = (wx: number, wy: number) => ({
      x: wx * vp.scale + vp.tx,
      y: wy * vp.scale + vp.ty,
    });
    const send = (type: string, x: number, y: number) =>
      overlay.dispatchEvent(
        new PointerEvent(type, {
          pointerId: 12,
          button: 0,
          buttons: type === "pointerup" ? 0 : 1,
          clientX: x,
          clientY: y,
          bubbles: true,
        }),
      );

    const start = toScreen(100, 100);
    const end = toScreen(150, 130);
    send("pointerdown", start.x, start.y);
    send("pointermove", end.x, end.y);
    send("pointerup", end.x, end.y);

    const units = session.document.pages[0].layers[0].units;
    const movedA = units.find((u) => u.id === strokeA.id)! as typeof strokeA;
    const untouchedB = units.find((u) => u.id === strokeB.id)! as typeof strokeB;

    expect(movedA.strokes[0].points[0].x).toBeCloseTo(150);
    expect(movedA.strokes[0].points[0].y).toBeCloseTo(130);
    expect(untouchedB.strokes[0].points[0].x).toBe(400);
    expect(untouchedB.strokes[0].points[0].y).toBe(400);
  });

  it("gives each drawn stroke its own unit, and erasing one removes only that unit", () => {
    const { controller, scene, overlay } = setup(984, 642);
    const doc = createDocument();
    const session = new EditSession(doc);
    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, session);
    // The real app's store re-feeds the controller on every session change
    // (see editorStore.ts's openDocument); without this, the controller keeps
    // acting on the document as it was at setDocument time.
    session.subscribe((event) => controller.setDocument(event.doc, 0, session));

    const vp = controller.getViewport();
    const toScreen = (wx: number, wy: number) => ({
      x: wx * vp.scale + vp.tx,
      y: wy * vp.scale + vp.ty,
    });
    const send = (type: string, x: number, y: number, pointerId: number) =>
      overlay.dispatchEvent(
        new PointerEvent(type, {
          pointerId,
          button: 0,
          buttons: type === "pointerup" ? 0 : 1,
          clientX: x,
          clientY: y,
          bubbles: true,
        }),
      );
    const draw = (from: Point, to: Point, pointerId: number) => {
      const a = toScreen(from.x, from.y);
      const b = toScreen(to.x, to.y);
      send("pointerdown", a.x, a.y, pointerId);
      send("pointermove", b.x, b.y, pointerId);
      send("pointerup", b.x, b.y, pointerId);
    };

    controller.setTool("pen");
    draw({ x: 100, y: 100 }, { x: 130, y: 100 }, 20);
    draw({ x: 400, y: 400 }, { x: 430, y: 400 }, 21);

    const drawUnits = () =>
      session.document.pages[0].layers[0].units.filter(
        (u): u is Extract<Unit, { type: "$draw" }> => u.type === "$draw",
      );
    const [firstUnit, secondUnit] = drawUnits();
    expect(drawUnits()).toHaveLength(2);

    // Erase the first stroke, at wherever the pen-smoothing actually recorded
    // it — only its unit should disappear.
    controller.setTool("eraser");
    const firstPoint = firstUnit.strokes[0].points[0];
    const eraseAt = toScreen(firstPoint.x, firstPoint.y);
    send("pointerdown", eraseAt.x, eraseAt.y, 22);
    send("pointerup", eraseAt.x, eraseAt.y, 22);

    const remaining = drawUnits();
    expect(remaining).toHaveLength(1);
    expect(remaining[0].id).toBe(secondUnit.id);
  });

  it("zooms about a point without moving the world point under it", () => {
    const { controller, scene, overlay } = setup(984, 642);
    const doc = createDocument();
    controller.attach(scene, overlay);
    controller.setDocument(doc, 0, new EditSession(doc));

    const before = controller.getViewport();
    const anchorWorld = {
      x: (400 - before.tx) / before.scale,
      y: (300 - before.ty) / before.scale,
    };

    overlay.dispatchEvent(
      new WheelEvent("wheel", {
        deltaY: -200,
        ctrlKey: true,
        clientX: 400,
        clientY: 300,
        bubbles: true,
        cancelable: true,
      }),
    );

    const after = controller.getViewport();
    expect(after.scale).toBeGreaterThan(before.scale);
    expect((400 - after.tx) / after.scale).toBeCloseTo(anchorWorld.x, 3);
    expect((300 - after.ty) / after.scale).toBeCloseTo(anchorWorld.y, 3);
  });
});
