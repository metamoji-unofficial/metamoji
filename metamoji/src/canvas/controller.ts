/**
 * The canvas controller — all imperative drawing and input lives here.
 *
 * Two stacked canvases:
 *
 *   scene   opaque, holds the paper and every committed unit. Repainted when the
 *           document or the viewport changes, which is at most once per gesture.
 *   overlay transparent and `desynchronized`, holds the stroke currently under
 *           the pen plus selection handles. Repainted per pointer frame, but it
 *           only ever draws one stroke.
 *
 * This is docs/10's "live bitmap" layer strategy reduced to the part that earns
 * its keep. What it buys: a pointer move never touches React and never repaints
 * committed ink, so stroke latency is independent of how full the page is.
 */

import { PEN_PRESETS } from "../editor/tools";
import type { EditSession } from "../editor/session";
import { newStrokeId } from "../model/ids";
import { createDrawUnit } from "../model/factory";
import { distanceToStroke, rectContainsPoint, simplify, strokeBounds } from "../model/stroke";
import type {
  InkPoint,
  ModelId,
  NoteDocument,
  Page,
  PenAttributes,
  Point,
  Rect,
  Stroke,
  Unit,
} from "../model/types";
import { currentLayer } from "../model/types";
import { canvasTheme, onCanvasThemeChange } from "./theme";
import {
  HANDLE_CURSORS,
  HANDLE_SIZE,
  ROTATE_OFFSET,
  handlePositions,
  hitTestHandle,
  hitTestUnit,
  normalizeRect,
  resizeUnit,
  toUnitLocal,
  unitsInRect,
  type HandleType,
} from "../render/hitTest";
import {
  drawStroke,
  renderPage,
  surveyChoiceAt,
  type AssetResolver,
} from "../render/renderer";
import {
  fitRect,
  panBy,
  screenToWorld,
  zoomAbout,
  type Viewport,
  IDENTITY_VIEWPORT,
} from "../render/viewport";
import { PointFilter } from "./oneEuro";
import {
  simplifyPolygon,
  unitInLasso,
  type LassoMode,
} from "../render/polygon";

export type ToolMode =
  | "pen"
  | "eraser"
  | "select"
  | "lasso"
  | "pan"
  | "text"
  | "sticky"
  | "image"
  | "shape"
  | "form"
  | "survey"
  | "laser";

export interface ControllerCallbacks {
  onSelectionChange: (ids: ModelId[]) => void;
  onViewportChange: (viewport: Viewport) => void;
  /** A tap with a placement tool active, in world coordinates. */
  onPlace: (tool: ToolMode, world: Point) => void;
  /** A drag with a frame-placing tool active, in world coordinates. */
  onPlaceFrame: (tool: ToolMode, frame: Rect) => void;
  onEditText: (unitId: ModelId) => void;
  /** The canvas asking the UI to switch tools, e.g. after a lasso completes. */
  onRequestTool: (tool: ToolMode) => void;
}

interface Gesture {
  kind:
    | "ink"
    | "erase"
    | "pan"
    | "marquee"
    | "move"
    | "transform"
    | "lasso"
    | "frame"
    | "laser";
  pointerId: number;
  startScreen: Point;
  startWorld: Point;
  lastScreen: Point;
  handle?: HandleType;
  /**
   * Geometry of the units being transformed, captured at gesture start. A
   * $draw unit's x/y/width/height are only its selection outline — its ink is
   * the stroke itself, so moving one also needs the stroke's own original
   * points to translate from.
   */
  originals?: Map<
    ModelId,
    Pick<Unit, "x" | "y" | "width" | "height" | "rotation"> & { strokes?: Stroke[] }
  >;
  marquee?: Rect;
  erased?: Set<string>;
  /** Lasso path and laser trail, both in world coordinates. */
  path?: Point[];
}

/** Devices without a pressure sensor report 0 or a constant 0.5. */
const NO_PRESSURE = 0.5;

/** How long a laser trail point stays visible. */
const LASER_FADE_MS = 900;

export class CanvasController {
  private scene: HTMLCanvasElement | null = null;
  private overlay: HTMLCanvasElement | null = null;
  private sceneCtx: CanvasRenderingContext2D | null = null;
  private overlayCtx: CanvasRenderingContext2D | null = null;

  private dpr = 1;
  private viewport: Viewport = IDENTITY_VIEWPORT;
  private doc: NoteDocument | null = null;
  private pageIndex = 0;
  private session: EditSession | null = null;

  private tool: ToolMode = "pen";
  private pen: PenAttributes = PEN_PRESETS[0];
  private eraserSize = 16;
  private selection: ModelId[] = [];
  private lassoMode: LassoMode = "overlap";
  /** Laser trail points with their fade deadlines. */
  private laserTrail: { x: number; y: number; until: number }[] = [];

  private gesture: Gesture | null = null;
  private wetPoints: InkPoint[] = [];
  private wetFilter = new PointFilter();
  private strokeStart = 0;
  /** Timestamp of the last pen event, used to reject palm touches. */
  private lastPenAt = 0;

