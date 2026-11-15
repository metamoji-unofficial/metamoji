/**
 * Typed wrappers over the Tauri command surface.
 *
 * Every call the frontend makes to Rust goes through here, so the IPC contract
 * has exactly one definition site. `invoke` is never called anywhere else.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { GenericModel, GenericTree } from "../model/generic";
import { api } from "../api/client";
import { memoryBackend } from "./memoryBackend";

export interface Tag {
  id: string;
  name: string;
  color: string;
}

export interface Folder {
  id: string;
  name: string;
  parentId: string | null;
  ord: number;
  noteCount: number;
}

export interface NoteSummary {
  id: string;
  title: string;
  createdAt: string;
  updatedAt: string;
  pageCount: number;
  revision: number;
  thumbnail: string | null;
  folderId: string | null;
  trashed: boolean;
  /** Revision the server last accepted, or null if never synced. */
  serverRevision: number | null;
  /** Local revision at the time of that sync. */
  syncedRevision: number | null;
  tags: Tag[];
}

export type NoteSort = "updated" | "created" | "title";

export interface ListQuery {
  trashed?: boolean;
  folderId?: string | null;
  /** With no folder given, restrict to notes that are in no folder. */
  rootOnly?: boolean;
  tagId?: string | null;
  text?: string;
  sort?: NoteSort;
}

export interface AppStatus {
  ready: boolean;
  dataDir: string;
  schemaVersion: number;
  needsLogin: boolean;
}

export interface ImportReport {
  formatVersion: number;
  modelCount: number;
  pageCount: number;
  strokeCount: number;
  unitCounts: Record<string, number>;
  undecodedModels: number;
  warnings: string[];
}

/** What a class note's classroom room added on top of the downloaded file. */
export interface RoomPull {
  directions: number;
  units: number;
  strokes: number;
  assets: number;
  /** Edit kinds the app received but does not apply, named and counted. */
  unsupported: string[];
  /** Set when the room could not be reached; the note still opens. */
  error: string | null;
}

/** One decoded change from a note's classroom, as it happens. */
export type ClassNoteChangeItem =
  | { kind: "stroke"; id: string; stroke: unknown }
  | { kind: "unit"; unitId: string; models: GenericModel[] }
  | { kind: "remove"; id: string }
  | { kind: "asset"; ticket: string };

export interface ClassNoteChange {
  noteId: string;
  boothId: string;
  sequence: number;
  /** The page as the classroom names it — not this app's model id. */
  pageId: string;
  layerId: string | null;
  layerType: string | null;
  changes: ClassNoteChangeItem[];
}

/** Where a note came from, when it came from a class box. */
export interface ClassOrigin {
  driveId: string;
  documentId: string;
  /** The room that carries what everyone writes on the note. */
  roomId: string | null;
}

export interface AtdocImportResult {
  tree: GenericTree;
  report: ImportReport;
  /** The document's own title, when it has one. */
  title: string | null;
  /** Absent for a file on disk, which belongs to no room. */
  room: RoomPull | null;
  /** The room the note belongs to, to be remembered alongside it. */
  roomId: string | null;
}

export interface ExportPagePayload {
  dataUrl: string;
  width: number;
  height: number;
}

/**
 * True when running inside Tauri. `vite dev` can also be opened in a plain
 * browser, which is convenient for working on the editor without a Rust
 * rebuild in the loop; there, the calls below fall back to an in-memory backend
 * instead of throwing on a missing `invoke`.
 */
export const isTauri = (): boolean =>
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

export async function appStatus(): Promise<AppStatus> {
  if (!isTauri()) return memoryBackend.appStatus();
  return invoke<AppStatus>("app_status");
}

export async function onAppReady(fn: (status: AppStatus) => void): Promise<() => void> {
  if (!isTauri()) {
    fn(memoryBackend.appStatus());
    return () => {};
  }
  return listen<AppStatus>("app://ready", (e) => fn(e.payload));
}

// ---------------------------------------------------------------------------
// Library
// ---------------------------------------------------------------------------

export async function libraryList(query: ListQuery = {}): Promise<NoteSummary[]> {
  if (!isTauri()) return memoryBackend.libraryList(query);
  return invoke<NoteSummary[]>("library_list", { query });
}

export async function librarySetFolder(
  id: string,
  folderId: string | null,
): Promise<void> {
  if (!isTauri()) return memoryBackend.librarySetFolder(id, folderId);
  return invoke("library_set_folder", { id, folderId });
}

