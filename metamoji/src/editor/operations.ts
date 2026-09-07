/**
 * Selection-level editing operations.
 *
 * Each of these is one user gesture, so each opens exactly one transaction and
 * produces exactly one undo entry — the rule docs/15 §3 describes.
 *
 * They live outside the store so the same logic serves keyboard shortcuts, the
 * inspector buttons and the context menu without any of them owning it.
 */

import type { EditSession } from "./session";
import { newStrokeId, newUnitId } from "../model/ids";
import { recomputeBounds, unionRect } from "../model/stroke";
import type { Layer, ModelId, NoteDocument, Page, Unit } from "../model/types";
import { currentLayer } from "../model/types";

/** Offset applied when pasting or duplicating in place, in document units. */
const PASTE_OFFSET = 24;

export interface Located {
  layer: Layer;
  layerIndex: number;
  unit: Unit;
  unitIndex: number;
}

export function locateUnits(page: Page, ids: readonly ModelId[]): Located[] {
  const wanted = new Set(ids);
  const out: Located[] = [];
  page.layers.forEach((layer, layerIndex) => {
    layer.units.forEach((unit, unitIndex) => {
      if (wanted.has(unit.id)) out.push({ layer, layerIndex, unit, unitIndex });
    });
  });
  return out;
}

/**
 * Deep-copies a unit under fresh ids.
 *
 * Ids must not be shared: two units with the same id would make selection,
 * hit testing and every delta that addresses a unit by id ambiguous.
 */
