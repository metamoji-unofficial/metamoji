/**
 * The note library.
 *
 * Stands in for the original's `MainActivity` / Cabinet hub (docs/02 §1). The
 * cloud half of Cabinet — drives, team folders, sync status — belongs to phase
 * B and would be dead chrome here, so what is left is the part that works
 * entirely locally: folders, tags, search and the note grid.
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router";
import { open } from "@tauri-apps/plugin-dialog";

import { Icon } from "../components/Icon";
import { ImportReportDialog } from "../components/ImportReportDialog";
import { useTranslation } from "../i18n/useTranslation";
import { Menu } from "../components/Menu";
import { AccountButton } from "../components/AccountButton";
import * as api from "../ipc/api";
import type {
  ClassBoxListing,
  Folder,
  ImportReport,
  ListQuery,
  NoteSort,
  NoteSummary,
  Tag,
} from "../ipc/api";
import { rasterisePdfUnits } from "../io/atdocPdf";
import { toGeneric } from "../model/converter";
import type { GenericTree } from "../model/generic";
import { createDocument } from "../model/factory";
import { newId, newNoteId } from "../model/ids";
import { markSessionClosed, readInterruptedSession, type OpenSession } from "../store/sessionStore";
import { useAuthStore } from "../store/authStore";
import { useClassroomStore } from "../store/classroomStore";

const TAG_COLORS = ["#32a5ff", "#d93025", "#188038", "#f29900", "#9334e6", "#e8710a"];

type View =
  | { kind: "all" }
  | { kind: "folder"; id: string }
  | { kind: "tag"; id: string }
  | { kind: "trash" }
  /** A class box: its notes live on the server, not in the local catalog. */
  | { kind: "classbox"; id: string; name: string };