// ---------------------------------------------------------------------------
// Folders and tags
// ---------------------------------------------------------------------------

export async function librarySetSyncState(
  id: string,
  serverRevision: number,
  syncedRevision: number,
): Promise<void> {
  if (!isTauri()) return memoryBackend.librarySetSyncState(id, serverRevision, syncedRevision);
  return invoke("library_set_sync_state", { id, serverRevision, syncedRevision });
}

export async function syncDriveRevision(): Promise<number> {
  if (!isTauri()) return memoryBackend.syncDriveRevision();
  return invoke<number>("sync_drive_revision");
}

export async function syncSetDriveRevision(revision: number): Promise<void> {
  if (!isTauri()) return memoryBackend.syncSetDriveRevision(revision);
  return invoke("sync_set_drive_revision", { revision });
}

export async function folderList(): Promise<Folder[]> {
  if (!isTauri()) return memoryBackend.folderList();
  return invoke<Folder[]>("folder_list");
}

export async function folderCreate(
  id: string,
  name: string,
  parentId: string | null = null,
): Promise<void> {
  if (!isTauri()) return memoryBackend.folderCreate(id, name, parentId);
  return invoke("folder_create", { id, name, parentId });
}

export async function folderRename(id: string, name: string): Promise<void> {
  if (!isTauri()) return memoryBackend.folderRename(id, name);
  return invoke("folder_rename", { id, name });
}

export async function folderDelete(id: string): Promise<void> {
  if (!isTauri()) return memoryBackend.folderDelete(id);
  return invoke("folder_delete", { id });
}

export async function tagList(): Promise<Tag[]> {
  if (!isTauri()) return memoryBackend.tagList();
  return invoke<Tag[]>("tag_list");
}

export async function tagCreate(id: string, name: string, color: string): Promise<Tag> {
  if (!isTauri()) return memoryBackend.tagCreate(id, name, color);
  return invoke<Tag>("tag_create", { id, name, color });
}

export async function tagDelete(id: string): Promise<void> {
  if (!isTauri()) return memoryBackend.tagDelete(id);
  return invoke("tag_delete", { id });
}

export async function tagSetOnDocument(
  documentId: string,
  tagId: string,
  on: boolean,
): Promise<void> {
  if (!isTauri()) return memoryBackend.tagSetOnDocument(documentId, tagId, on);
  return invoke("tag_set_on_document", { documentId, tagId, on });
}

export async function libraryCreate(tree: GenericTree, title: string): Promise<NoteSummary> {
  if (!isTauri()) return memoryBackend.libraryCreate(tree, title);
  return invoke<NoteSummary>("library_create", { tree, title });
}

export async function libraryRename(id: string, title: string): Promise<void> {
  if (!isTauri()) return memoryBackend.libraryRename(id, title);
  return invoke("library_rename", { id, title });
}

export async function librarySetTrashed(id: string, trashed: boolean): Promise<void> {
  if (!isTauri()) return memoryBackend.librarySetTrashed(id, trashed);
  return invoke("library_set_trashed", { id, trashed });
}

export async function libraryDelete(id: string): Promise<void> {
  if (!isTauri()) return memoryBackend.libraryDelete(id);
  return invoke("library_delete", { id });
}

export async function libraryDuplicate(
  id: string,
  newId: string,
  title: string,
): Promise<NoteSummary> {
  if (!isTauri()) return memoryBackend.libraryDuplicate(id, newId, title);
  return invoke<NoteSummary>("library_duplicate", { id, newId, title });
}

// ---------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------

export async function noteLoad(id: string): Promise<GenericTree> {
  if (!isTauri()) return memoryBackend.noteLoad(id);
  return invoke<GenericTree>("note_load", { id });
}

export async function noteSave(
  tree: GenericTree,
  title: string,
  createdAt: string,
  revision: number,
  searchBody = "",
): Promise<string> {
  if (!isTauri()) return memoryBackend.noteSave(tree, title, createdAt, revision);
  return invoke<string>("note_save", { tree, title, createdAt, revision, searchBody });
}

export async function noteClose(id: string): Promise<void> {
  if (!isTauri()) return;
  return invoke("note_close", { id });
}

export async function noteSetThumbnail(
  id: string,
  pageId: string,
  revision: number,
  pngDataUrl: string,
): Promise<void> {
  if (!isTauri()) return memoryBackend.noteSetThumbnail(id, pageId, revision, pngDataUrl);
  return invoke("note_set_thumbnail", { id, pageId, revision, pngBase64: pngDataUrl });
}

