/**
 * The note editor.
 *
 * Owns document loading, autosave, keyboard shortcuts and the asset cache, and
 * wires the canvas controller to the store. Everything that happens per pointer
 * event is inside the controller, not here.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";

import type { CanvasController, ToolMode } from "../canvas/controller";
import { CanvasHost } from "../components/CanvasHost";
import { Inspector } from "../components/Inspector";
import { TextEditOverlay } from "../components/TextEditOverlay";
import { Menu } from "../components/Menu";
import { useTranslation } from "../i18n/useTranslation";
import { PageStrip } from "../components/PageStrip";
import { Icon, type IconName } from "../components/Icon";
import { Toolbar } from "../components/Toolbar";
import { TOOLS } from "../editor/tools";
import * as api from "../ipc/api";
import { fromGeneric, toGeneric } from "../model/converter";
import {
  createBgImageUnit,
  createFlipUnit,
  createFormUnit,
  createImageUnit,
  createPage,
  createShapeUnit,
  createSurveyUnit,
  createTextUnit,
} from "../model/factory";
import { newTicket } from "../model/ids";
import type { ModelId, Point, Rect } from "../model/types";
import {
  clipboardUnits,
  copyUnits,
  cutUnits,
  deleteUnits,
  duplicateUnits,
  nudgeUnits,
  pasteUnits,
  reorderUnits,
  selectAll,
} from "../editor/operations";
import { currentLayer, searchableText } from "../model/types";
import { IDENTITY_VIEWPORT, type Viewport } from "../render/viewport";
import { EXPORT_DPI, renderPagesForExport, renderPageToDataUrl } from "../io/pageRender";
import { editFor, forgetEdit } from "../classroom/live";
import { parsePageRange, readPdfInfo, renderPdfPages } from "../io/pdf";
import { useAssetCache } from "../hooks/useAssetCache";
import { useEditorStore } from "../store/editorStore";
import { usePrefsStore } from "../store/prefsStore";
import { markSessionClosed, markSessionOpen } from "../store/sessionStore";


export function EditorScreen() {
  const { noteId } = useParams<{ noteId: string }>();
  const navigate = useNavigate();
  const { t } = useTranslation();
  const controllerRef = useRef<CanvasController | null>(null);

  const doc = useEditorStore((s) => s.doc);
  const session = useEditorStore((s) => s.session);
  const pageIndex = useEditorStore((s) => s.pageIndex);
  const saveState = useEditorStore((s) => s.saveState);
  const canUndo = useEditorStore((s) => s.canUndo);
  const canRedo = useEditorStore((s) => s.canRedo);
  const openDocument = useEditorStore((s) => s.openDocument);
  const closeDocument = useEditorStore((s) => s.closeDocument);
  const setPageIndex = useEditorStore((s) => s.setPageIndex);
  const addPage = useEditorStore((s) => s.addPage);
  const duplicatePage = useEditorStore((s) => s.duplicatePage);
  const deletePage = useEditorStore((s) => s.deletePage);
  const reorderPage = useEditorStore((s) => s.reorderPage);
  const undo = useEditorStore((s) => s.undo);
  const redo = useEditorStore((s) => s.redo);
  const setSaveState = useEditorStore((s) => s.setSaveState);
  const markSaved = useEditorStore((s) => s.markSaved);

  const [loadError, setLoadError] = useState<string | null>(null);
  const [showPages, setShowPages] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  /** What the last attempt to send this note's writing to its class did. */
  const [classSync, setClassSync] = useState<
    { kind: "sent"; strokes: number } | { kind: "failed"; message: string } | null
  >(null);
  const [showClassError, setShowClassError] = useState(false);
  /** True while this note's classroom is connected and pushing updates. */
  const [watching, setWatching] = useState(false);
  /** True for a note taken from a class box, connected or not. */
  const [classNote, setClassNote] = useState(false);
  const [resyncing, setResyncing] = useState(false);
  const [editingUnitId, setEditingUnitId] = useState<ModelId | null>(null);
  // The live viewport is kept in a ref so panning and zooming never re-render
  // the editor. It is mirrored into state only while a text box is open, since
  // that overlay is positioned in screen space and has to follow the canvas.
  const viewportRef = useRef<Viewport>(IDENTITY_VIEWPORT);
  const [overlayViewport, setOverlayViewport] = useState<Viewport>(IDENTITY_VIEWPORT);
  const editingRef = useRef<ModelId | null>(null);
  editingRef.current = editingUnitId;

  const handleViewportChange = useCallback((vp: Viewport) => {
    viewportRef.current = vp;
    if (editingRef.current) setOverlayViewport(vp);
  }, []);

  const beginTextEdit = useCallback((unitId: ModelId) => {
    setOverlayViewport(viewportRef.current);
    setEditingUnitId(unitId);
  }, []);
  const assets = useAssetCache(noteId ?? null, doc);

  // -- load ---------------------------------------------------------------

  useEffect(() => {
    if (!noteId) return;
    let cancelled = false;

    (async () => {
      try {
        const tree = await api.noteLoad(noteId);
        if (cancelled) return;
        const loaded = fromGeneric(tree);
        openDocument(loaded, noteId);
        markSessionOpen({ noteId, pageIndex: 0, title: loaded.meta.title });
      } catch (err) {
        if (!cancelled) setLoadError(String(err));
      }
    })();

    return () => {
      cancelled = true;
      // Leaving the editor is a clean exit, so the marker goes away. Anything
      // left behind means the app stopped without unwinding.
      markSessionClosed();
      closeDocument();
      void api.noteClose(noteId);
    };
  }, [noteId, openDocument, closeDocument]);

  // -- autosave -----------------------------------------------------------

  const save = useCallback(async () => {
    const state = useEditorStore.getState();
    if (!state.doc || !state.noteId) return;
    setSaveState("saving");
    try {
      await api.noteSave(
        toGeneric(state.doc),
        state.doc.meta.title,
        state.doc.meta.createdAt,
        state.doc.revision,
        searchableText(state.doc),
      );

      // Page 1 doubles as the library cover, so refresh it alongside the save.
      const thumbnail = controllerRef.current?.renderThumbnail();
      if (thumbnail && state.pageIndex === 0) {
        await api.noteSetThumbnail(
          state.noteId,
          state.doc.pages[0].id,
          state.doc.revision,
          thumbnail,
        );
      }
      markSaved();

      // A note copied out of a class box is only half saved when it is on
      // disk: what the user wrote is theirs to share, and the class sees it
      // through the room rather than through the file. Failing to send is not
      // a failed save — the writing is safe locally either way — so it is
      // reported on its own line.
      try {
        const sent = await api.classboxSendStrokes(state.noteId);
        setClassSync(sent > 0 ? { kind: "sent", strokes: sent } : null);
      } catch (err) {
        // One repair attempt before bothering the user: most failures here are
        // the two sides disagreeing about what has been sent, and that is
        // exactly what a resync settles.
        try {
          const report = await api.classnoteResync(state.noteId);
          setClassSync(
            report && report.problems.length === 0
              ? { kind: "sent", strokes: report.sent }
              : { kind: "failed", message: report?.problems.join(" / ") ?? String(err) },
          );
          if (report && report.problems.length > 0) setShowClassError(true);
        } catch {
          setClassSync({ kind: "failed", message: String(err) });
          setShowClassError(true);
        }
      }
    } catch (err) {
      console.error("save failed", err);
      setSaveState("error");
    }
  }, [markSaved, setSaveState]);

  // -- the classroom, while the note is open --------------------------------

  useEffect(() => {
    if (!noteId) return;
    let stopped = false;
    const off: Array<() => void> = [];
    let retry: ReturnType<typeof setTimeout> | null = null;

    void api
      .classboxOrigin(noteId)
      .then((origin) => {
        if (!stopped) setClassNote(origin !== null);
      })
      .catch(() => {});

    const connect = async () => {
      if (stopped) return;
      try {
        // Settle any disagreement about what has been sent *before* opening
        // the connection: the repair forgets how far each booth was read, and
        // the attach that follows then asks for the room from the beginning.
        // Anything the note already has is dropped on the way in.
        const report = await api.classnoteResync(noteId);
        applyGone(report?.gone ?? []);
        if (report && (report.neverArrived > 0 || report.sent > 0)) {
          setClassSync({ kind: "sent", strokes: report.sent });
        }
        setWatching(await api.classnoteWatch(noteId));
      } catch (err) {
        console.error("classroom watch failed", err);
        setWatching(false);
        // A room that is not there now may be in a moment; a note left open
        // over a dropped connection should not stay silent for ever.
        retry = setTimeout(() => void connect(), 15_000);
      }
    };

    void (async () => {
      off.push(
        await api.onClassNoteChange((change) => {
          if (change.noteId !== noteId) return;
          const session = useEditorStore.getState().session;
          const doc = useEditorStore.getState().doc;
          if (!session || !doc) return;
          const edit = editFor(doc, change);
          // Through `applyRemote`: someone else's writing is not this user's
          // to undo, and must not go back out as a Direction of our own.
          if (edit) session.applyRemote(edit);
        }),
      );
      off.push(
        await api.onClassNoteEnded((info) => {
          if (info.noteId !== noteId) return;
          setWatching(false);
          // Reconnecting also resyncs, which is what repairs whatever the
          // connection dropped in the middle of.
          retry = setTimeout(() => void connect(), 3_000);
        }),
      );
      if (stopped) {
        for (const stop of off) stop();
        return;
      }
      await connect();
    })();

    return () => {
      stopped = true;
      if (retry) clearTimeout(retry);
      for (const stop of off) stop();
      setWatching(false);
      setClassNote(false);
      void api.classnoteUnwatch();
    };
  }, [noteId]);

  /**
   * Compares the note with its classroom and repairs the difference.
   *
   * On the toolbar rather than buried in an error, because the two sides can
   * drift without either noticing — a post that did not land looks exactly
   * like one that did from here. Nothing to diagnose first: press it.
   */
  /** Stops showing what the room no longer holds. */
  const applyGone = (ids: readonly string[]) => {
    if (ids.length === 0) return;
    const { session, doc } = useEditorStore.getState();
    if (!session || !doc) return;
    const edit = forgetEdit(doc, ids);
    if (edit) session.applyRemote(edit);
  };

  const resync = useCallback(async () => {
    const id = useEditorStore.getState().noteId;
    if (!id 
      || resyncing) return;
    setResyncing(true);
    setShowClassError(false);
    try {
      const report = await api.classnoteResync(id);
      applyGone(report?.gone ?? []);
      if (report && report.problems.length > 0) {
        setClassSync({ kind: "failed", message: report.problems.join(" / ") });
        setShowClassError(true);
      } else {
        setClassSync({ kind: "sent", strokes: report?.sent ?? 0 });
      }
    } catch (err) {
      setClassSync({ kind: "failed", message: String(err) });
      setShowClassError(true);
    } finally {
      setResyncing(false);
    }
  }, [resyncing]);

  const autosaveSeconds = usePrefsStore((s) => s.autosaveSeconds);

  useEffect(() => {
    const state = useEditorStore.getState();
    if (noteId && state.doc) {
      markSessionOpen({ noteId, pageIndex, title: state.doc.meta.title });
    }
  }, [noteId, pageIndex, doc?.meta.title]);

  useEffect(() => {
    if (saveState !== "dirty") return;
    const timer = setTimeout(() => void save(), autosaveSeconds * 1000);
    return () => clearTimeout(timer);
  }, [saveState, save, doc, autosaveSeconds]);

  // -- keyboard -----------------------------------------------------------

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      if (target && /^(INPUT|TEXTAREA)$/.test(target.tagName)) return;
      if (target?.isContentEditable) return;

      const state = useEditorStore.getState();
      const s = state.session;
      const p = state.doc?.pages[state.pageIndex];
      const selection = state.selection;
      const mod = e.metaKey || e.ctrlKey;

      if (mod && e.key.toLowerCase() === "z") {
        e.preventDefault();
        // docs/15 §5: the original cancels an in-flight gesture around undo, so
        // a half-drawn stroke cannot commit onto an already-changed document.
        controllerRef.current?.cancelGesture();
        if (e.shiftKey) redo();
        else undo();
        return;
      }
      if (mod && e.key.toLowerCase() === "s") {
        e.preventDefault();
        void save();
        return;
      }
      if (mod && e.key === "0") {
        e.preventDefault();
        controllerRef.current?.fitPage();
        return;
      }

      // -- selection editing ------------------------------------------------
      if (s && p) {
        if (mod && e.key.toLowerCase() === "a") {
          e.preventDefault();
          state.setTool("select");
          state.setSelection(selectAll(p));
          return;
        }
        if (mod && e.key.toLowerCase() === "c" && selection.length > 0) {
          e.preventDefault();
          copyUnits(p, selection);
          return;
        }
        if (mod && e.key.toLowerCase() === "x" && selection.length > 0) {
          e.preventDefault();
          cutUnits(s, p, selection);
          state.setSelection([]);
          return;
        }
        if (mod && e.key.toLowerCase() === "v") {
          e.preventDefault();
          state.setSelection(pasteUnits(s, p, clipboardUnits()));
          return;
        }
        if (mod && e.key.toLowerCase() === "d" && selection.length > 0) {
          e.preventDefault();
          state.setSelection(duplicateUnits(s, p, selection));
          return;
        }
        if ((e.key === "Delete" || e.key === "Backspace") && selection.length > 0) {
          e.preventDefault();
          deleteUnits(s, p, selection);
          state.setSelection([]);
          return;
        }
        if (mod && e.key === "]" && selection.length > 0) {
          e.preventDefault();
          reorderUnits(s, p, selection, e.shiftKey ? "front" : "forward");
          return;
        }
        if (mod && e.key === "[" && selection.length > 0) {
          e.preventDefault();
          reorderUnits(s, p, selection, e.shiftKey ? "back" : "backward");
          return;
        }

        // Arrow keys nudge; shift makes it a coarse step.
        const arrows: Record<string, [number, number]> = {
          ArrowLeft: [-1, 0],
          ArrowRight: [1, 0],
          ArrowUp: [0, -1],
          ArrowDown: [0, 1],
        };
        const arrow = arrows[e.key];
        if (arrow && selection.length > 0) {
          e.preventDefault();
          const step = e.shiftKey ? 10 : 1;
          nudgeUnits(s, p, selection, arrow[0] * step, arrow[1] * step);
          return;
        }
      }

      if (e.key === "Escape") {
        controllerRef.current?.cancelGesture();
        state.setSelection([]);
        return;
      }

      // -- page navigation --------------------------------------------------
      if (!mod && e.key === "PageDown") {
        e.preventDefault();
        state.setPageIndex(state.pageIndex + 1);
        return;
      }
      if (!mod && e.key === "PageUp") {
        e.preventDefault();
        state.setPageIndex(state.pageIndex - 1);
        return;
      }

      // -- tools ------------------------------------------------------------
      if (mod) return;
      const tool = TOOLS.find((t) => t.shortcut.toLowerCase() === e.key.toLowerCase());
      if (tool) state.setTool(tool.id);
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [undo, redo, save]);

  // -- placement tools ----------------------------------------------------

  const handlePlace = useCallback(
    async (tool: ToolMode, world: Point) => {
      const state = useEditorStore.getState();
      const { session: s, doc: d, pageIndex: pi, noteId: nid } = state;
      if (!s || !d || !nid) return;
      const page = d.pages[pi];
      const layer = page && currentLayer(page);
      if (!page || !layer) return;

      if (tool === "text") {
        const unit = createTextUnit(world.x, world.y);
        s.transact("テキストを追加", () => {
          s.record({
            kind: "unit.add",
            pageId: page.id,
            layerId: layer.id,
            index: layer.units.length,
            unit,
          });
        });
        beginTextEdit(unit.id);
        state.setTool("select");
        state.setSelection([unit.id]);
        return;
      }

      if (tool === "sticky") {
        const unit = createFlipUnit(world.x, world.y);
        s.transact("付箋を追加", () => {
          s.record({
            kind: "unit.add",
            pageId: page.id,
            layerId: layer.id,
            index: layer.units.length,
            unit,
          });
        });
        beginTextEdit(unit.id);
        state.setTool("select");
        state.setSelection([unit.id]);
        return;
      }

      if (tool === "survey") {
        const unit = createSurveyUnit(world.x, world.y);
        s.transact("アンケートを追加", () => {
          s.record({
            kind: "unit.add",
            pageId: page.id,
            layerId: layer.id,
            index: layer.units.length,
            unit,
          });
        });
        state.setTool("select");
        state.setSelection([unit.id]);
        return;
      }

      if (tool === "image") {
        const dataUrl = await api.pickAndReadFile("画像", ["png", "jpg", "jpeg", "gif", "webp"]);
        if (!dataUrl) return;

        const size = await imageSize(dataUrl);
        const ticket = newTicket();
        await api.assetPut(nid, ticket, dataUrl);

        // Fit wide images to the page so an imported photo does not land
        // several times larger than the paper.
        const maxWidth = page.paperWidth * 0.6;
        const scale = size.width > maxWidth ? maxWidth / size.width : 1;
        const unit = createImageUnit(
          world.x,
          world.y,
          size.width * scale,
          size.height * scale,
          ticket,
        );

        s.transact("画像を追加", () => {
          s.record({
            kind: "unit.add",
            pageId: page.id,
            layerId: layer.id,
            index: layer.units.length,
            unit,
          });
        });
        state.setTool("select");
        state.setSelection([unit.id]);
      }
    },
    [beginTextEdit],
  );

  const handlePlaceFrame = useCallback((tool: ToolMode, frame: Rect) => {
    const state = useEditorStore.getState();
    const { session: s, doc: d, pageIndex: pi } = state;
    if (!s || !d) return;
    const p = d.pages[pi];
    const layer = p && currentLayer(p);
    if (!p || !layer || layer.locked) return;

    const unit =
      tool === "shape"
        ? createShapeUnit(
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            state.shapeKind,
            state.shapeStrokeColor,
            state.shapeFillColor,
          )
        : createFormUnit(frame.x, frame.y, frame.width, frame.height, state.formKind);

    s.transact(tool === "shape" ? "図形を追加" : "表・罫線を追加", () => {
      s.record({
        kind: "unit.add",
        pageId: p.id,
        layerId: layer.id,
        index: layer.units.length,
        unit,
      });
    });
    state.setTool("select");
    state.setSelection([unit.id]);
  }, []);

  // -- export / import ----------------------------------------------------

  const exportPdf = useCallback(async () => {
    const state = useEditorStore.getState();
    if (!state.doc) return;
    const path = await saveDialog({
      defaultPath: `${state.doc.meta.title}.pdf`,
      filters: [{ name: "PDF", extensions: ["pdf"] }],
    });
    if (!path) return;

    setBusy(t("PDF を書き出しています…"));
    try {
      const pages = renderPagesForExport(state.doc, assets, EXPORT_DPI.print);
      await api.exportPdf(path, state.doc.meta.title, pages);
    } catch (err) {
      setLoadError(t("PDF の書き出しに失敗しました: {error}", { error: String(err) }));
    } finally {
      setBusy(null);
    }
  }, [assets]);

  const exportPng = useCallback(async () => {
    const state = useEditorStore.getState();
    const p = state.doc?.pages[state.pageIndex];
    if (!state.doc || !p) return;

    setBusy(t("画像を書き出しています…"));
    try {
      const dataUrl = renderPageToDataUrl(p, {
        assets,
        scale: EXPORT_DPI.print / 150,
      });
      await api.pickAndWriteFile(
        `${state.doc.meta.title}-${state.pageIndex + 1}.png`,
        "PNG",
        ["png"],
        dataUrl,
      );
    } catch (err) {
      setLoadError(t("画像の書き出しに失敗しました: {error}", { error: String(err) }));
    } finally {
      setBusy(null);
    }
  }, [assets]);

  /**
   * Imports PDF pages as note pages.
   *
   * Each PDF page becomes a background image on its own note page, so the user
   * can annotate on top of it with every normal tool. Keeping the PDF as a live
   * embedded object instead would mean re-rendering it on every repaint for no
   * benefit the user can see.
   */
  const importPdf = useCallback(async () => {
    const state = useEditorStore.getState();
    const { session: s, doc: d, noteId: nid } = state;
    if (!s || !d || !nid) return;

    setBusy(t("PDF を読み込んでいます…"));
    try {
      const dataUrl = await api.pickAndReadFile("PDF", ["pdf"]);
      if (!dataUrl) return;
      const info = await readPdfInfo(dataUrl);
      const numbers = parsePageRange("", info.pageCount);
      const paperWidth = d.pages[state.pageIndex]?.paperWidth ?? 1240;

      const images = await renderPdfPages(dataUrl, paperWidth, numbers, (done, total) =>
        setBusy(t("PDF を読み込んでいます… {done} / {total}", { done, total })),
      );

      const insertAt = state.pageIndex + 1;
      s.beginEdit("PDF を取り込み");
      for (const [i, image] of images.entries()) {
        const ticket = newTicket();
        await api.assetPut(nid, ticket, image.dataUrl);

        const newPage = createPage("blank");
        newPage.paperWidth = paperWidth;
        newPage.paperHeight = (image.height / image.width) * paperWidth;

        const bg = createBgImageUnit(newPage.paperWidth, newPage.paperHeight, ticket);
        newPage.layers[0].units.push(bg);

        s.record({ kind: "page.add", index: insertAt + i, page: newPage });
      }
      s.endEdit();

      state.setPageIndex(insertAt);
    } catch (err) {
      setLoadError(t("PDF の取り込みに失敗しました: {error}", { error: String(err) }));
    } finally {
      setBusy(null);
    }
  }, []);

  const page = doc?.pages[pageIndex];

  if (loadError) {
    return (
      <div className="app">
        <header className="topbar">
          <BackButton onClick={() => navigate("/")} />
        </header>
        <div className="loading">
          <div className="notice notice--error">
            <Icon name="error" size={20} />
            <span>{t("ノートを開けませんでした: {error}", { error: loadError })}</span>
          </div>
        </div>
      </div>
    );
  }

  if (!doc) {
    return (
      <div className="app">
        <header className="topbar">
          <span className="topbar__title">{t("読み込み中…")}</span>
        </header>
        <div className="loading">{t("読み込み中…")}</div>
      </div>
    );
  }

  return (
    <div className="app">
      <header className="topbar">
        <BackButton onClick={() => navigate("/")} />
        <span className="topbar__title">{doc.meta.title}</span>
        <span className="save-chip" data-state={saveState}>
          <Icon name={saveIcon(saveState)} size={16} />
          {t(saveLabel(saveState))}
        </span>
        {busy && (
          <span className="save-chip">
            <Icon name="pending" size={16} />
            {busy}
          </span>
        )}
        {classNote && (
          <>
            <span
              className="save-chip"
              data-state={watching ? "saved" : "dirty"}
              title={watching ? t("教室につながっています") : t("教室につながっていません")}
            >
              <Icon name="school" size={16} />
              {t("教室")}
            </span>
            <button
              type="button"
              className="icon-btn"
              onClick={() => void resync()}
              disabled={resyncing}
              title={t("同期し直す")}
            >
              <Icon name="refresh" />
              <span className="sr-only">{t("同期し直す")}</span>
            </button>
          </>
        )}
        {classSync && (
          <button
            type="button"
            className="save-chip"
            data-state={classSync.kind === "failed" ? "error" : "saved"}
            // The reason matters more than the fact: "could not send" alone
            // leaves nothing to act on, and the server's own words usually
            // name the step that failed.
            onClick={() => classSync.kind === "failed" && setShowClassError(true)}
          >
            <Icon name={classSync.kind === "failed" ? "error" : "school"} size={16} />
            {classSync.kind === "failed"
              ? t("教室に送れませんでした")
              : t("教室に送信: {count}", { count: classSync.strokes })}
          </button>
        )}
        <div className="topbar__spacer" />

        <Menu
          label={t("ファイル")}
          icon="description"
          items={[
            {
              id: "import-pdf",
              label: t("PDF を取り込む…"),
              icon: "file_open",
              onSelect: () => void importPdf(),
            },
            {
              id: "export-pdf",
              label: t("PDF で書き出す…"),
              icon: "picture_as_pdf",
              separatorBefore: true,
              onSelect: () => void exportPdf(),
            },
            {
              id: "export-png",
              label: t("このページを画像で書き出す…"),
              icon: "image",
              onSelect: () => void exportPng(),
            },
            {
              id: "save",
              label: t("保存"),
              icon: "save",
              shortcut: "⌘S",
              separatorBefore: true,
              onSelect: () => void save(),
            },
          ]}
        />

        <button
          type="button"
          className="icon-btn"
          onClick={() => setShowPages((v) => !v)}
          aria-pressed={showPages}
          title={t("ページ一覧")}
        >
          <Icon name="note_stack" />
          <span className="sr-only">{t("ページ一覧")}</span>
        </button>

        <button
          type="button"
          className="icon-btn"
          onClick={() => {
            controllerRef.current?.cancelGesture();
            undo();
          }}
          disabled={!canUndo}
          title={`${t("元に戻す")} (Cmd+Z)`}
        >
          <Icon name="undo" />
          <span className="sr-only">{t("元に戻す")}</span>
        </button>
        <button
          type="button"
          className="icon-btn"
          onClick={() => {
            controllerRef.current?.cancelGesture();
            redo();
          }}
          disabled={!canRedo}
          title={`${t("やり直す")} (Cmd+Shift+Z)`}
        >
          <Icon name="redo" />
          <span className="sr-only">{t("やり直す")}</span>
        </button>
      </header>

      {showClassError && classSync?.kind === "failed" && (
        <div className="notice notice--error" style={{ margin: "var(--space-3)" }}>
          <Icon name="error" size={20} />
          <span>{classSync.message}</span>
          <button type="button" className="btn btn--text" onClick={() => void resync()}>
            {t("同期し直す")}
          </button>
          <button
            type="button"
            className="btn btn--text"
            onClick={() => setShowClassError(false)}
          >
            {t("閉じる")}
          </button>
        </div>
      )}

      <div className="editor">
        <Toolbar />
        {showPages && (
          <PageStrip
            doc={doc}
            pageIndex={pageIndex}
            assets={assets}
            onSelect={setPageIndex}
            onReorder={reorderPage}
            onDuplicate={duplicatePage}
            onDelete={deletePage}
            onAdd={addPage}
          />
        )}
        <CanvasHost
          controllerRef={controllerRef}
          assets={assets}
          onPlace={handlePlace}
          onPlaceFrame={handlePlaceFrame}
          onEditText={beginTextEdit}
          onViewportChange={handleViewportChange}
        />
        <Inspector />
      </div>

      {editingUnitId && page && session && (
        <TextEditOverlay
          unitId={editingUnitId}
          page={page}
          session={session}
          viewport={overlayViewport}
          onClose={() => {
            setEditingUnitId(null);
            controllerRef.current?.invalidateAll();
          }}
        />
      )}

      <footer className="statusbar">
        <button
          type="button"
          className="icon-btn icon-btn--sm"
          onClick={() => setPageIndex(pageIndex - 1)}
          disabled={pageIndex === 0}
          title={t("前のページ")}
        >
          <Icon name="keyboard_arrow_left" size={20} />
          <span className="sr-only">{t("前のページ")}</span>
        </button>
        <span className="statusbar__pages">
          {pageIndex + 1} / {doc.pages.length}
        </span>
        <button
          type="button"
          className="icon-btn icon-btn--sm"
          onClick={() => setPageIndex(pageIndex + 1)}
          disabled={pageIndex >= doc.pages.length - 1}
          title={t("次のページ")}
        >
          <Icon name="keyboard_arrow_right" size={20} />
          <span className="sr-only">{t("次のページ")}</span>
        </button>
        <button
          type="button"
          className="icon-btn icon-btn--sm"
          onClick={addPage}
          title={t("ページを追加")}
        >
          <Icon name="add" size={20} />
          <span className="sr-only">{t("ページを追加")}</span>
        </button>

        <div className="statusbar__spacer" />

        <button
          type="button"
          className="icon-btn icon-btn--sm"
          onClick={() => controllerRef.current?.zoomBy(1 / 1.25)}
          title={t("縮小")}
        >
          <Icon name="zoom_out" size={20} />
          <span className="sr-only">{t("縮小")}</span>
        </button>
        <button
          type="button"
          className="icon-btn icon-btn--sm"
          onClick={() => controllerRef.current?.fitPage()}
          title={t("全体表示")}
        >
          <Icon name="fit_screen" size={20} />
          <span className="sr-only">{t("全体表示")}</span>
        </button>
        <button
          type="button"
          className="icon-btn icon-btn--sm"
          onClick={() => controllerRef.current?.zoomBy(1.25)}
          title={t("拡大")}
        >
          <Icon name="zoom_in" size={20} />
          <span className="sr-only">{t("拡大")}</span>
        </button>
      </footer>
    </div>
  );
}

/** Material's top-app-bar leading navigation icon. */
function BackButton({ onClick }: { onClick: () => void }) {
  const { t } = useTranslation();
  return (
    <button type="button" className="icon-btn" onClick={onClick} title={t("ノート一覧に戻る")}>
      <Icon name="arrow_back" />
      <span className="sr-only">{t("ノート一覧に戻る")}</span>
    </button>
  );
}

function saveIcon(state: string): IconName {
  switch (state) {
    case "saving":
      return "pending";
    case "dirty":
      return "edit";
    case "error":
      return "error";
    default:
      return "check_circle";
  }
}

function saveLabel(state: string): string {
  switch (state) {
    case "saving":
      return "保存中…";
    case "dirty":
      return "未保存";
    case "error":
      return "保存に失敗";
    default:
      return "保存済み";
  }
}

function imageSize(dataUrl: string): Promise<{ width: number; height: number }> {
  return new Promise((resolve) => {
    const img = new Image();
    img.onload = () => resolve({ width: img.naturalWidth, height: img.naturalHeight });
    img.onerror = () => resolve({ width: 320, height: 240 });
    img.src = dataUrl;
  });
}