export function LibraryScreen() {
  const navigate = useNavigate();
  const { t } = useTranslation();

  const [notes, setNotes] = useState<NoteSummary[]>([]);
  const [folders, setFolders] = useState<Folder[]>([]);
  const [tags, setTags] = useState<Tag[]>([]);
  const [view, setView] = useState<View>({ kind: "all" });
  const [search, setSearch] = useState("");
  const [sort, setSort] = useState<NoteSort>("updated");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [importing, setImporting] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [report, setReport] = useState<ImportReport | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [interrupted, setInterrupted] = useState<OpenSession | null>(() =>
    readInterruptedSession(),
  );
  // Selection is a set of ids rather than a flag on the summaries because the
  // summaries are replaced wholesale on every refresh; a flag would be lost
  // each time, and the whole point of the selection is to outlive the actions
  // taken on it.
  const [selected, setSelected] = useState<ReadonlySet<string>>(EMPTY_SELECTION);
  /** The last note clicked, so shift-click has a run to extend from. */
  const [anchor, setAnchor] = useState<string | null>(null);

  // Class boxes sit in the sidebar with folders and tags because that is what
  // they are to the user: another place their notes live. That they come from
  // a different server over a different protocol is not their problem.
  const session = useAuthStore((s) => s.session);
  const myBoxes = useClassroomStore((s) => s.myBoxes);
  const boxListing = useClassroomStore((s) => s.listing);
  const openingBox = useClassroomStore((s) => s.openingBox);
  const listingError = useClassroomStore((s) => s.listingError);
  const boxPath = useClassroomStore((s) => s.boxPath);
  const loadingBoxes = useClassroomStore((s) => s.loadingBoxes);
  const boxesError = useClassroomStore((s) => s.boxesError);

  useEffect(() => {
    if (session) void useClassroomStore.getState().loadMyBoxes();
  }, [session]);

  const query = useMemo<ListQuery>(
    () => ({
      trashed: view.kind === "trash",
      folderId: view.kind === "folder" ? view.id : null,
      tagId: view.kind === "tag" ? view.id : null,
      text: search,
      sort,
    }),
    [view, search, sort],
  );

  const refresh = useCallback(async () => {
    try {
      const [nextNotes, nextFolders, nextTags] = await Promise.all([
        api.libraryList(query),
        api.folderList(),
        api.tagList(),
      ]);
      setNotes(nextNotes);
      setFolders(nextFolders);
      setTags(nextTags);
      // A note that has just been trashed, deleted or filtered out is gone
      // from the grid, and a selection counting notes nobody can see is a
      // count nobody can act on.
      setSelected((prev) => {
        if (prev.size === 0) return prev;
        const live = new Set(nextNotes.map((n) => n.id).filter((id) => prev.has(id)));
        return live.size === prev.size ? prev : live;
      });
      setError(null);
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, [query]);

  useEffect(() => {
    // Debounced so typing in the search box does not fire a query per keystroke.
    const timer = setTimeout(() => void refresh(), search ? 200 : 0);
    return () => clearTimeout(timer);
  }, [refresh, search]);

  // Moving to another folder, tag or the trash ends the selection: the notes
  // it held are no longer on screen, and carrying it across would leave the
  // action bar offering to delete notes the user cannot see.
  useEffect(() => {
    setSelected(EMPTY_SELECTION);
    setAnchor(null);
  }, [view, search]);

  useEffect(() => {
    if (selected.size === 0) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") clearSelection();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selected.size]);

  // -- notes ---------------------------------------------------------------

  const createNote = async () => {
    try {
      const doc = createDocument(t("ノート {n}", { n: notes.length + 1 }));
      const summary = await api.libraryCreate(toGeneric(doc), doc.meta.title);
      if (view.kind === "folder") await api.librarySetFolder(summary.id, view.id);
      navigate(`/note/${summary.id}`);
    } catch (err) {
      setError(String(err));
    }
  };

  const importAtdoc = async () => {
    setImporting(true);
    setError(null);
    try {
      const noteId = newNoteId();
      const result = await api.pickAndImportAtdoc(noteId);
      if (!result) return;
      // The document's own name beats the file's: a note copied out of a class
      // box is called something like `doc_1730.atdoc` on disk.
      const title = result.title ?? result.fileName ?? t("取り込んだノート");
      await renderBackgrounds(noteId, result.tree);
      await api.libraryCreate(result.tree, title);
      await refresh();
      setReport(result.report);
    } catch (err) {
      setError(String(err));
    } finally {
      setImporting(false);
      setBusy(null);
    }
  };

  const act = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
      await refresh();
    } catch (err) {
      setError(String(err));
    }
  };

  const duplicate = (note: NoteSummary) =>
    act(() => api.libraryDuplicate(note.id, newNoteId(), t("{title} のコピー", { title: note.title })));

  const rename = (id: string, title: string) => {
    setRenaming(null);
    if (title.trim()) void act(() => api.libraryRename(id, title.trim()));
  };

  // -- selection ------------------------------------------------------------

  const clearSelection = () => {
    setSelected(EMPTY_SELECTION);
    setAnchor(null);
  };

  const toggleSelect = (id: string, extend: boolean) => {
    setSelected((prev) => selectionAfterClick(notes.map((n) => n.id), prev, anchor, id, extend));
    setAnchor(id);
  };

  const selectAll = () => {
    setSelected(new Set(notes.map((n) => n.id)));
    setAnchor(null);
  };

  /**
   * Runs `fn` over a set of notes, one at a time.
   *
   * Sequential rather than `Promise.all`: the backend is a single catalog and
   * forty concurrent writes to it buy nothing, while one-at-a-time gives a
   * count to show and lets a note that fails leave the rest untouched. One
   * unreadable note out of forty should not abandon the other thirty-nine, so
   * failures are counted and reported at the end instead of thrown.
   */
  const applyToNotes = async (ids: string[], fn: (id: string) => Promise<unknown>) => {
    if (ids.length === 0) return;
    setError(null);
    let failures = 0;
    let firstError = "";
    for (const [index, id] of ids.entries()) {
      setBusy(t("{done} / {total} 件を処理しています…", { done: index + 1, total: ids.length }));
      try {
        await fn(id);
      } catch (err) {
        failures += 1;
        firstError ||= String(err);
      }
    }
    setBusy(null);
    clearSelection();
    await refresh();
    if (failures > 0) {
      setError(t("{count} 件を処理できませんでした: {error}", { count: failures, error: firstError }));
    }
  };

  const selectedIds = () => notes.filter((note) => selected.has(note.id)).map((note) => note.id);

  const trashSelected = () => void applyToNotes(selectedIds(), (id) => api.librarySetTrashed(id, true));

  const restoreSelected = () =>
    void applyToNotes(selectedIds(), (id) => api.librarySetTrashed(id, false));

  const deleteSelected = () => {
    const ids = selectedIds();
    if (!window.confirm(t("選択した {count} 件を完全に削除しますか?", { count: ids.length }))) return;
    void applyToNotes(ids, (id) => api.libraryDelete(id));
  };

  /**
   * Empties the trash — or as much of it as is on screen. With a search in the
   * box the grid is a subset, and deleting the notes it does not show would be
   * deleting things the user was not looking at.
   */
  const emptyTrash = () => {
    const ids = notes.map((note) => note.id);
    if (!window.confirm(t("ゴミ箱の {count} 件を完全に削除しますか?", { count: ids.length }))) return;
    void applyToNotes(ids, (id) => api.libraryDelete(id));
  };

  // -- folders and tags -----------------------------------------------------

  const addFolder = () => {
    const name = window.prompt(t("フォルダ名"));
    if (name?.trim()) void act(() => api.folderCreate(newId("folder"), name.trim(), null));
  };

  const addTag = () => {
    const name = window.prompt(t("タグ名"));
    if (!name?.trim()) return;
    const color = TAG_COLORS[tags.length % TAG_COLORS.length];
    void act(() => api.tagCreate(newId("tag"), name.trim(), color));
  };

  /**
   * Turns the note's embedded PDF into page images.
   *
   * Shared by both ways in, because a class-box note and a `.atdoc` on disk are
   * the same document in the same format — one arrived over the network.
   */
  const renderBackgrounds = async (noteId: string, tree: GenericTree) => {
    await rasterisePdfUnits(noteId, tree, (done, total) =>
      setBusy(t("PDF を読み込んでいます… {done} / {total}", { done, total })),
    );
  };

  /**
   * Downloads every note's background under a class-box folder — PDFs and
   * images, no handwriting — into a folder the user picks.
   *
   * Shares `busy`/`error` with the rest of the screen rather than its own
   * progress UI: the download can take a while, and this is the banner the
   * user is already watching during an import.
   */
  const downloadClassboxFolder = async (path: string) => {
    if (view.kind !== "classbox") return;
    const dir = await open({ directory: true, multiple: false });
    if (typeof dir !== "string") return;

    setError(null);
    setBusy(t("ダウンロードしています…"));
    const unlisten = await api.onClassboxDownloadProgress((p) => {
      setBusy(
        p.title
          ? t("ダウンロードしています… {done} / {total}({title})", {
              done: p.done,
              total: p.total,
              title: p.title,
            })
          : t("ダウンロードしています… {done} / {total}", { done: p.done, total: p.total }),
      );
    });
    try {
      const result = await api.classboxDownloadFolder(view.id, path, dir);
      window.alert(
        t("{saved} 件を保存しました(背景なし: {skipped} 件、失敗: {failed} 件)", {
          saved: result.saved.length,
          skipped: result.skipped.length,
          failed: result.failed.length,
        }),
      );
      if (result.failed.length > 0) {
        setError(result.failed.map((f) => `${f.title ?? f.documentId}: ${f.error}`).join("\n"));
      }
    } catch (err) {
      setError(String(err));
    } finally {
      unlisten();
      setBusy(null);
    }
  };

  const openClassNote = async (documentId: string, title: string | null) => {
    if (view.kind !== "classbox") return;
    setError(null);
    setBusy(t("ノートを取り込んでいます…"));
    try {
      const noteId = newNoteId();
      // The download is the teacher's copy; the room holds everyone's
      // writing, and `classboxOpenNote` waits for both.
      setBusy(t("教室から書き込みを取り込んでいます…"));
      const result = await api.classboxOpenNote(view.id, documentId, noteId);
      await renderBackgrounds(noteId, result.tree);
      const summary = await api.libraryCreate(
        result.tree,
        // The class box's own title is the fresher of the two: renaming a note
        // there does not rewrite the document inside it.
        title ?? result.title ?? t("クラスボックスのノート"),
      );
      // Remembered so what the user writes on this copy can go back to the
      // class it came from.
      await api.classboxLink(summary.id, view.id, documentId, result.roomId);
      // The note is in the library either way. But if the classroom could not
      // be reached, everything written on it is missing, and opening it
      // straight away would look like the note is simply blank.
      if (result.room?.error) {
        await refresh();
        setError(
          t("ノートは取り込みましたが、教室につながらなかったので書き込みは入っていません: {error}", {
            error: result.room.error,
          }),
        );
        return;
      }
      navigate(`/note/${summary.id}`);
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(null);
    }
  };

  const viewTitle =
    view.kind === "trash"
      ? t("ゴミ箱")
      : view.kind === "classbox"
        ? view.name
        : view.kind === "folder"
          ? (folders.find((f) => f.id === view.id)?.name ?? t("フォルダ"))
          : view.kind === "tag"
            ? (tags.find((tag) => tag.id === view.id)?.name ?? t("タグ"))
            : t("すべてのノート");

  return (
    <div className="app">
      <header className="topbar">
        <span className="topbar__title">MetaMoJi</span>
        <div className="topbar__spacer" />
        <div className="search-field">
          <Icon name="search" size={20} />
          <input
            className="search-box"
            type="search"
            placeholder={t("ノートを検索")}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        </div>
        <Menu
          label={t("並び順")}
          icon="sort"
          items={[
            {
              id: "updated",
              label: t("更新日時"),
              checked: sort === "updated",
              onSelect: () => setSort("updated"),
            },
            {
              id: "created",
              label: t("作成日時"),
              checked: sort === "created",
              onSelect: () => setSort("created"),
            },
            {
              id: "title",
              label: t("名前"),
              checked: sort === "title",
              onSelect: () => setSort("title"),
            },
          ]}
        />
        <button
          type="button"
          className="btn btn--text"
          onClick={importAtdoc}
          disabled={importing}
        >
          <Icon name="file_open" size={18} />
          {importing ? t("取り込み中…") : t("取り込む")}
        </button>
        <AccountButton />
        <button
          type="button"
          className="icon-btn"
          onClick={() => navigate("/classroom")}
          title={t("教室")}
        >
          <Icon name="school" />
          <span className="sr-only">{t("教室")}</span>
        </button>
        <button
          type="button"
          className="icon-btn"
          onClick={() => navigate("/settings")}
          title={t("設定")}
        >
          <Icon name="settings" />
          <span className="sr-only">{t("設定")}</span>
        </button>
      </header>

      <div className="library-body">
        <nav className="library-sidebar" aria-label={t("ライブラリ")}>
          <button
            type="button"
            className="sidebar-item"
            aria-current={view.kind === "all"}
            onClick={() => setView({ kind: "all" })}
          >
            <Icon name="note_stack" size={20} />
            <span className="sidebar-item__label">{t("すべてのノート")}</span>
          </button>

          <div className="sidebar-heading">
            <span>{t("フォルダ")}</span>
            <button
              type="button"
              className="icon-btn icon-btn--sm"
              onClick={addFolder}
              title={t("フォルダを追加")}
            >
              <Icon name="create_new_folder" size={20} />
              <span className="sr-only">{t("フォルダを追加")}</span>
            </button>
          </div>
          {folders.length === 0 && <p className="sidebar-empty">{t("まだありません")}</p>}
          {folders.map((folder) => (
            <div key={folder.id} className="sidebar-row">
              <button
                type="button"
                className="sidebar-item"
                aria-current={view.kind === "folder" && view.id === folder.id}
                onClick={() => setView({ kind: "folder", id: folder.id })}
                onDragOver={(e) => e.preventDefault()}
                onDrop={(e) => {
                  e.preventDefault();
                  const noteId = e.dataTransfer.getData("text/note-id");
                  if (noteId) void act(() => api.librarySetFolder(noteId, folder.id));
                }}
              >
                <Icon name="folder" size={20} />
                <span className="sidebar-item__label">{folder.name}</span>
                <span className="sidebar-count">{folder.noteCount}</span>
              </button>
              <button
                type="button"
                className="icon-btn icon-btn--sm sidebar-action"
                title={t("フォルダを削除")}
                onClick={() => void act(() => api.folderDelete(folder.id))}
              >
                <Icon name="delete" size={20} />
                <span className="sr-only">{t("フォルダを削除")}</span>
              </button>
            </div>
          ))}

          {session && (
            <>
              <div className="sidebar-heading">
                <span>{t("クラス")}</span>
                <button
                  type="button"
                  className="icon-btn icon-btn--sm"
                  onClick={() => navigate("/classroom")}
                  title={t("教室")}
                >
                  <Icon name="school" size={20} />
                  <span className="sr-only">{t("教室")}</span>
                </button>
              </div>
              {loadingBoxes ? (
                <p className="sidebar-empty">{t("読み込み中…")}</p>
              ) : boxesError ? (
                <>
                  {/*
                   * The message goes on screen, not only in a tooltip: it is
                   * the server's own wording and the only thing that says what
                   * went wrong.
                   */}
                  <p className="sidebar-empty" title={boxesError}>
                    {boxesError}
                  </p>
                  <button
                    type="button"
                    className="sidebar-item"
                    onClick={() => void useClassroomStore.getState().loadMyBoxes()}
                  >
                    <Icon name="refresh" size={20} />
                    <span className="sidebar-item__label">{t("再読み込み")}</span>
                  </button>
                </>
              ) : myBoxes === null || myBoxes.length === 0 ? (
                <p className="sidebar-empty">{t("まだありません")}</p>
              ) : (
                myBoxes.map((box) => (
                  <div key={box.driveId} className="sidebar-row">
                    <button
                      type="button"
                      className="sidebar-item"
                      aria-current={view.kind === "classbox" && view.id === box.driveId}
                      onClick={() => {
                        const name = box.name ?? box.driveId;
                        setView({ kind: "classbox", id: box.driveId, name });
                        void useClassroomStore.getState().selectBox(box);
                      }}
                    >
                      <Icon name="school" size={20} />
                      <span className="sidebar-item__label">
                        {box.name ?? box.driveId}
                      </span>
                    </button>
                  </div>
                ))
              )}
            </>
          )}

          <div className="sidebar-heading">
            <span>{t("タグ")}</span>
            <button
              type="button"
              className="icon-btn icon-btn--sm"
              onClick={addTag}
              title={t("タグを追加")}
            >
              <Icon name="new_label" size={20} />
              <span className="sr-only">{t("タグを追加")}</span>
            </button>
          </div>
          {tags.length === 0 && <p className="sidebar-empty">{t("まだありません")}</p>}
          {tags.map((tag) => (
            <div key={tag.id} className="sidebar-row">
              <button
                type="button"
                className="sidebar-item"
                aria-current={view.kind === "tag" && view.id === tag.id}
                onClick={() => setView({ kind: "tag", id: tag.id })}
              >
                <span className="tag-dot" style={{ background: tag.color }} />
                <span className="sidebar-item__label">{tag.name}</span>
              </button>
              <button
                type="button"
                className="icon-btn icon-btn--sm sidebar-action"
                title={t("タグを削除")}
                onClick={() => void act(() => api.tagDelete(tag.id))}
              >
                <Icon name="delete" size={20} />
                <span className="sr-only">{t("タグを削除")}</span>
              </button>
            </div>
          ))}

          <div className="sidebar-heading">
            <span>{t("その他")}</span>
          </div>
          <button
            type="button"
            className="sidebar-item"
            aria-current={view.kind === "trash"}
            onClick={() => setView({ kind: "trash" })}
          >
            <Icon name="delete" size={20} />
            <span className="sidebar-item__label">{t("ゴミ箱")}</span>
          </button>
        </nav>

        <main className="library">
          {/*
            * Material's contextual top app bar: while notes are selected the
            * title is replaced by what is selected and what can be done to it,
            * rather than adding a second bar and pushing the grid down.
            */}
          {selected.size > 0 ? (
            <div className="library__header library__header--select">
              <button
                type="button"
                className="icon-btn"
                onClick={clearSelection}
                title={t("選択を解除")}
              >
                <Icon name="close" />
                <span className="sr-only">{t("選択を解除")}</span>
              </button>
              <h1>{t("{count} 件を選択中", { count: selected.size })}</h1>
              <div className="library__actions">
                <button
                  type="button"
                  className="btn btn--text"
                  onClick={selectAll}
                  disabled={selected.size === notes.length}
                >
                  <Icon name="check" size={18} />
                  {t("すべて選択")}
                </button>
                {view.kind === "trash" ? (
                  <>
                    <button type="button" className="btn" onClick={restoreSelected}>
                      <Icon name="restore_from_trash" size={18} />
                      {t("元に戻す操作")}
                    </button>
                    <button
                      type="button"
                      className="btn btn--primary btn--danger"
                      onClick={deleteSelected}
                    >
                      <Icon name="delete_forever" size={18} />
                      {t("完全に削除")}
                    </button>
                  </>
                ) : (
                  <button
                    type="button"
                    className="btn btn--primary btn--danger"
                    onClick={trashSelected}
                  >
                    <Icon name="delete" size={18} />
                    {t("ゴミ箱へ")}
                  </button>
                )}
              </div>
            </div>
          ) : (
            <div className="library__header">
              <h1>{viewTitle}</h1>
              <span className="library__count">
                {t("{count} 件", {
                  count:
                    view.kind === "classbox"
                      ? (boxListing?.documents.length ?? 0)
                      : notes.length,
                })}
              </span>
              {view.kind === "trash" && notes.length > 0 && (
                <div className="library__actions">
                  <button type="button" className="btn btn--danger" onClick={emptyTrash}>
                    <Icon name="delete_forever" size={18} />
                    {t("ゴミ箱を空にする")}
                  </button>
                </div>
              )}
            </div>
          )}

          {interrupted && (
            <div className="notice resume" style={{ marginBottom: "var(--space-4)" }}>
              <Icon name="info" size={20} />
              <span style={{ flex: 1 }}>
                {t("前回は「{title}」を開いたままでした。", { title: interrupted.title })}
              </span>
              <span style={{ display: "flex", gap: "var(--space-2)" }}>
                <button
                  type="button"
                  className="btn btn--primary"
                  onClick={() => navigate(`/note/${interrupted.noteId}`)}
                >
                  {t("開く")}
                </button>
                <button
                  type="button"
                  className="btn btn--text"
                  onClick={() => {
                    markSessionClosed();
                    setInterrupted(null);
                  }}
                >
                  {t("閉じる")}
                </button>
              </span>
            </div>
          )}

          {error && (
            <div className="notice notice--error" style={{ marginBottom: "var(--space-4)" }}>
              <Icon name="error" size={20} />
              <span>{error}</span>
            </div>
          )}

          {busy && (
            <div className="notice" style={{ marginBottom: "var(--space-4)" }}>
              <Icon name="pending" size={20} />
              <span>{busy}</span>
            </div>
          )}

          {view.kind === "classbox" ? (
            <ClassBoxGrid
              listing={boxListing}
              loading={openingBox}
              error={listingError}
              path={boxPath}
              onNavigate={(next) => useClassroomStore.getState().setBoxPath(next)}
              onRetry={() => void useClassroomStore.getState().openBox()}
              onOpen={(id, title) => void openClassNote(id, title)}
              onDownload={(path) => void downloadClassboxFolder(path)}
            />
          ) : loading ? (
            <div className="library__empty">{t("読み込み中…")}</div>
          ) : notes.length === 0 ? (
            <div className="library__empty">
              {search ? (
                <>
                  <Icon name="search" size={48} />
                  <p>{t("「{search}」に一致するノートはありません。", { search })}</p>
                </>
              ) : view.kind === "trash" ? (
                <>
                  <Icon name="delete" size={48} />
                  <p>{t("ゴミ箱は空です。")}</p>
                </>
              ) : (
                <>
                  <Icon name="note_stack" size={48} />
                  <p>{t("まだノートがありません。")}</p>
                  <button type="button" className="btn btn--primary" onClick={createNote}>
                    <Icon name="add" size={18} />
                    {t("最初のノートを作成")}
                  </button>
                </>
              )}
            </div>
          ) : (
            <div
              className={`library__grid${selected.size > 0 ? " library__grid--selecting" : ""}`}
            >
              {notes.map((note) => (
                <NoteCard
                  key={note.id}
                  note={note}
                  tags={tags}
                  renaming={renaming === note.id}
                  inTrash={view.kind === "trash"}
                  selected={selected.has(note.id)}
                  selecting={selected.size > 0}
                  onToggleSelect={(extend) => toggleSelect(note.id, extend)}
                  onOpen={() => navigate(`/note/${note.id}`)}
                  onStartRename={() => setRenaming(note.id)}
                  onRename={(title) => rename(note.id, title)}
                  onDuplicate={() => void duplicate(note)}
                  onTrash={() => void act(() => api.librarySetTrashed(note.id, true))}
                  onRestore={() => void act(() => api.librarySetTrashed(note.id, false))}
                  onDelete={() => {
                    if (window.confirm(t("「{title}」を完全に削除しますか?", { title: note.title }))) {
                      void act(() => api.libraryDelete(note.id));
                    }
                  }}
                  onToggleTag={(tagId, on) =>
                    void act(() => api.tagSetOnDocument(note.id, tagId, on))
                  }
                  onRemoveFromFolder={() => void act(() => api.librarySetFolder(note.id, null))}
                />
              ))}
            </div>
          )}
        </main>
      </div>

      {/* Material's FAB: the library has exactly one primary action. */}
      <button type="button" className="fab" onClick={createNote} title={t("新規ノート")}>
        <Icon name="add" />
        {t("新規ノート")}
      </button>

      {report && <ImportReportDialog report={report} onClose={() => setReport(null)} />}
    </div>
  );
}

interface CardProps {
  note: NoteSummary;
  tags: Tag[];
  renaming: boolean;
  inTrash: boolean;
  selected: boolean;
  /** Something is selected — so a click on any card extends that, not opens. */
  selecting: boolean;
  onToggleSelect: (extend: boolean) => void;
  onOpen: () => void;
  onStartRename: () => void;
  onRename: (title: string) => void;
  onDuplicate: () => void;
  onTrash: () => void;
  onRestore: () => void;
  onDelete: () => void;
  onToggleTag: (tagId: string, on: boolean) => void;
  onRemoveFromFolder: () => void;
}

function NoteCard({
  note,
  tags,
  renaming,
  inTrash,
  selected,
  selecting,
  onToggleSelect,
  onOpen,
  onStartRename,
  onRename,
  onDuplicate,
  onTrash,
  onRestore,
  onDelete,
  onToggleTag,
  onRemoveFromFolder,
}: CardProps) {
  const { t } = useTranslation();
  const noteTagIds = new Set(note.tags.map((t) => t.id));

  return (
    <div
      className={`note-card${selected ? " note-card--selected" : ""}`}
      draggable={!inTrash && !selecting}
      onDragStart={(e) => e.dataTransfer.setData("text/note-id", note.id)}
    >
      <button
        type="button"
        className="icon-btn icon-btn--sm note-card__select"
        aria-pressed={selected}
        title={selected ? t("選択を解除") : t("選択")}
        onClick={(e) => onToggleSelect(e.shiftKey)}
      >
        <Icon name="check" size={20} />
        <span className="sr-only">{selected ? t("選択を解除") : t("選択")}</span>
      </button>

      <button
        type="button"
        className="note-card__open"
        /*
         * Once a selection is running the grid is a picker, not a launcher:
         * opening a note here would throw away the selection the user is
         * halfway through building.
         */
        onClick={(e) => (selecting ? onToggleSelect(e.shiftKey) : onOpen())}
        disabled={renaming}
      >
        {note.thumbnail ? (
          <img className="note-card__thumb" src={note.thumbnail} alt="" draggable={false} />
        ) : (
          <div className="note-card__thumb" />
        )}
      </button>

      <div className="note-card__body">
        {renaming ? (
          <input
            className="note-card__rename"
            autoFocus
            defaultValue={note.title}
            onBlur={(e) => onRename(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") e.currentTarget.blur();
              if (e.key === "Escape") onRename(note.title);
            }}
          />
        ) : (
          <div className="note-card__title" title={note.title}>
            {note.title}
          </div>
        )}

        <div className="note-card__meta">
          {t("{count} ページ", { count: note.pageCount })} · {formatDate(note.updatedAt)}
        </div>

        {note.tags.length > 0 && (
          <div className="note-card__tags">
            {note.tags.map((tag) => (
              <span key={tag.id} className="chip">
                <span className="tag-dot" style={{ background: tag.color }} />
                {tag.name}
              </span>
            ))}
          </div>
        )}
      </div>

      <div className="note-card__footer">
        <Menu
          icon="more_vert"
          title={t("操作")}
          items={
            inTrash
              ? [
                  {
                    id: "restore",
                    label: t("元に戻す操作"),
                    icon: "restore_from_trash" as const,
                    onSelect: onRestore,
                  },
                  {
                    id: "delete",
                    label: t("完全に削除"),
                    icon: "delete_forever" as const,
                    danger: true,
                    separatorBefore: true,
                    onSelect: onDelete,
                  },
                ]
              : [
                  {
                    id: "rename",
                    label: t("名前を変更"),
                    icon: "edit" as const,
                    onSelect: onStartRename,
                  },
                  {
                    id: "duplicate",
                    label: t("複製"),
                    icon: "content_copy" as const,
                    onSelect: onDuplicate,
                  },
                  ...(note.folderId
                    ? [
                        {
                          id: "unfile",
                          label: t("フォルダから出す"),
                          icon: "folder" as const,
                          onSelect: onRemoveFromFolder,
                        },
                      ]
                    : []),
                  // A tag row is a toggle, so it gets a check rather than an
                  // icon — including when off, which is what reserves the slot.
                  ...tags.map((tag) => ({
                    id: `tag-${tag.id}`,
                    label: tag.name,
                    checked: noteTagIds.has(tag.id),
                    separatorBefore: tag.id === tags[0]?.id,
                    onSelect: () => onToggleTag(tag.id, !noteTagIds.has(tag.id)),
                  })),
                  {
                    id: "trash",
                    label: t("ゴミ箱へ"),
                    icon: "delete" as const,
                    danger: true,
                    separatorBefore: true,
                    onSelect: onTrash,
                  },
                ]
          }
        />
      </div>
    </div>
  );
}

/**
 * The notes inside a class box, one folder at a time.
 *
 * Separate from the local grid because almost nothing is shared: these come
 * from a different server, have no local catalog row, and open as a copy —
 * there is no rename, no trash, no drag to a folder. Reusing `NoteCard` would
 * mean disabling most of it.
 *
 * Folders here are *paths*, not ids (`SdMOFolder` is keyed by `absPath`), so
 * navigation is string manipulation and needs no lookup table.
 */
function ClassBoxGrid({
  listing,
  loading,
  error,
  path,
  onNavigate,
  onRetry,
  onOpen,
  onDownload,
}: {
  listing: ClassBoxListing | null;
  loading: boolean;
  error: string | null;
  path: string;
  onNavigate: (path: string) => void;
  onRetry: () => void;
  onOpen: (documentId: string, title: string | null) => void;
  /** Downloads every note's background under `path`, subfolders included. */
  onDownload: (path: string) => void;
}) {
  const { t } = useTranslation();

  if (loading) return <div className="library__empty">{t("読み込み中…")}</div>;

  if (!listing) {
    return (
      <div className="library__empty">
        <Icon name="school" size={48} />
        <p>{t("クラスを開けませんでした。")}</p>
        {/*
         * The server's own words, verbatim. The message names the step and the
         * request that failed; replacing it with a sentence of our own is what
         * left this impossible to diagnose from a screenshot.
         */}
        {error && (
          <div className="notice notice--error" style={{ textAlign: "left" }}>
            <Icon name="error" size={20} />
            <span>{error}</span>
          </div>
        )}
        <button type="button" className="btn" onClick={onRetry}>
          <Icon name="refresh" size={18} />
          {t("再読み込み")}
        </button>
      </div>
    );
  }

  const here = listing.folders.filter((f) => (f.parentPath ?? "/") === path);
  const notes = listing.documents.filter((d) => d.folderPath === path);
  const crumbs = breadcrumbs(path);

  if (here.length === 0 && notes.length === 0) {
    return (
      <div className="library__empty">
        {crumbs.length > 0 && <Breadcrumbs crumbs={crumbs} onNavigate={onNavigate} />}
        <Icon name="school" size={48} />
        <p>{path === "/" ? t("ノートはまだありません。") : t("このフォルダは空です。")}</p>
        {listing.unrecognised.length > 0 && (
          // An unreadable class box and an empty one mean very different
          // things; saying "empty" for both would send a teacher looking for
          // the wrong problem.
          <div className="notice notice--warning">
            <Icon name="warning" size={20} />
            <span>
              {t("中身を解釈できませんでした({count} 件のレコード、未知の項目: {types})。", {
                count: listing.recordCount,
                types: listing.unrecognised.join(", "),
              })}
            </span>
          </div>
        )}
      </div>
    );
  }

  return (
    <>
      <div className="box-toolbar">
        {crumbs.length > 0 && <Breadcrumbs crumbs={crumbs} onNavigate={onNavigate} />}
        <div className="library__actions">
          <button type="button" className="btn btn--text" onClick={() => onDownload(path)}>
            <Icon name="download" size={18} />
            {t("このフォルダ内をすべてダウンロード")}
          </button>
        </div>
      </div>

      {/*
       * Two grids, not one. A folder card is a single row of text and a note
       * card is a whole page thumbnail, so putting both in one auto-fill grid
       * makes every row as tall as its tallest member and leaves ragged gaps
       * under the folders. Separate sections also let the folders be denser,
       * which is what they want to be.
       */}
      {here.length > 0 && (
        <section className="box-section">
          <h2 className="box-section__title">{t("フォルダ")}</h2>
          <div className="folder-grid">
            {here.map((folder) => (
              <button
                key={folder.absPath}
                type="button"
                className="folder-card"
                onClick={() => onNavigate(folder.absPath)}
              >
                <Icon name="folder" size={24} />
                <span className="folder-card__name">{folder.name}</span>
              </button>
            ))}
          </div>
        </section>
      )}

      {notes.length > 0 && (
        <section className="box-section">
          {/* Only worth a heading when there is something above it. */}
          {here.length > 0 && <h2 className="box-section__title">{t("ノート")}</h2>}
          <div className="library__grid">
            {notes.map((doc) => (
              <div key={doc.documentId} className="note-card">
                <button
                  type="button"
                  className="note-card__open"
                  onClick={() => onOpen(doc.documentId, doc.title)}
                >
                  <div className="note-card__thumb" />
                </button>
                <div className="note-card__body">
                  <div className="note-card__title" title={doc.title ?? doc.documentId}>
                    {doc.title ?? doc.documentId}
                  </div>
                  <div className="note-card__meta">{t("端末に複製して開きます")}</div>
                </div>
              </div>
            ))}
          </div>
        </section>
      )}
    </>
  );
}

/** Shared empty set, so an empty selection is always the same object. */
const EMPTY_SELECTION: ReadonlySet<string> = new Set<string>();

/**
 * The selection a click on `id` leaves behind.
 *
 * A plain click toggles one note. Shift-click fills in the run from the last
 * note clicked, which is the only way to pick forty notes without forty
 * clicks. The anchor may have left the grid since it was set — trashed,
 * renamed out of the current search — and then there is no run to fill, so the
 * click falls back to a plain toggle rather than selecting nothing.
 */
export function selectionAfterClick(
  ids: readonly string[],
  selected: ReadonlySet<string>,
  anchor: string | null,
  id: string,
  extend: boolean,
): Set<string> {
  const next = new Set(selected);
  const from = anchor === null ? -1 : ids.indexOf(anchor);
  const to = ids.indexOf(id);

  if (extend && from >= 0 && to >= 0) {
    for (let i = Math.min(from, to); i <= Math.max(from, to); i += 1) next.add(ids[i]);
    return next;
  }

  if (next.has(id)) next.delete(id);
  else next.add(id);
  return next;
}

/** `/算数/4月/` → the trail back to the top. Empty at the top itself. */
export function breadcrumbs(path: string): { name: string; path: string }[] {
  const segments = path.split("/").filter(Boolean);
  if (segments.length === 0) return [];

  const trail: { name: string; path: string }[] = [];
  let accumulated = "/";
  for (const segment of segments) {
    accumulated += `${segment}/`;
    trail.push({ name: segment, path: accumulated });
  }
  return trail;
}

function Breadcrumbs({
  crumbs,
  onNavigate,
}: {
  crumbs: { name: string; path: string }[];
  onNavigate: (path: string) => void;
}) {
  const { t } = useTranslation();
  return (
    <nav className="breadcrumbs" aria-label={t("フォルダ")}>
      <button type="button" className="breadcrumbs__crumb" onClick={() => onNavigate("/")}>
        <Icon name="school" size={18} />
        {t("最上位")}
      </button>
      {crumbs.map((crumb, index) => (
        <span key={crumb.path} className="breadcrumbs__step">
          <Icon name="keyboard_arrow_right" size={18} />
          <button
            type="button"
            className="breadcrumbs__crumb"
            // The last crumb is where we already are.
            disabled={index === crumbs.length - 1}
            onClick={() => onNavigate(crumb.path)}
          >
            {crumb.name}
          </button>
        </span>
      ))}
    </nav>
  );
}

function formatDate(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  return new Intl.DateTimeFormat("ja-JP", {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(date);
}