// ---------------------------------------------------------------------------
// Assets
// ---------------------------------------------------------------------------

export async function assetPut(
  noteId: string,
  ticket: string,
  dataUrl: string,
): Promise<string> {
  if (!isTauri()) return memoryBackend.assetPut(noteId, ticket, dataUrl);
  return invoke<string>("asset_put", { noteId, ticket, dataUrl });
}

export async function assetGet(noteId: string, ticket: string): Promise<string> {
  if (!isTauri()) return memoryBackend.assetGet(noteId, ticket);
  return invoke<string>("asset_get", { noteId, ticket });
}

export async function assetList(noteId: string): Promise<string[]> {
  if (!isTauri()) return memoryBackend.assetList(noteId);
  return invoke<string[]>("asset_list", { noteId });
}

export async function fileReadDataUrl(path: string): Promise<string> {
  if (!isTauri()) throw new Error("ファイルの読み込みはデスクトップアプリでのみ利用できます");
  return invoke<string>("file_read_data_url", { path });
}

export async function fileWriteBytes(path: string, dataUrl: string): Promise<void> {
  if (!isTauri()) throw new Error("ファイルの書き出しはデスクトップアプリでのみ利用できます");
  return invoke("file_write_bytes", { path, dataUrl });
}

// ---------------------------------------------------------------------------
// .atdoc import
// ---------------------------------------------------------------------------

export async function atdocImport(
  path: string,
  newRootId: string,
): Promise<AtdocImportResult> {
  if (!isTauri()) return memoryBackend.atdocImport();
  return invoke<AtdocImportResult>("atdoc_import", { path, newRootId });
}