  private assets: AssetResolver = { getImage: () => undefined };
  private frame = 0;
  private sceneDirty = true;
  private overlayDirty = true;
  /** False until the container reports a non-zero size — see `resize`. */
  private hasBeenSized = false;
  /** Ensures the automatic fit-to-page happens once per opened document. */
  private hasFitted = false;

  private detachTheme: (() => void) | null = null;

  constructor(private callbacks: ControllerCallbacks) {}

  // -- lifecycle -----------------------------------------------------------

  attach(scene: HTMLCanvasElement, overlay: HTMLCanvasElement): void {
    this.scene = scene;
    this.overlay = overlay;
    // Opaque scene compositing is measurably cheaper, and we always paint paper.
    this.sceneCtx = scene.getContext("2d", { alpha: false });
    this.overlayCtx = overlay.getContext("2d", {
      alpha: true,
      desynchronized: true,
    } as CanvasRenderingContext2DSettings);

    overlay.style.touchAction = "none";
    overlay.addEventListener("pointerdown", this.onPointerDown);
    overlay.addEventListener("pointermove", this.onPointerMove);
    overlay.addEventListener("pointerup", this.onPointerUp);
    overlay.addEventListener("pointercancel", this.onPointerCancel);
    overlay.addEventListener("wheel", this.onWheel, { passive: false });
    overlay.addEventListener("dblclick", this.onDoubleClick);

    this.detachTheme = onCanvasThemeChange(() => this.invalidateAll());

    this.resize();
    this.startLoop();
  }

  dispose(): void {
    const overlay = this.overlay;
    if (overlay) {
      overlay.removeEventListener("pointerdown", this.onPointerDown);
      overlay.removeEventListener("pointermove", this.onPointerMove);
      overlay.removeEventListener("pointerup", this.onPointerUp);
      overlay.removeEventListener("pointercancel", this.onPointerCancel);
      overlay.removeEventListener("wheel", this.onWheel);
      overlay.removeEventListener("dblclick", this.onDoubleClick);
    }
    this.detachTheme?.();
    this.detachTheme = null;
    if (this.frame) cancelAnimationFrame(this.frame);
    this.frame = 0;
    this.scene = null;
    this.overlay = null;
    this.sceneCtx = null;
    this.overlayCtx = null;
  }

  private startLoop(): void {
    const tick = () => {
      if (this.sceneDirty) this.paintScene();
      if (this.overlayDirty) this.paintOverlay();
      this.frame = requestAnimationFrame(tick);
    };
    this.frame = requestAnimationFrame(tick);
  }

  resize(): void {
    const { scene, overlay } = this;
    if (!scene || !overlay) return;
    const rect = scene.parentElement?.getBoundingClientRect();
    // A hidden or not-yet-laid-out container measures zero. Sizing the canvases
    // to that would be worse than leaving them alone, so wait for the
    // ResizeObserver to report a real size.
    if (!rect || rect.width === 0 || rect.height === 0) return;

    // Capping DPR at 2 keeps the backing store sane on 5K displays, where the
    // real ratio can make each canvas tens of megabytes.
    this.dpr = Math.min(window.devicePixelRatio || 1, 2);
    for (const canvas of [scene, overlay]) {
      canvas.width = Math.round(rect.width * this.dpr);
      canvas.height = Math.round(rect.height * this.dpr);
      canvas.style.width = `${rect.width}px`;
      canvas.style.height = `${rect.height}px`;
    }
    this.invalidateAll();

    // The first real size is the first moment "fit the page" means anything.
    // Fitting before this — which is what happens if the note opens while the
    // window is still being laid out, or while the view is hidden — computes a
    // zoom from a 300x150 default canvas and leaves the page at a wrong scale
    // that nothing later corrects.
    if (!this.hasBeenSized) {
      this.hasBeenSized = true;
      if (this.doc && !this.hasFitted) {
        this.hasFitted = true;
        this.fitPage();
      }
    }
  }

  // -- state in -------------------------------------------------------------

  setDocument(doc: NoteDocument | null, pageIndex: number, session: EditSession | null): void {
    const pageChanged = this.pageIndex !== pageIndex;
    const docChanged = this.doc !== doc;
    this.doc = doc;
    this.pageIndex = pageIndex;
    this.session = session;
    if (docChanged || pageChanged) this.invalidateAll();
    // A document that arrives after the canvas was already sized still needs
    // its initial fit; `resize` only fits when it is the one going first.
    if (docChanged && doc && this.hasBeenSized && !this.hasFitted) {
      this.hasFitted = true;
      this.fitPage();
    }
  }

  setTool(tool: ToolMode): void {
    this.tool = tool;
    if (this.overlay) this.overlay.style.cursor = cursorForTool(tool);
    this.overlayDirty = true;
  }

  setPen(pen: PenAttributes): void {
    this.pen = pen;
  }

  setEraserSize(size: number): void {
    this.eraserSize = size;
  }

  setLassoMode(mode: LassoMode): void {
    this.lassoMode = mode;
  }


  setSelection(ids: ModelId[]): void {
    this.selection = ids;
    this.overlayDirty = true;
  }

  setAssets(assets: AssetResolver): void {
    this.assets = assets;
    this.invalidateAll();
  }