export function cloneUnit(unit: Unit, offset = 0): Unit {
  const base = {
    ...unit,
    id: newUnitId(),
    x: unit.x + offset,
    y: unit.y + offset,
  };

  if (base.type === "$draw") {
    // The stroke's points are its real position — x/y/width/height are only
    // the selection outline (see stroke.ts) — so the offset has to move both,
    // or the pasted copy's outline would land somewhere its ink isn't.
    const strokes = base.strokes.map((stroke) =>
      recomputeBounds({
        ...stroke,
        id: newStrokeId(),
        points: stroke.points.map((p) => ({ ...p, x: p.x + offset, y: p.y + offset })),
        pen: { ...stroke.pen },
      }),
    );
    const bounds = strokes.reduce(
      (acc, s) => unionRect(acc, s.bounds),
      strokes[0]?.bounds ?? { x: base.x, y: base.y, width: 0, height: 0 },
    );
    return { ...base, ...bounds, strokes };
  }
  return base;
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

export function deleteUnits(
  session: EditSession,
  page: Page,
  ids: readonly ModelId[],
): boolean {
  const located = locateUnits(page, ids);
  if (located.length === 0) return false;

  session.transact("削除", () => {
    // Back-to-front within each layer so the remaining indices stay valid.
    for (const item of [...located].sort((a, b) => b.unitIndex - a.unitIndex)) {
      session.record({
        kind: "unit.remove",
        pageId: page.id,
        layerId: item.layer.id,
        index: item.unitIndex,
        unit: item.unit,
      });
    }
  });
  return true;
}

// ---------------------------------------------------------------------------
// Duplicate and paste
// ---------------------------------------------------------------------------

export function duplicateUnits(
  session: EditSession,
  page: Page,
  ids: readonly ModelId[],
): ModelId[] {
  const located = locateUnits(page, ids);
  if (located.length === 0) return [];

  const created: ModelId[] = [];
  session.transact("複製", () => {
    for (const item of located) {
      const copy = cloneUnit(item.unit, PASTE_OFFSET);
      created.push(copy.id);
      session.record({
        kind: "unit.add",
        pageId: page.id,
        layerId: item.layer.id,
        index: item.layer.units.length,
        unit: copy,
      });
    }
  });
  return created;
}

/**
 * Pastes units into the page's active layer.
 *
 * `at` places the group's top-left corner at a point — used when pasting at the
 * pointer. Without it the copies land offset from the originals, which is what
 * you want when pasting back into the page they came from.
 */
export function pasteUnits(
  session: EditSession,
  page: Page,
  units: readonly Unit[],
  at?: { x: number; y: number },
): ModelId[] {
  if (units.length === 0) return [];
  const layer = currentLayer(page);
  if (!layer || layer.locked) return [];

  let dx = PASTE_OFFSET;
  let dy = PASTE_OFFSET;
  if (at) {
    const minX = Math.min(...units.map((u) => u.x));
    const minY = Math.min(...units.map((u) => u.y));
    dx = at.x - minX;
    dy = at.y - minY;
  }

  const created: ModelId[] = [];
  session.transact("貼り付け", () => {
    units.forEach((unit, i) => {
      const copy = cloneUnit(unit);
      copy.x = unit.x + dx;
      copy.y = unit.y + dy;
      created.push(copy.id);
      session.record({
        kind: "unit.add",
        pageId: page.id,
        layerId: layer.id,
        index: layer.units.length + i,
        unit: copy,
      });
    });
  });
  return created;
}

// ---------------------------------------------------------------------------
// Z-order
// ---------------------------------------------------------------------------

export type ZOrderMove = "front" | "back" | "forward" | "backward";

export function reorderUnits(
  session: EditSession,
  page: Page,
  ids: readonly ModelId[],
  move: ZOrderMove,
): boolean {
  const located = locateUnits(page, ids);
  if (located.length === 0) return false;

  const label =
    move === "front"
      ? "最前面へ"
      : move === "back"
        ? "最背面へ"
        : move === "forward"
          ? "前面へ"
          : "背面へ";

  let changed = false;
  session.transact(label, () => {
    // Group by layer: z-order is per-layer, and moving across layers would be a
    // different operation with different semantics.
    const byLayer = new Map<ModelId, Located[]>();
    for (const item of located) {
      const list = byLayer.get(item.layer.id) ?? [];
      list.push(item);
      byLayer.set(item.layer.id, list);
    }

    for (const [layerId, items] of byLayer) {
      const layer = page.layers.find((l) => l.id === layerId);
      if (!layer) continue;
      const last = layer.units.length - 1;

      // Processing order decides whether the selection keeps its internal
      // stacking. The two families need opposite orders:
      //
      //   front/back land every unit on the same index, so the *last* one moved
      //   ends up outermost — process from the far end inwards.
      //   forward/backward move one step, so the one nearest the destination
      //   must go first or it gets stepped over by its own neighbour.
      const farEndFirst = move === "front" || move === "backward";
      const ordered = [...items].sort((a, b) =>
        farEndFirst ? a.unitIndex - b.unitIndex : b.unitIndex - a.unitIndex,
      );

      // Track live positions as we go; each move shifts everything between.
      const positions = new Map(layer.units.map((u, i) => [u.id, i]));

      for (const item of ordered) {
        const from = positions.get(item.unit.id);
        if (from === undefined) continue;

        const to =
          move === "front"
            ? last
            : move === "back"
              ? 0
              : move === "forward"
                ? Math.min(last, from + 1)
                : Math.max(0, from - 1);
        if (to === from) continue;

        session.record({
          kind: "unit.reorder",
          pageId: page.id,
          layerId,
          unitId: item.unit.id,
          from,
          to,
        });
        changed = true;

        // Re-derive positions after the shift.
        for (const [id, pos] of positions) {
          if (id === item.unit.id) {
            positions.set(id, to);
          } else if (from < to && pos > from && pos <= to) {
            positions.set(id, pos - 1);
          } else if (from > to && pos >= to && pos < from) {
            positions.set(id, pos + 1);
          }
        }
      }
    }
  });
  return changed;
}

// ---------------------------------------------------------------------------
// Move between pages
// ---------------------------------------------------------------------------

export function nudgeUnits(
  session: EditSession,
  page: Page,
  ids: readonly ModelId[],
  dx: number,
  dy: number,
): boolean {
  const located = locateUnits(page, ids);
  if (located.length === 0) return false;

  session.transact("移動", () => {
    for (const item of located) {
      session.record({
        kind: "unit.update",
        pageId: page.id,
        layerId: item.layer.id,
        unitId: item.unit.id,
        before: { x: item.unit.x, y: item.unit.y },
        after: { x: item.unit.x + dx, y: item.unit.y + dy },
      });
    }
  });
  return true;
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/**
 * Units held for paste.
 *
 * Kept in memory rather than the system clipboard: a unit is a typed object
 * with asset references into *this* document, and serialising it as text would
 * either lose the assets or paste something unusable into other applications.
 * Copying a rendered image to the system clipboard is a separate feature.
 */
let clipboard: Unit[] = [];

export function copyUnits(page: Page, ids: readonly ModelId[]): number {
  const located = locateUnits(page, ids);
  clipboard = located.map((item) => cloneUnit(item.unit));
  return clipboard.length;
}

export function cutUnits(
  session: EditSession,
  page: Page,
  ids: readonly ModelId[],
): number {
  const count = copyUnits(page, ids);
  if (count > 0) deleteUnits(session, page, ids);
  return count;
}

export function clipboardUnits(): readonly Unit[] {
  return clipboard;
}

export function hasClipboardContent(): boolean {
  return clipboard.length > 0;
}

export function clearClipboard(): void {
  clipboard = [];
}

// ---------------------------------------------------------------------------
// Selection helpers
// ---------------------------------------------------------------------------

export function selectAll(page: Page): ModelId[] {
  const out: ModelId[] = [];
  for (const layer of page.layers) {
    if (!layer.visible || layer.locked) continue;
    for (const unit of layer.units) {
      if (unit.type === "$bgimage") continue;
      out.push(unit.id);
    }
  }
  return out;
}

export function pageOf(doc: NoteDocument, pageIndex: number): Page | undefined {
  return doc.pages[pageIndex];
}