export async function atdocProbe(path: string): Promise<number> {
  if (!isTauri()) throw new Error(".atdoc の判定はデスクトップアプリでのみ利用できます");
  return invoke<number>("atdoc_probe", { path });
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

export async function exportPdf(
  path: string,
  title: string,
  pages: ExportPagePayload[],
): Promise<void> {
  if (!isTauri()) throw new Error("PDF の書き出しはデスクトップアプリでのみ利用できます");
  return invoke("export_pdf", { path, title, pages });
}

// ---------------------------------------------------------------------------
// MetaMoJi cloud sign-in
// ---------------------------------------------------------------------------
//
// These go to Rust rather than being `fetch`ed from here for two reasons:
// `mps.metamoji.com` sends no CORS headers for a `tauri://localhost` origin, and
// the session is a cookie the protocol expects a real jar to hold. See
// `src-tauri/src/cloud.rs`.

export interface School {
  serverUrl: string;
  coLoginId: string;
  isClassRoom: boolean;
  isOnPremise: boolean;
}

export interface ClassGroup {
  name: string;
  id: string;
  /** 出席番号. Empty when the school does not publish them. */
  idNumbers: string[];
}

export interface CloudSession {
  userId: string;
  loginName: string;
  name: string;
  email: string | null;
  coLoginId: string;
  companyId: string | null;
  companyName: string | null;
  isClassRoom: boolean;
  isOnPremise: boolean;
  restHost: string;
}

/** Signing in needs the network stack in Rust; there is no browser stand-in. */
function requireTauri(): void {
  if (!isTauri()) {
    throw new Error(
      "サインインはデスクトップアプリでのみ利用できます(ブラウザからは MetaMoJi のサーバーに接続できません)。",
    );
  }
}

export async function cloudRootServer(): Promise<string> {
  if (!isTauri()) return "https://mps.metamoji.com/";
  return invoke<string>("cloud_root_server");
}

export async function cloudSetRootServer(url: string): Promise<void> {
  if (!isTauri()) return;
  return invoke<void>("cloud_set_root_server", { url });
}

export async function cloudResolveSchool(coLoginId: string): Promise<School> {
  requireTauri();
  return invoke<School>("cloud_resolve_school", { coLoginId });
}

export async function cloudClassGroups(coLoginId: string): Promise<ClassGroup[]> {
  requireTauri();
  return invoke<ClassGroup[]>("cloud_class_groups", { coLoginId });
}

export async function cloudLogin(
  coLoginId: string,
  loginName: string,
  password: string,
): Promise<CloudSession> {
  requireTauri();
  return invoke<CloudSession>("cloud_login", { coLoginId, loginName, password });
}

export async function cloudClassroomLogin(
  coLoginId: string,
  classGroupId: string,
  idNumber: string,
  password: string,
): Promise<CloudSession> {
  requireTauri();
  return invoke<CloudSession>("cloud_classroom_login", {
    coLoginId,
    classGroupId,
    idNumber,
    password,
  });
}

export async function cloudLogout(): Promise<void> {
  if (!isTauri()) return;
  return invoke<void>("cloud_logout");
}

export async function cloudSession(): Promise<CloudSession | null> {
  if (!isTauri()) return null;
  return invoke<CloudSession | null>("cloud_session");
}

// ---------------------------------------------------------------------------
// Classroom
// ---------------------------------------------------------------------------
//
// Two halves: a class box is the shared drive with a join code; a room is the
// live session inside it, carried by a relay socket that Rust owns. Events from
// that socket arrive through `onClassroomEvent`, not as return values.

export interface ClassBox {
  driveId: string;
  groupId: string | null;
  name: string | null;
  joinCode: string | null;
  joinEnabled: boolean | null;
}

export interface CollaboRoom {
  roomId: string;
  title: string | null;
  roomType: string | null;
  ownerId: string | null;
}

export interface RelayInfo {
  host: string;
  port: number;
  sessionId: string;
  serverProtocolVersion: string | null;
  clientDirectionVersion: string | null;
}

export interface CollaboMember {
  userId: string;
  name: string | null;
  role: string | null;
}

/** Mirrors `collabo::socket::CollaboEvent`. */
export type ClassroomEvent =
  | { kind: "connected" }
  | {
      kind: "loggedIn";
      ok: boolean;
      message: string | null;
      roomType: string | null;
      userId: string | null;
      roles: string[];
    }
  | { kind: "boothUpdated"; boothId: string }
  | { kind: "roomUpdated"; key: string; value: string; userId: string | null }
  | { kind: "modeChanged"; key: string; enabled: boolean }
  | { kind: "roleChanged"; key: string; enabled: boolean }
  | { kind: "message"; title: string | null; body: string }
  | { kind: "settingChanged"; setting: string }
  | {
      kind: "direction";
      boothId: string;
      sequence: number;
      userId: string | null;
      ownEcho: boolean;
    }
  | { kind: "postAck"; boothId: string; packetNo: string; ok: boolean }
  | { kind: "finished" }
  | { kind: "disconnected"; reason: string };

// -- class boxes, through the shared client ---------------------------------
//
// These four go out through `@metamoji/sdk` rather than a Tauri command. They
// are plain request/response calls with nothing binary in them and nothing the
// classroom socket depends on, which is what makes them the ones to move
// first. Everything below still goes through Rust: the drive service hands
// back zip archives it parses, and the relay is a raw TLS socket a webview
// cannot open.
//
// The session is not split by this. `createTauriTransport` sends every request
// back down through Rust's own client, so there is one cookie jar and one
// `session.json` whichever side made the call.

export async function classroomCreateBox(name: string): Promise<ClassBox> {
  requireTauri();
  const { data, error } = await api().classBoxes.create({ groupName: name });
  if (error) throw new Error(error.message);

  const driveId = required(data.driveId, "driveId");
  // The create response carries no join code; the teacher needs one to read
  // out, so fetch it rather than making them press a second button.
  const code = await classroomBoxCode(driveId).catch(() => null);
  return {
    driveId,
    groupId: data.groupId ?? null,
    name,
    joinCode: code?.joinCode ?? null,
    joinEnabled: code?.joinEnabled ?? null,
  };
}

export async function classroomJoinBox(joinCode: string): Promise<ClassBox> {
  requireTauri();
  const { data, error } = await api().classBoxes.join({ joinCode });
  if (error) throw new Error(error.message);
  return {
    driveId: required(data.driveId, "driveId"),
    groupId: null,
    name: null,
    joinCode,
    joinEnabled: null,
  };
}

export async function classroomBoxCode(driveId: string, regenerate = false): Promise<ClassBox> {
  requireTauri();
  const { data, error } = await api().classBoxes.getJoinCode({
    driveId,
    updateJoinCode: regenerate,
  });
  if (error) throw new Error(error.message);
  return {
    driveId,
    groupId: null,
    name: null,
    joinCode: data.joinCode ?? null,
    joinEnabled: data.joinEnabled ?? null,
  };
}

export async function classroomUpdateBox(
  driveId: string,
  name: string | null,
  joinEnabled: boolean | null,
): Promise<void> {
  requireTauri();
  const { error } = await api().classBoxes.update({
    driveId,
    ...(name !== null ? { name } : {}),
    // A tri-state on the wire: the third value means "leave it alone", which
    // is what omitting the field would have meant anyway.
    ...(joinEnabled !== null ? { joinEnabled: joinEnabled ? "ENABLED" : "DISABLED" } : {}),
  } as const);
  if (error) throw new Error(error.message);
}

/** A field the server is expected to send; its absence is the error. */
function required(value: string | undefined, field: string): string {
  if (!value) throw new Error(`サーバーの応答に ${field} がありません`);
  return value;
}

export async function classroomCreateRoom(
  title: string,
  roomType = "formal",
): Promise<CollaboRoom> {
  requireTauri();
  return invoke<CollaboRoom>("classroom_create_room", { title, roomType });
}

export async function classroomEnter(
  roomId: string,
  driveId: string,
  nickname: string,
  roomPassword: string | null = null,
): Promise<RelayInfo> {
  requireTauri();
  return invoke<RelayInfo>("classroom_enter", { roomId, driveId, nickname, roomPassword });
}

export async function classroomLeave(): Promise<void> {
  if (!isTauri()) return;
  return invoke<void>("classroom_leave");
}

export async function classroomMembers(roomId: string): Promise<CollaboMember[]> {
  requireTauri();
  return invoke<CollaboMember[]>("classroom_members", { roomId });
}

export async function classroomAttachBooth(
  boothId: string,
  lastSequence = 0,
): Promise<void> {
  requireTauri();
  return invoke<void>("classroom_attach_booth", { boothId, lastSequence });
}

export async function classroomDetachBooth(boothId: string): Promise<void> {
  requireTauri();
  return invoke<void>("classroom_detach_booth", { boothId });
}

export async function classroomCurrentRoom(): Promise<string | null> {
  if (!isTauri()) return null;
  return invoke<string | null>("classroom_current_room");
}

/** Subscribes to relay pushes. Returns the unsubscribe function. */
export async function onClassroomEvent(
  fn: (event: ClassroomEvent) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  return listen<ClassroomEvent>("classroom://event", (e) => fn(e.payload));
}

// ---------------------------------------------------------------------------
// Class box contents
// ---------------------------------------------------------------------------
//
// A class box is a drive, and drives live behind a separate service with its
// own host and session — `src-tauri/src/drive/`. `classboxOpen` does the whole
// handshake, so there is no half-open state for the UI to represent.

export interface DriveDocument {
  documentId: string;
  title: string | null;
  revision: string | null;
  updatedAt: string | null;
  /** The folder that claims it, as an absolute path. `/` for the top level. */
  folderPath: string;
}

/** One folder in a class box. Folders are keyed by path, not by id. */
export interface DriveFolder {
  /** `/算数/4月/` — slash-delimited, with a slash at each end. */
  absPath: string;
  name: string;
  parentPath: string | null;
  depth: number;
}

export interface ClassBoxListing {
  documents: DriveDocument[];
  folders: DriveFolder[];
  /**
   * Entries in the archive that were not document JSON. Non-empty means the
   * archive holds more than the decoder understands — an unreadable class box
   * and an empty one must not look the same.
   */
  unrecognised: string[];
  /** Records seen, deleted ones included. */
  recordCount: number;
}

export async function classboxOpen(driveId: string): Promise<ClassBoxListing> {
  requireTauri();
  return invoke<ClassBoxListing>("classbox_open", { driveId });
}

export async function classboxRevision(driveId: string): Promise<string | null> {
  requireTauri();
  return invoke<string | null>("classbox_revision", { driveId });
}

/** Downloads a note and converts it with the same importer `.atdoc` uses. */
export async function classboxOpenNote(
  driveId: string,
  documentId: string,
  newRootId: string,
  revision: string | null = null,
): Promise<AtdocImportResult> {
  requireTauri();
  return invoke<AtdocImportResult>("classbox_open_note", {
    driveId,
    documentId,
    revision,
    newRootId,
  });
}

/**
 * Records that a note is a copy of a class-box document.
 *
 * Without it the note is just a local file: sending what the user writes on it
 * back to the class needs to know which document it is a copy of.
 */
export async function classboxLink(
  noteId: string,
  driveId: string,
  documentId: string,
  roomId: string | null,
): Promise<void> {
  if (!isTauri()) return;
  await invoke("classbox_link", { noteId, driveId, documentId, roomId });
}

export async function classboxOrigin(noteId: string): Promise<ClassOrigin | null> {
  if (!isTauri()) return null;
  return invoke<ClassOrigin | null>("classbox_origin", { noteId });
}

/**
 * Sends everything written on this note's own layers to its classroom, and
 * returns how many strokes went. Already-sent strokes are skipped, so calling
 * it after every save is the intended use.
 */
export async function classboxSendStrokes(noteId: string): Promise<number> {
  if (!isTauri()) return 0;
  return invoke<number>("classbox_send_strokes", { noteId });
}

/**
 * Stays in the note's classroom while it is open, so an edit made anywhere
 * else in the class arrives here as it happens.
 *
 * Returns false for a note with no classroom to watch. There is no polling
 * interval to choose: the relay pushes.
 */
export async function classnoteWatch(noteId: string): Promise<boolean> {
  if (!isTauri()) return false;
  return invoke<boolean>("classnote_watch", { noteId });
}

/** What a resync found and repaired. */
export interface Resync {
  inRoom: number;
  neverArrived: number;
  erasedElsewhere: number;
  sent: number;
  /** Element ids the room does not hold; the note has to stop showing them. */
  gone: string[];
  problems: string[];
}

/**
 * Compares the note with its classroom and repairs the difference.
 *
 * Sync gets stuck when the two sides disagree about what has been sent, and
 * neither notices: the room's own history is what settles it.
 */
export async function classnoteResync(noteId: string): Promise<Resync | null> {
  if (!isTauri()) return null;
  return invoke<Resync>("classnote_resync", { noteId });
}

/** Fires when the classroom connection drops; the caller has to ask again. */
export async function onClassNoteEnded(
  fn: (info: { noteId: string; reason: string }) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  return listen<{ noteId: string; reason: string }>("classnote://ended", (e) => fn(e.payload));
}

export async function classnoteUnwatch(): Promise<void> {
  if (!isTauri()) return;
  await invoke("classnote_unwatch");
}

/** Subscribes to what the watched note's classroom sends. */
export async function onClassNoteChange(
  fn: (change: ClassNoteChange) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  return listen<ClassNoteChange>("classnote://change", (e) => fn(e.payload));
}

export async function classboxNoteThumbnail(
  driveId: string,
  documentId: string,
  revision: string | null = null,
): Promise<string> {
  requireTauri();
  return invoke<string>("classbox_note_thumbnail", { driveId, documentId, revision });
}

export async function classboxClose(): Promise<void> {
  if (!isTauri()) return;
  return invoke<void>("classbox_close");
}

/** A class box the signed-in user belongs to. */
export interface DriveEntry {
  driveId: string;
  name: string | null;
  groupId: string | null;
  teamId: string | null;
  hidden: boolean;
}

/** The user's class boxes. Hidden ones are already filtered out. */
export async function classboxList(): Promise<DriveEntry[]> {
  requireTauri();
  return invoke<DriveEntry[]>("classbox_list");
}

/** One note's background, saved to disk. */
export interface SavedBackground {
  documentId: string;
  title: string | null;
  /** File names written, relative to the folder the download was into. */
  files: string[];
}

/** A note with no background to download — not a failure. */
export interface SkippedNote {
  documentId: string;
  title: string | null;
  reason: string;
}

export interface FailedNote {
  documentId: string;
  title: string | null;
  error: string;
}

/** What downloading a class-box folder found. */
export interface FolderDownloadResult {
  /** Notes under the folder, subfolders included. */
  total: number;
  saved: SavedBackground[];
  skipped: SkippedNote[];
  failed: FailedNote[];
}

/**
 * Downloads every note's background (PDF or image, no handwriting) under a
 * class-box folder into `destDir`. Subfolders are included and mirrored on
 * disk, matching `DriveFolder`'s own hierarchy.
 */
export async function classboxDownloadFolder(
  driveId: string,
  folderPath: string,
  destDir: string,
): Promise<FolderDownloadResult> {
  requireTauri();
  return invoke<FolderDownloadResult>("classbox_download_folder", {
    driveId,
    folderPath,
    destDir,
  });
}

/** How far a folder download has gotten. */
export interface DownloadProgress {
  done: number;
  total: number;
  /** The note about to download, or `null` once the last one is done. */
  title: string | null;
}

export async function onClassboxDownloadProgress(
  fn: (progress: DownloadProgress) => void,
): Promise<() => void> {
  if (!isTauri()) return () => {};
  return listen<DownloadProgress>("classbox://download-progress", (e) => fn(e.payload));
}