  invalidateAll(): void {
    this.sceneDirty = true;
    this.overlayDirty = true;
  }

  getViewport(): Viewport {
    return this.viewport;
  }

  setViewport(viewport: Viewport): void {
    this.viewport = viewport;
    this.callbacks.onViewportChange(viewport);
    this.invalidateAll();
  }

  fitPage(): void {
    const page = this.page();
    const scene = this.scene;
    if (!page || !scene) return;
    // Guard against the default 300x150 canvas: fitting to that produces a
    // scale the user never asked for and cannot see being wrong.
    if (!this.hasBeenSized || scene.clientWidth === 0 || scene.clientHeight === 0) return;

    const rect: Rect = {
      x: 0,
      y: 0,
      width: page.paperWidth,
      height: page.paperHeight,
    };
    this.setViewport(fitRect(rect, scene.clientWidth, scene.clientHeight, 32));
  }

  zoomBy(factor: number): void {
    const scene = this.scene;
    if (!scene) return;
    this.setViewport(
      zoomAbout(this.viewport, scene.clientWidth / 2, scene.clientHeight / 2, factor),
    );
  }

  /**
   * Aborts whatever gesture is in flight.
   *
   * docs/15 §5 notes the original cancels an in-progress gesture around undo,
   * via `IUndoOrRedoEventHandler`. Same reason here: undoing mid-stroke would
   * otherwise commit a stroke onto a document that has already moved.
   */
  cancelGesture(): void {
    if (!this.gesture) return;
    this.gesture = null;
    this.wetPoints = [];
    this.wetFilter.reset();
    this.overlayDirty = true;
  }

  // -- painting -------------------------------------------------------------

  private page(): Page | null {
    return this.doc?.pages[this.pageIndex] ?? null;
  }

  private paintScene(): void {
    const ctx = this.sceneCtx;
    const scene = this.scene;
    if (!ctx || !scene) return;
    this.sceneDirty = false;

    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    const page = this.page();
    if (!page) {
      ctx.fillStyle = canvasTheme().workspace;
      ctx.fillRect(0, 0, scene.clientWidth, scene.clientHeight);
      return;
    }

    // The workspace and the sheet's shadow are `renderPage`'s job, not this
    // function's: `renderPage` opens by clearing the canvas, so anything
    // painted here first would be wiped — invisibly, because an opaque canvas
    // clears to black and black is what a dark workspace looks like.
    renderPage(ctx, page, {
      background: canvasTheme().workspace,
      pageShadow: true,
      viewport: this.viewport,
      viewWidth: scene.clientWidth,
      viewHeight: scene.clientHeight,
      assets: this.assets,
      pageNumber: this.pageIndex + 1,
      pageCount: this.doc?.pages.length ?? 1,
    });
  }

  private paintOverlay(): void {
    const ctx = this.overlayCtx;
    const overlay = this.overlay;
    if (!ctx || !overlay) return;
    this.overlayDirty = false;

    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    ctx.clearRect(0, 0, overlay.clientWidth, overlay.clientHeight);

    ctx.save();
    ctx.translate(this.viewport.tx, this.viewport.ty);
    ctx.scale(this.viewport.scale, this.viewport.scale);

    // The stroke under the pen, drawn alone.
    if (this.wetPoints.length > 0) {
      drawStroke(ctx, {
        id: "wet",
        points: this.wetPoints,
        pen: this.pen,
        bounds: strokeBounds(this.wetPoints, this.pen.width),
      });
    }

    if (this.gesture?.kind === "lasso" && this.gesture.path) {
      const path = this.gesture.path;
      ctx.strokeStyle = canvasTheme().selection;
      ctx.lineWidth = 1.5 / this.viewport.scale;
      ctx.setLineDash([5 / this.viewport.scale, 4 / this.viewport.scale]);
      ctx.beginPath();
      ctx.moveTo(path[0].x, path[0].y);
      for (const p of path.slice(1)) ctx.lineTo(p.x, p.y);
      // Show the loop the release will close, rather than leaving the user to
      // imagine it.
      ctx.closePath();
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.fillStyle = canvasTheme().selectionFill;
      ctx.fill();
    }

    if (this.gesture?.kind === "frame" && this.gesture.marquee) {
      const r = normalizeRect(this.gesture.marquee);
      ctx.strokeStyle = canvasTheme().selection;
      ctx.lineWidth = 1.5 / this.viewport.scale;
      ctx.strokeRect(r.x, r.y, r.width, r.height);
      ctx.fillStyle = canvasTheme().selectionFillFaint;
      ctx.fillRect(r.x, r.y, r.width, r.height);
    }

    if (this.gesture?.kind === "marquee" && this.gesture.marquee) {
      const r = normalizeRect(this.gesture.marquee);
      ctx.strokeStyle = canvasTheme().selection;
      ctx.lineWidth = 1 / this.viewport.scale;
      ctx.setLineDash([6 / this.viewport.scale, 4 / this.viewport.scale]);
      ctx.strokeRect(r.x, r.y, r.width, r.height);
      ctx.setLineDash([]);
      ctx.fillStyle = canvasTheme().selectionFill;
      ctx.fillRect(r.x, r.y, r.width, r.height);
    }

    this.paintLaser(ctx);

    ctx.restore();

    this.paintSelectionHandles(ctx);
    this.paintEraserCursor(ctx);
  }

