import { beforeEach, describe, expect, it } from "vitest";

import {
  clearClipboard,
  cloneUnit,
  copyUnits,
  cutUnits,
  clipboardUnits,
  deleteUnits,
  duplicateUnits,
  nudgeUnits,
  pasteUnits,
  reorderUnits,
  selectAll,
} from "./operations";
import { EditSession } from "./session";
import { createDocument, createTextUnit } from "../model/factory";
import { newStrokeId } from "../model/ids";
import { strokeBounds } from "../model/stroke";
import type { DrawUnit, NoteDocument, Page, TextUnit } from "../model/types";

function docWithUnits(count: number): NoteDocument {
  const doc = createDocument();
  const layer = doc.pages[0].layers[0];
  for (let i = 0; i < count; i++) {
    const unit = createTextUnit(i * 50, i * 50);
    unit.text = `unit ${i}`;
    layer.units.push(unit);
  }
  return doc;
}

function page(session: EditSession): Page {
  return session.document.pages[0];
}

function unitIds(session: EditSession): string[] {
  return page(session).layers[0].units.map((u) => u.id);
}

function texts(session: EditSession): string[] {
  return page(session).layers[0].units.map((u) => (u as TextUnit).text);
}

beforeEach(() => {
  clearClipboard();
});

describe("delete", () => {
  it("removes the selected units in one undo step", () => {
    const session = new EditSession(docWithUnits(4));
    const ids = unitIds(session);

    deleteUnits(session, page(session), [ids[1], ids[2]]);
    expect(texts(session)).toEqual(["unit 0", "unit 3"]);

    session.undo();
    expect(texts(session)).toEqual(["unit 0", "unit 1", "unit 2", "unit 3"]);
  });

  it("does nothing for an empty selection", () => {
    const session = new EditSession(docWithUnits(2));
    expect(deleteUnits(session, page(session), [])).toBe(false);
    expect(session.canUndo).toBe(false);
  });
});

describe("duplicate", () => {
  it("creates independent copies with new ids", () => {
    const session = new EditSession(docWithUnits(1));
    const [original] = unitIds(session);

    const created = duplicateUnits(session, page(session), [original]);
    expect(created).toHaveLength(1);
    expect(created[0]).not.toBe(original);
    expect(page(session).layers[0].units).toHaveLength(2);

    // The copy is offset so it is visibly not the original.
    const [a, b] = page(session).layers[0].units;
    expect(b.x).toBeGreaterThan(a.x);
  });

  it("gives a duplicated ink unit fresh stroke ids", () => {
    const doc = createDocument();
    const points = [
      { x: 0, y: 0, p: 0.5, t: 0 },
      { x: 10, y: 10, p: 0.5, t: 5 },
    ];
    const ink: DrawUnit = {
      id: "unit_ink",
      type: "$draw",
      x: 0,
      y: 0,
      width: 100,
      height: 100,
      rotation: 0,
      contentScale: 1,
      strokes: [
        {
          id: newStrokeId(),
          points,
          pen: {
            color: "#000",
            width: 2,
            penType: "ballpoint",
            opacity: 1,
            pressureSensitivity: 0.5,
          },
          bounds: strokeBounds(points, 2),
        },
      ],
    };
    doc.pages[0].layers[0].units.push(ink);

    const copy = cloneUnit(ink) as DrawUnit;
    expect(copy.id).not.toBe(ink.id);
    expect(copy.strokes[0].id).not.toBe(ink.strokes[0].id);
    // Points must be copied, not aliased — editing one must not move the other.
    copy.strokes[0].points[0].x = 999;
    expect(ink.strokes[0].points[0].x).toBe(0);
  });

  it("offsets a duplicated ink unit's actual points, not just its outline", () => {
    // x/y/width/height are only the selection outline for a $draw unit — the
    // ink itself is the stroke's own points — so an offset that moved only
    // the outline would paste a copy whose selection box sits somewhere its
    // ink doesn't.
    const points = [
      { x: 0, y: 0, p: 0.5, t: 0 },
      { x: 10, y: 10, p: 0.5, t: 5 },
    ];
    const ink: DrawUnit = {
      id: "unit_ink",
      type: "$draw",
      x: 0,
      y: 0,
      width: 100,
      height: 100,
      rotation: 0,
      contentScale: 1,
      strokes: [
        {
          id: newStrokeId(),
          points,
          pen: { color: "#000", width: 2, penType: "ballpoint", opacity: 1, pressureSensitivity: 0.5 },
          bounds: strokeBounds(points, 2),
        },
      ],
    };

    const copy = cloneUnit(ink, 24) as DrawUnit;
    expect(copy.strokes[0].points[0].x).toBe(24);
    expect(copy.strokes[0].points[0].y).toBe(24);
    // The outline must track where the offset points actually landed.
    expect(copy.x).toBe(copy.strokes[0].bounds.x);
    expect(copy.y).toBe(copy.strokes[0].bounds.y);
  });
});