  /**
   * The laser trail.
   *
   * Deliberately not part of the document: docs/02 lists the laser pointer
   * among the editing tools, but it marks nothing permanent, so it never opens
   * a transaction and never produces an undo entry. It lives on the overlay and
   * fades on a timer.
   */
  private pushLaser(world: Point): void {
    this.laserTrail.push({ x: world.x, y: world.y, until: Date.now() + LASER_FADE_MS });
    this.overlayDirty = true;
  }

  private paintLaser(ctx: CanvasRenderingContext2D): void {
    if (this.laserTrail.length === 0) return;
    const now = Date.now();
    this.laserTrail = this.laserTrail.filter((p) => p.until > now);
    if (this.laserTrail.length === 0) {
      this.overlayDirty = true;
      return;
    }

    ctx.save();
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    for (let i = 1; i < this.laserTrail.length; i++) {
      const a = this.laserTrail[i - 1];
      const b = this.laserTrail[i];
      const life = (b.until - now) / LASER_FADE_MS;
      ctx.globalAlpha = Math.max(0, Math.min(1, life)) * 0.8;
      ctx.strokeStyle = canvasTheme().laser;
      ctx.lineWidth = 4 / this.viewport.scale;
      ctx.beginPath();
      ctx.moveTo(a.x, a.y);
      ctx.lineTo(b.x, b.y);
      ctx.stroke();
    }

    const head = this.laserTrail[this.laserTrail.length - 1];
    ctx.globalAlpha = 0.9;
    ctx.fillStyle = canvasTheme().laser;
    ctx.beginPath();
    ctx.arc(head.x, head.y, 5 / this.viewport.scale, 0, Math.PI * 2);
    ctx.fill();
    ctx.restore();

    // Keep repainting while anything is still fading.
    this.overlayDirty = true;
  }

  /**
   * Handles are drawn in screen space so they stay a constant size at any zoom.
   * Their positions come from world space; only the glyphs are screen-sized.
   */
  private paintSelectionHandles(ctx: CanvasRenderingContext2D): void {
    const page = this.page();
    if (!page || this.selection.length === 0 || this.tool !== "select") return;

    const units = page.layers
      .flatMap((l) => l.units)
      .filter((u) => this.selection.includes(u.id));
    if (units.length === 0) return;

    for (const unit of units) {
      ctx.save();
      ctx.translate(this.viewport.tx, this.viewport.ty);
      ctx.scale(this.viewport.scale, this.viewport.scale);
      if (unit.rotation !== 0) {
        const cx = unit.x + unit.width / 2;
        const cy = unit.y + unit.height / 2;
        ctx.translate(cx, cy);
        ctx.rotate(unit.rotation);
        ctx.translate(-cx, -cy);
      }

      ctx.strokeStyle = canvasTheme().selection;
      ctx.lineWidth = 1.5 / this.viewport.scale;
      ctx.strokeRect(unit.x, unit.y, unit.width, unit.height);

      // Ink only supports moving, not resizing or rotating — showing handles
      // that don't do anything would just invite a dead drag.
      if (units.length === 1 && unit.type !== "$draw") {
        const positions = handlePositions(unit);
        const size = HANDLE_SIZE / this.viewport.scale;
        ctx.fillStyle = canvasTheme().handleFill;
        for (const [name, p] of Object.entries(positions)) {
          if (name === "move") continue;
          if (name === "rotate") {
            const ry = p.y - ROTATE_OFFSET / this.viewport.scale;
            ctx.beginPath();
            ctx.moveTo(p.x, p.y);
            ctx.lineTo(p.x, ry);
            ctx.stroke();
            ctx.beginPath();
            ctx.arc(p.x, ry, size / 2, 0, Math.PI * 2);
            ctx.fill();
            ctx.stroke();
            continue;
          }
          ctx.fillRect(p.x - size / 2, p.y - size / 2, size, size);
          ctx.strokeRect(p.x - size / 2, p.y - size / 2, size, size);
        }
      }
      ctx.restore();
    }
  }

  private paintEraserCursor(ctx: CanvasRenderingContext2D): void {
    if (this.tool !== "eraser" || !this.gesture || this.gesture.kind !== "erase") return;
    const screen = this.gesture.lastScreen;
    ctx.save();
    ctx.strokeStyle = "rgba(30,30,30,0.65)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.arc(screen.x, screen.y, (this.eraserSize / 2) * this.viewport.scale, 0, Math.PI * 2);
    ctx.stroke();
    ctx.restore();
  }

  // -- input ---------------------------------------------------------------

  private localPoint(e: PointerEvent): Point {
    const rect = this.overlay!.getBoundingClientRect();
    return { x: e.clientX - rect.left, y: e.clientY - rect.top };
  }

  /**
   * Rejects palm contact: once a pen has been seen, touch input is treated as
   * the hand resting on the screen rather than as a second cursor.
   */
  private shouldIgnore(e: PointerEvent): boolean {
    if (e.pointerType === "pen") {
      this.lastPenAt = e.timeStamp;
      return false;
    }
    if (e.pointerType === "touch" && e.timeStamp - this.lastPenAt < 700) return true;
    return false;
  }

  private normalizedPressure(e: PointerEvent): number {
    if (e.pointerType === "mouse") return NO_PRESSURE;
    if (e.pressure <= 0) return NO_PRESSURE;
    return e.pressure;
  }

  private onPointerDown = (e: PointerEvent): void => {
    if (this.shouldIgnore(e) || !this.overlay) return;
    if (e.button !== 0 && e.button !== 1) return;

    const screen = this.localPoint(e);
    const world = screenToWorld(this.viewport, screen.x, screen.y);
    this.capturePointer(e.pointerId);

    const base = {
      pointerId: e.pointerId,
      startScreen: screen,
      startWorld: world,
      lastScreen: screen,
    };

    // Middle-drag and space-drag always pan, whatever tool is active.
    if (e.button === 1 || this.tool === "pan") {
      this.gesture = { ...base, kind: "pan" };
      return;
    }

    switch (this.tool) {
      case "pen": {
        this.gesture = { ...base, kind: "ink" };
        this.strokeStart = e.timeStamp;
        this.wetFilter.reset();
        this.wetPoints = [];
        this.pushInkSample(world.x, world.y, this.normalizedPressure(e), 0);
        break;
      }

      case "eraser":
        this.gesture = { ...base, kind: "erase", erased: new Set() };
        this.eraseAt(world);
        break;

      case "select":
        // Answering wins over selecting: a student tapping a choice means to
        // vote, not to pick the unit up.
        if (!e.shiftKey && this.tryAnswerSurvey(world)) break;
        this.beginSelectGesture(base, world, e.shiftKey);
        break;

      case "lasso":
        this.gesture = { ...base, kind: "lasso", path: [world] };
        break;

      case "shape":
      case "form":
        this.gesture = { ...base, kind: "frame" };
        break;

      case "laser":
        this.gesture = { ...base, kind: "laser", path: [world] };
        this.pushLaser(world);
        break;

      case "text":
      case "sticky":
      case "image":
      case "survey":
        this.callbacks.onPlace(this.tool, world);
        break;
    }
    this.overlayDirty = true;
  };

  private beginSelectGesture(
    base: Omit<Gesture, "kind">,
    world: Point,
    additive: boolean,
  ): void {
    const page = this.page();
    if (!page) return;

    // A handle on an already-selected unit wins over anything underneath it.
    if (this.selection.length === 1) {
      const unit = page.layers
        .flatMap((l) => l.units)
        .find((u) => u.id === this.selection[0]);
      if (unit) {
        const handle = hitTestHandle(unit, world.x, world.y, this.viewport.scale);
        if (handle && handle !== "move") {
          this.gesture = {
            ...base,
            kind: "transform",
            handle,
            originals: this.captureGeometry(page, this.selection),
          };
          return;
        }
      }
    }

    const hit = hitTestUnit(page, world.x, world.y);
    if (hit) {
      const already = this.selection.includes(hit.unit.id);
      const next = additive
        ? already
          ? this.selection.filter((id) => id !== hit.unit.id)
          : [...this.selection, hit.unit.id]
        : already
          ? this.selection
          : [hit.unit.id];
      this.selection = next;
      this.callbacks.onSelectionChange(next);
      this.gesture = {
        ...base,
        kind: "move",
        originals: this.captureGeometry(page, next),
      };
      return;
    }

    if (!additive) {
      this.selection = [];
      this.callbacks.onSelectionChange([]);
    }
    this.gesture = {
      ...base,
      kind: "marquee",
      marquee: { x: world.x, y: world.y, width: 0, height: 0 },
    };
  }

  private captureGeometry(page: Page, ids: ModelId[]) {
    const map = new Map<
      ModelId,
      Pick<Unit, "x" | "y" | "width" | "height" | "rotation"> & { strokes?: Stroke[] }
    >();
    for (const layer of page.layers) {
      for (const unit of layer.units) {
        if (!ids.includes(unit.id)) continue;
        map.set(unit.id, {
          x: unit.x,
          y: unit.y,
          width: unit.width,
          height: unit.height,
          rotation: unit.rotation,
          strokes:
            unit.type === "$draw"
              ? unit.strokes.map((s) => ({ ...s, points: s.points.slice() }))
              : undefined,
        });
      }
    }
    return map;
  }

  /**
   * Pointer capture keeps events coming while the pointer leaves the canvas.
   * It is an optimisation, not a precondition: `setPointerCapture` throws
   * `NotFoundError` when the pointer is no longer active — which happens for
   * real when a pen leaves range or the OS cancels the pointer between the
   * event being queued and this handler running. Letting that propagate would
   * abort the rest of `pointerdown`, so the gesture would silently never start.
   */
  private capturePointer(pointerId: number): void {
    try {
      this.overlay?.setPointerCapture(pointerId);
    } catch {
      // Without capture the gesture still works; it just ends early if the
      // pointer leaves the canvas.
    }
  }