describe("clipboard", () => {
  it("copies and pastes into the active layer", () => {
    const session = new EditSession(docWithUnits(2));
    const ids = unitIds(session);

    expect(copyUnits(page(session), [ids[0]])).toBe(1);
    pasteUnits(session, page(session), clipboardUnits());

    expect(page(session).layers[0].units).toHaveLength(3);
    expect(texts(session)[2]).toBe("unit 0");
  });

  it("pastes at a point when one is given", () => {
    const session = new EditSession(docWithUnits(1));
    copyUnits(page(session), unitIds(session));

    pasteUnits(session, page(session), clipboardUnits(), { x: 500, y: 400 });
    const pasted = page(session).layers[0].units[1];
    expect(pasted.x).toBe(500);
    expect(pasted.y).toBe(400);
  });

  it("cut removes the originals but keeps them pasteable", () => {
    const session = new EditSession(docWithUnits(2));
    const ids = unitIds(session);

    expect(cutUnits(session, page(session), [ids[0]])).toBe(1);
    expect(texts(session)).toEqual(["unit 1"]);

    pasteUnits(session, page(session), clipboardUnits());
    expect(texts(session)).toEqual(["unit 1", "unit 0"]);
  });

  it("pasted units are independent of the clipboard entry", () => {
    const session = new EditSession(docWithUnits(1));
    copyUnits(page(session), unitIds(session));

    pasteUnits(session, page(session), clipboardUnits());
    pasteUnits(session, page(session), clipboardUnits());

    const units = page(session).layers[0].units;
    const ids = new Set(units.map((u) => u.id));
    // Pasting twice must not produce two units sharing one id.
    expect(ids.size).toBe(units.length);
  });

  it("does not paste into a locked layer", () => {
    const session = new EditSession(docWithUnits(1));
    copyUnits(page(session), unitIds(session));
    session.transact("ロック", () => {
      session.record({
        kind: "layer.update",
        pageId: page(session).id,
        layerId: page(session).layers[0].id,
        before: { locked: false },
        after: { locked: true },
      });
    });

    expect(pasteUnits(session, page(session), clipboardUnits())).toEqual([]);
  });
});

describe("z-order", () => {
  it("brings a unit to the front", () => {
    const session = new EditSession(docWithUnits(4));
    const ids = unitIds(session);

    reorderUnits(session, page(session), [ids[0]], "front");
    expect(texts(session)).toEqual(["unit 1", "unit 2", "unit 3", "unit 0"]);
  });

  it("sends a unit to the back", () => {
    const session = new EditSession(docWithUnits(4));
    const ids = unitIds(session);

    reorderUnits(session, page(session), [ids[3]], "back");
    expect(texts(session)).toEqual(["unit 3", "unit 0", "unit 1", "unit 2"]);
  });

  it("moves one step forward and backward", () => {
    const session = new EditSession(docWithUnits(4));
    const ids = unitIds(session);

    reorderUnits(session, page(session), [ids[1]], "forward");
    expect(texts(session)).toEqual(["unit 0", "unit 2", "unit 1", "unit 3"]);

    reorderUnits(session, page(session), [ids[1]], "backward");
    expect(texts(session)).toEqual(["unit 0", "unit 1", "unit 2", "unit 3"]);
  });

  it("keeps the relative order of a multi-unit selection", () => {
    const session = new EditSession(docWithUnits(5));
    const ids = unitIds(session);

    reorderUnits(session, page(session), [ids[0], ids[1]], "front");
    // 0 and 1 end up on top, still in that order relative to each other.
    expect(texts(session)).toEqual(["unit 2", "unit 3", "unit 4", "unit 0", "unit 1"]);
  });

  it("keeps the relative order when sending several units to the back", () => {
    const session = new EditSession(docWithUnits(5));
    const ids = unitIds(session);

    reorderUnits(session, page(session), [ids[3], ids[4]], "back");
    expect(texts(session)).toEqual(["unit 3", "unit 4", "unit 0", "unit 1", "unit 2"]);
  });

  it("is a single undo step", () => {
    const session = new EditSession(docWithUnits(4));
    const ids = unitIds(session);
    const before = texts(session);

    reorderUnits(session, page(session), [ids[0], ids[1]], "front");
    session.undo();
    expect(texts(session)).toEqual(before);
  });

  it("does nothing when already at the edge", () => {
    const session = new EditSession(docWithUnits(3));
    const ids = unitIds(session);

    expect(reorderUnits(session, page(session), [ids[2]], "forward")).toBe(false);
    expect(reorderUnits(session, page(session), [ids[0]], "backward")).toBe(false);
  });
});

describe("nudge", () => {
  it("moves the selection by a delta as one undo step", () => {
    const session = new EditSession(docWithUnits(2));
    const ids = unitIds(session);

    nudgeUnits(session, page(session), ids, 10, -5);
    const units = page(session).layers[0].units;
    expect(units[0].x).toBe(10);
    expect(units[0].y).toBe(-5);
    expect(units[1].x).toBe(60);

    session.undo();
    expect(page(session).layers[0].units[0].x).toBe(0);
  });
});

describe("select all", () => {
  it("returns every selectable unit on the page", () => {
    const session = new EditSession(docWithUnits(3));
    expect(selectAll(page(session))).toHaveLength(3);
  });

  it("skips hidden and locked layers", () => {
    const session = new EditSession(docWithUnits(2));
    session.transact("隠す", () => {
      session.record({
        kind: "layer.update",
        pageId: page(session).id,
        layerId: page(session).layers[0].id,
        before: { visible: true },
        after: { visible: false },
      });
    });
    expect(selectAll(page(session))).toHaveLength(0);
  });
});