  private releasePointer(pointerId: number): void {
    try {
      this.overlay?.releasePointerCapture(pointerId);
    } catch {
      // Already released, or never captured.
    }
  }

  private onPointerMove = (e: PointerEvent): void => {
    const gesture = this.gesture;
    if (!gesture || e.pointerId !== gesture.pointerId) return;

    const screen = this.localPoint(e);
    const world = screenToWorld(this.viewport, screen.x, screen.y);

    switch (gesture.kind) {
      case "ink": {
        // Consuming coalesced events is the difference between a smooth curve
        // and a polygon: at speed the browser batches several samples into one
        // pointermove and reports only the last.
        const coalesced =
          typeof e.getCoalescedEvents === "function" ? e.getCoalescedEvents() : [];
        const samples = coalesced.length > 0 ? coalesced : [e];
        const rect = this.overlay!.getBoundingClientRect();
        for (const sample of samples) {
          const p = screenToWorld(
            this.viewport,
            sample.clientX - rect.left,
            sample.clientY - rect.top,
          );
          this.pushInkSample(
            p.x,
            p.y,
            this.normalizedPressure(sample),
            sample.timeStamp - this.strokeStart,
          );
        }
        break;
      }

      case "erase":
        this.eraseAt(world);
        break;

      case "pan":
        this.setViewport(
          panBy(this.viewport, screen.x - gesture.lastScreen.x, screen.y - gesture.lastScreen.y),
        );
        break;

      case "marquee":
        gesture.marquee = {
          x: gesture.startWorld.x,
          y: gesture.startWorld.y,
          width: world.x - gesture.startWorld.x,
          height: world.y - gesture.startWorld.y,
        };
        break;

      case "move":
      case "transform":
        this.applyLiveTransform(gesture, world, e.shiftKey);
        break;

      case "lasso":
        gesture.path?.push(world);
        break;

      case "frame":
        gesture.marquee = frameFrom(gesture.startWorld, world, e.shiftKey);
        break;

      case "laser":
        this.pushLaser(world);
        break;
    }

    gesture.lastScreen = screen;
    this.overlayDirty = true;
  };

  private pushInkSample(x: number, y: number, pressure: number, t: number): void {
    const f = this.wetFilter.filter(x, y, pressure, t);
    const last = this.wetPoints[this.wetPoints.length - 1];
    // Drop samples the pen barely moved for; they bloat the document and make
    // the outline self-intersect.
    if (last && Math.hypot(f.x - last.x, f.y - last.y) < 0.35) return;
    this.wetPoints.push({ x: f.x, y: f.y, p: f.p, t });
  }

  /**
   * Moves or resizes live during the drag, without recording an undo entry —
   * the whole gesture becomes one entry when the pointer lifts.
   */
  private applyLiveTransform(gesture: Gesture, world: Point, keepAspect: boolean): void {
    const { session, doc } = this;
    const page = this.page();
    if (!session || !doc || !page || !gesture.originals) return;

    const dx = world.x - gesture.startWorld.x;
    const dy = world.y - gesture.startWorld.y;

    session.beginEdit(gesture.kind === "move" ? "移動" : "変形");
    for (const [unitId, original] of gesture.originals) {
      const located = locate(page, unitId);
      if (!located) continue;

      let after: Partial<Unit>;
      if (located.unit.type === "$draw") {
        // A $draw unit's x/y/width/height are only its selection outline —
        // the ink itself is the stroke's own recorded points, so moving it
        // means translating those (and refreshing the outline to match).
        // Resizing/rotating stroke geometry isn't implemented, so anything
        // but a plain move is a no-op here.
        const stroke = original.strokes?.[0];
        if (gesture.kind !== "move" || !stroke) continue;
        const points = stroke.points.map((p) => ({ ...p, x: p.x + dx, y: p.y + dy }));
        const bounds = strokeBounds(points, stroke.pen.width);
        after = {
          x: bounds.x,
          y: bounds.y,
          width: bounds.width,
          height: bounds.height,
          strokes: [{ ...stroke, points, bounds }],
        } as Partial<Unit>;
      } else if (gesture.kind === "move") {
        after = { x: original.x + dx, y: original.y + dy };
      } else if (gesture.handle === "rotate") {
        const cx = original.x + original.width / 2;
        const cy = original.y + original.height / 2;
        after = { rotation: Math.atan2(world.y - cy, world.x - cx) + Math.PI / 2 };
      } else if (gesture.handle) {
        after = resizeUnit({ ...located.unit, ...original }, gesture.handle, dx, dy, keepAspect);
      } else {
        continue;
      }

      session.record({
        kind: "unit.update",
        pageId: page.id,
        layerId: located.layerId,
        unitId,
        before: original as Partial<Unit>,
        after,
      });
    }
    session.endEdit();
    this.sceneDirty = true;
  }

  private eraseAt(world: Point): void {
    const { session } = this;
    const page = this.page();
    const gesture = this.gesture;
    if (!session || !page || !gesture?.erased) return;

    const radius = this.eraserSize / 2;

    for (const layer of page.layers) {
      if (!layer.visible || layer.locked) continue;
      for (const unit of layer.units) {
        if (unit.type !== "$draw") continue;
        const local = toUnitLocal(unit, world.x, world.y);

        for (let i = unit.strokes.length - 1; i >= 0; i--) {
          const stroke = unit.strokes[i];
          if (gesture.erased.has(stroke.id)) continue;
          if (!rectContainsPoint(inflate(stroke.bounds, radius), local.x, local.y)) continue;
          if (distanceToStroke(stroke, local.x, local.y) > radius + stroke.pen.width / 2) continue;

          gesture.erased.add(stroke.id);
          // Every erase in one drag folds into a single undo entry, so undo
          // restores the whole sweep rather than one stroke at a time.
          session.beginEdit("消しゴム");
          if (unit.strokes.length === 1) {
            // The common case: a stroke has its own unit, so erasing it means
            // removing that unit rather than leaving an empty one behind.
            session.record({
              kind: "unit.remove",
              pageId: page.id,
              layerId: layer.id,
              index: layer.units.indexOf(unit),
              unit,
            });
          } else {
            // A unit still carrying more than one stroke — from an imported
            // note, or a remote peer's — only loses the one stroke erased.
            session.record({
              kind: "stroke.remove",
              pageId: page.id,
              layerId: layer.id,
              unitId: unit.id,
              index: i,
              stroke,
            });
          }
          session.endEdit();
          this.sceneDirty = true;
        }
      }
    }
  }

  private onPointerUp = (e: PointerEvent): void => {
    const gesture = this.gesture;
    if (!gesture || e.pointerId !== gesture.pointerId) return;
    this.releasePointer(e.pointerId);

    if (gesture.kind === "ink") this.commitStroke();

    if (gesture.kind === "marquee" && gesture.marquee) {
      const page = this.page();
      if (page) {
        const ids = unitsInRect(page, gesture.marquee).map((u) => u.id);
        this.selection = ids;
        this.callbacks.onSelectionChange(ids);
      }
    }

    if (gesture.kind === "lasso" && gesture.path) {
      const page = this.page();
      // A stray tap is not a loop; three points is the minimum that encloses
      // anything at all.
      if (page && gesture.path.length >= 3) {
        const polygon = simplifyPolygon(gesture.path);
        const ids: ModelId[] = [];
        for (const layer of page.layers) {
          if (!layer.visible || layer.locked) continue;
          for (const unit of layer.units) {
            if (unitInLasso(unit, polygon, this.lassoMode)) ids.push(unit.id);
          }
        }
        this.selection = ids;
        this.callbacks.onSelectionChange(ids);
        // Hand over to the select tool: a lasso that leaves you unable to move
        // what you just selected has only done half the job.
        if (ids.length > 0) {
          this.tool = "select";
          this.callbacks.onRequestTool("select");
        }
      }
    }

    if (gesture.kind === "frame" && gesture.marquee) {
      const frame = normalizeRect(gesture.marquee);
      // Ignore an accidental click; a frame needs real extent to be useful.
      if (frame.width >= 8 && frame.height >= 8) {
        this.callbacks.onPlaceFrame(this.tool, frame);
      }
    }

    this.gesture = null;
    this.overlayDirty = true;
  };

  private onPointerCancel = (): void => {
    this.cancelGesture();
  };

  /**
   * Turns the wet stroke into a committed one.
   *
   * Ordering matters: the document is updated and the scene repainted in the
   * same frame that clears the overlay, so the stroke is never absent from both
   * canvases at once — that gap is what shows up as a flicker at pointer-up.
   */
  private commitStroke(): void {
    const { session, doc } = this;
    const page = this.page();
    if (!session || !doc || !page || this.wetPoints.length === 0) {
      this.wetPoints = [];
      return;
    }

    const points = simplify(this.wetPoints, 0.6);
    const stroke: Stroke = {
      id: newStrokeId(),
      points,
      pen: { ...this.pen },
      bounds: strokeBounds(points, this.pen.width),
    };

    const layer = currentLayer(page);
    if (!layer) {
      this.wetPoints = [];
      return;
    }

    // Each stroke gets its own $draw unit, sized to its own bounds, so it can
    // be selected and moved independently of every other stroke on the layer.
    const unit = createDrawUnit();
    unit.x = stroke.bounds.x;
    unit.y = stroke.bounds.y;
    unit.width = stroke.bounds.width;
    unit.height = stroke.bounds.height;
    unit.strokes = [stroke];

    session.transact("ペン", () => {
      session.record({
        kind: "unit.add",
        pageId: page.id,
        layerId: layer.id,
        index: layer.units.length,
        unit,
      });
    });

    this.wetPoints = [];
    this.wetFilter.reset();
    this.sceneDirty = true;
    this.paintScene();
  }

  private onWheel = (e: WheelEvent): void => {
    e.preventDefault();
    const rect = this.overlay!.getBoundingClientRect();
    const x = e.clientX - rect.left;
    const y = e.clientY - rect.top;

    // Both Chromium and WebKit deliver trackpad pinch as wheel + ctrlKey.
    if (e.ctrlKey || e.metaKey) {
      this.setViewport(zoomAbout(this.viewport, x, y, Math.exp(-e.deltaY * 0.01)));
    } else {
      this.setViewport(panBy(this.viewport, -e.deltaX, -e.deltaY));
    }
  };

  /**
   * A tap inside a survey's choice list records a vote.
   *
   * Answering has to be a plain click on the unit rather than a mode or a
   * dialog: in a classroom the respondent is a student who was handed the note,
   * and asking them to find a tool first would lose most of them.
   */
  private tryAnswerSurvey(world: Point): boolean {
    const page = this.page();
    const session = this.session;
    if (!page || !session) return false;

    const hit = hitTestUnit(page, world.x, world.y);
    if (!hit || hit.unit.type !== "$surveyunit") return false;

    const unit = hit.unit;
    if (!unit.allowAnswer) return false;
    // Results replace the choice list once they are showing, so there is
    // nothing to click.
    const total = Object.values(unit.result).reduce((sum, n) => sum + n, 0);
    if (total > 0 && unit.publish) return false;

    const ctx = this.sceneCtx;
    if (!ctx) return false;
    const local = toUnitLocal(unit, world.x, world.y);
    const index = surveyChoiceAt(ctx, unit, local.y);
    if (index === null) return false;

    const wasChosen = unit.answer.includes(index);
    const answer =
      unit.surveyKind === "radio"
        ? wasChosen
          ? []
          : [index]
        : wasChosen
          ? unit.answer.filter((a) => a !== index)
          : [...unit.answer, index];

    // The tally moves with the answer, so a changed mind does not leave a
    // stale vote behind.
    const result = { ...unit.result };
    const bump = (i: number, by: number) => {
      const next = (result[String(i)] ?? 0) + by;
      if (next > 0) result[String(i)] = next;
      else delete result[String(i)];
    };
    for (const previous of unit.answer) {
      if (!answer.includes(previous)) bump(previous, -1);
    }
    for (const next of answer) {
      if (!unit.answer.includes(next)) bump(next, 1);
    }

    session.transact("アンケートに回答", () => {
      session.record({
        kind: "unit.update",
        pageId: page.id,
        layerId: hit.layer.id,
        unitId: unit.id,
        before: { answer: unit.answer, result: unit.result },
        after: { answer, result },
      });
    });
    this.sceneDirty = true;
    return true;
  }

  private onDoubleClick = (e: MouseEvent): void => {
    const page = this.page();
    if (!page || !this.overlay) return;
    const rect = this.overlay.getBoundingClientRect();
    const world = screenToWorld(this.viewport, e.clientX - rect.left, e.clientY - rect.top);
    const hit = hitTestUnit(page, world.x, world.y);
    if (hit && (hit.unit.type === "$text" || hit.unit.type === "$flipunit")) {
      this.callbacks.onEditText(hit.unit.id);
    }
  };

  /** Renders the current page to a PNG data URL, for library thumbnails. */
  renderThumbnail(maxWidth = 320): string | null {
    const page = this.page();
    if (!page) return null;

    const scale = maxWidth / page.paperWidth;
    const canvas = document.createElement("canvas");
    canvas.width = Math.round(page.paperWidth * scale);
    canvas.height = Math.round(page.paperHeight * scale);
    const ctx = canvas.getContext("2d");
    if (!ctx) return null;

    ctx.fillStyle = page.paperColor;
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    renderPage(ctx, page, {
      viewport: { scale, tx: 0, ty: 0 },
      viewWidth: canvas.width,
      viewHeight: canvas.height,
      assets: this.assets,
      pageNumber: this.pageIndex + 1,
      pageCount: this.doc?.pages.length ?? 1,
    });
    return canvas.toDataURL("image/png");
  }
}

// ---------------------------------------------------------------------------

function locate(page: Page, unitId: ModelId): { layerId: ModelId; unit: Unit } | null {
  for (const layer of page.layers) {
    const unit = layer.units.find((u) => u.id === unitId);
    if (unit) return { layerId: layer.id, unit };
  }
  return null;
}

function inflate(rect: Rect, by: number): Rect {
  return {
    x: rect.x - by,
    y: rect.y - by,
    width: rect.width + by * 2,
    height: rect.height + by * 2,
  };
}

/**
 * The rect a drag defines. Holding shift constrains it to a square, which is
 * what makes circles and 45-degree lines reachable without a separate tool.
 */
function frameFrom(start: Point, current: Point, constrain: boolean): Rect {
  let width = current.x - start.x;
  let height = current.y - start.y;
  if (constrain) {
    const size = Math.max(Math.abs(width), Math.abs(height));
    width = Math.sign(width || 1) * size;
    height = Math.sign(height || 1) * size;
  }
  return { x: start.x, y: start.y, width, height };
}

function cursorForTool(tool: ToolMode): string {
  switch (tool) {
    case "pen":
    case "lasso":
    case "shape":
    case "form":
    case "laser":
      return "crosshair";
    case "eraser":
      return "cell";
    case "pan":
      return "grab";
    case "text":
      return "text";
    case "sticky":
    case "image":
    case "survey":
      return "copy";
    default:
      return "default";
  }
}

export { HANDLE_CURSORS };
