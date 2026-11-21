//! The IPC surface.
//!
//! Deliberately narrow: the frontend owns the live document and never sees a
//! file path it did not get from a dialog, never sees SQL, and never touches the
//! filesystem. That is why neither `tauri-plugin-fs` nor `tauri-plugin-sql` is
//! installed — both would hand the webview capabilities it has no use for.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use tauri::{Emitter, State};

use crate::cloud::{ClassGroup, CloudClient, CloudSession, DriveEntry, School};
use crate::collabo::{
    self,
    session::{ClassroomState, CollaboMember, CollaboRoom, EnterResult},
};
use crate::drive::{self, listing::Listing, DriveClient, DriveDocument};
use crate::error::{AppError, AppResult};
use crate::model::{AppStatus, GenericTree, NoteSummary};
use crate::state::AppState;
use crate::storage::{now_iso, Folder, ListQuery, NoteStore, Tag};

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn app_status(state: State<'_, AppState>) -> AppStatus {
    state.status()
}

// ---------------------------------------------------------------------------
// Library
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn library_list(
    state: State<'_, AppState>,
    query: Option<ListQuery>,
) -> AppResult<Vec<NoteSummary>> {
    state
        .catalog
        .lock()
        .unwrap()
        .list(&query.unwrap_or_default())
}

// -- folders ----------------------------------------------------------------

#[tauri::command]
pub fn library_set_sync_state(
    state: State<'_, AppState>,
    id: String,
    server_revision: i64,
    synced_revision: i64,
) -> AppResult<()> {
    state
        .catalog
        .lock()
        .unwrap()
        .set_sync_state(&id, server_revision, synced_revision)
}

#[tauri::command]
pub fn sync_drive_revision(state: State<'_, AppState>) -> i64 {
    state.catalog.lock().unwrap().drive_revision()
}

#[tauri::command]
pub fn sync_set_drive_revision(state: State<'_, AppState>, revision: i64) -> AppResult<()> {
    state.catalog.lock().unwrap().set_drive_revision(revision)
}

#[tauri::command]
pub fn folder_list(state: State<'_, AppState>) -> AppResult<Vec<Folder>> {
    state.catalog.lock().unwrap().folders()
}

#[tauri::command]
pub fn folder_create(
    state: State<'_, AppState>,
    id: String,
    name: String,
    parent_id: Option<String>,
) -> AppResult<()> {
    state
        .catalog
        .lock()
        .unwrap()
        .create_folder(&id, &name, parent_id.as_deref())
}

#[tauri::command]
pub fn folder_rename(state: State<'_, AppState>, id: String, name: String) -> AppResult<()> {
    state.catalog.lock().unwrap().rename_folder(&id, &name)
}

#[tauri::command]
pub fn folder_delete(state: State<'_, AppState>, id: String) -> AppResult<()> {
    state.catalog.lock().unwrap().delete_folder(&id)
}

#[tauri::command]
pub fn library_set_folder(
    state: State<'_, AppState>,
    id: String,
    folder_id: Option<String>,
) -> AppResult<()> {
    state
        .catalog
        .lock()
        .unwrap()
        .set_folder(&id, folder_id.as_deref())
}

// -- tags -------------------------------------------------------------------

#[tauri::command]
pub fn tag_list(state: State<'_, AppState>) -> AppResult<Vec<Tag>> {
    state.catalog.lock().unwrap().tags()
}

#[tauri::command]
pub fn tag_create(
    state: State<'_, AppState>,
    id: String,
    name: String,
    color: String,
) -> AppResult<Tag> {
    state.catalog.lock().unwrap().create_tag(&id, &name, &color)
}

#[tauri::command]
pub fn tag_delete(state: State<'_, AppState>, id: String) -> AppResult<()> {
    state.catalog.lock().unwrap().delete_tag(&id)
}

#[tauri::command]
pub fn tag_set_on_document(
    state: State<'_, AppState>,
    document_id: String,
    tag_id: String,
    on: bool,
) -> AppResult<()> {
    state
        .catalog
        .lock()
        .unwrap()
        .set_document_tag(&document_id, &tag_id, on)
}

/// Registers a brand-new note. The frontend has already built the tree; this
/// creates the file, writes it, and indexes it.
#[tauri::command]
pub fn library_create(
    state: State<'_, AppState>,
    tree: GenericTree,
    title: String,
) -> AppResult<NoteSummary> {
    let id = tree.root_id.clone();
    let path = state.paths.note_path(&id);
    let now = now_iso();

    {
        let mut store = NoteStore::open(&path)?;
        store.write_tree(&tree, &title, &now, &now, 0)?;
    }

    let page_count = count_pages(&tree);
    state
        .catalog
        .lock()
        .unwrap()
        .upsert(&id, &path, &title, &now, &now, page_count, 0)?;

    Ok(NoteSummary {
        id,
        title,
        created_at: now.clone(),
        updated_at: now,
        page_count,
        revision: 0,
        thumbnail: None,
        folder_id: None,
        trashed: false,
        server_revision: None,
        synced_revision: None,
        tags: Vec::new(),
    })
}

#[tauri::command]
pub fn library_rename(state: State<'_, AppState>, id: String, title: String) -> AppResult<()> {
    state
        .catalog
        .lock()
        .unwrap()
        .rename(&id, &title, &now_iso())
}

#[tauri::command]
pub fn library_set_trashed(state: State<'_, AppState>, id: String, trashed: bool) -> AppResult<()> {
    state.catalog.lock().unwrap().set_trashed(&id, trashed)
}

/// Permanently removes a note and its file. Reachable only from the trash view,
/// so the user has already had one chance to change their mind.
#[tauri::command]
pub fn library_delete(state: State<'_, AppState>, id: String) -> AppResult<()> {
    state.close_note(&id);
    let path = state.catalog.lock().unwrap().path_of(&id)?;
    state.catalog.lock().unwrap().remove(&id)?;

    // SQLite leaves -wal and -shm siblings behind; take them too.
    for suffix in ["", "-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{}", path.to_string_lossy(), suffix));
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

#[tauri::command]
pub fn library_duplicate(
    state: State<'_, AppState>,
    id: String,
    new_id: String,
    title: String,
) -> AppResult<NoteSummary> {
    let src = state.catalog.lock().unwrap().path_of(&id)?;
    let dst = state.paths.note_path(&new_id);
    std::fs::copy(&src, &dst)?;

    let now = now_iso();
    let (page_count, revision) = {
        let store = NoteStore::open(&dst)?;
        let (_, _, _, revision, page_count) = store.meta()?;
        (page_count, revision)
    };

    state
        .catalog
        .lock()
        .unwrap()
        .upsert(&new_id, &dst, &title, &now, &now, page_count, revision)?;

    Ok(NoteSummary {
        id: new_id,
        title,
        created_at: now.clone(),
        updated_at: now,
        page_count,
        revision,
        thumbnail: None,
        folder_id: None,
        trashed: false,
        server_revision: None,
        synced_revision: None,
        tags: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn note_load(state: State<'_, AppState>, id: String) -> AppResult<GenericTree> {
    let path = state.paths.note_path(&id);
    if !path.exists() {
        return Err(AppError::NoteNotFound(id));
    }
    let store = state.note(&id)?;
    let tree = store.lock().unwrap().read_tree()?;
    let _ = state.catalog.lock().unwrap().touch_access(&id, &now_iso());
    Ok(tree)
}

#[tauri::command]
pub fn note_save(
    state: State<'_, AppState>,
    tree: GenericTree,
    title: String,
    created_at: String,
    revision: i64,
    // `search_body` is the note's text-unit content, flattened. The frontend
    // supplies it because it is the only side that knows how to read a unit.
    search_body: Option<String>,
) -> AppResult<String> {
    let id = tree.root_id.clone();
    let now = now_iso();
    let store = state.note(&id)?;
    store
        .lock()
        .unwrap()
        .write_tree(&tree, &title, &created_at, &now, revision)?;

    let page_count = count_pages(&tree);
    let path = state.paths.note_path(&id);
    state.catalog.lock().unwrap().upsert(
        &id,
        &path,
        &title,
        &created_at,
        &now,
        page_count,
        revision,
    )?;
    state.catalog.lock().unwrap().index_document(
        &id,
        &title,
        search_body.as_deref().unwrap_or(""),
    )?;
    Ok(now)
}

#[tauri::command]
pub fn note_close(state: State<'_, AppState>, id: String) {
    state.close_note(&id);
}

#[tauri::command]
pub fn note_set_thumbnail(
    state: State<'_, AppState>,
    id: String,
    page_id: String,
    revision: i64,
    png_base64: String,
) -> AppResult<()> {
    let png = decode_data_url(&png_base64)?;
    let store = state.note(&id)?;
    store
        .lock()
        .unwrap()
        .put_thumbnail(&page_id, revision, &png)?;
    // The library grid shows the first page, so that one doubles as the cover.
    state.catalog.lock().unwrap().set_thumbnail(&id, &png)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Assets
// ---------------------------------------------------------------------------

/// Stores bytes and returns the ticket the model will reference.
///
/// `data` is a data URL, which is how the frontend already has an image after a
/// paste or a file read.
#[tauri::command]
pub fn asset_put(
    state: State<'_, AppState>,
    note_id: String,
    ticket: String,
    data_url: String,
) -> AppResult<String> {
    let (mime, bytes) = parse_data_url(&data_url)?;
    let store = state.note(&note_id)?;
    store.lock().unwrap().put_asset(&ticket, &mime, &bytes)?;
    Ok(ticket)
}

#[tauri::command]
pub fn asset_get(state: State<'_, AppState>, note_id: String, ticket: String) -> AppResult<String> {
    let store = state.note(&note_id)?;
    let (mime, bytes) = store.lock().unwrap().get_asset(&ticket)?;
    Ok(format!(
        "data:{};base64,{}",
        mime,
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

#[tauri::command]
pub fn asset_list(state: State<'_, AppState>, note_id: String) -> AppResult<Vec<String>> {
    let store = state.note(&note_id)?;
    let ids = store.lock().unwrap().asset_ids()?;
    Ok(ids)
}

/// Shows a native "open file" dialog and, if the user picks one, reads it and
/// returns it as a data URL. The path never crosses the IPC boundary: the
/// webview supplies only the filter, and the pick + read happen here in one
/// step, so a malicious script cannot hand this command a path of its own
/// choosing (see `reports/vulnerabilities/001_arbitrary-file-read-write-via-ipc.md`).
#[tauri::command]
pub fn pick_and_read_file(
    app: tauri::AppHandle,
    filter_name: String,
    extensions: Vec<String>,
) -> AppResult<Option<String>> {
    use tauri_plugin_dialog::DialogExt;

    let ext_refs: Vec<&str> = extensions.iter().map(String::as_str).collect();
    let Some(picked) = app
        .dialog()
        .file()
        .add_filter(&filter_name, &ext_refs)
        .blocking_pick_file()
    else {
        return Ok(None);
    };
    let path = picked
        .into_path()
        .map_err(|e| AppError::other(e.to_string()))?;
    let bytes = std::fs::read(&path)?;
    let mime = mime_for_path(path.to_string_lossy().as_ref());
    Ok(Some(format!(
        "data:{};base64,{}",
        mime,
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    )))
}

/// Shows a native "save file" dialog and, if the user picks a destination,
/// writes the given data URL's bytes there. Same rationale as
/// `pick_and_read_file`: the destination path is chosen by the OS dialog, not
/// supplied by the webview.
#[tauri::command]
pub fn pick_and_write_file(
    app: tauri::AppHandle,
    default_name: String,
    filter_name: String,
    extensions: Vec<String>,
    data_url: String,
) -> AppResult<bool> {
    use tauri_plugin_dialog::DialogExt;

    let ext_refs: Vec<&str> = extensions.iter().map(String::as_str).collect();
    let Some(picked) = app
        .dialog()
        .file()
        .set_file_name(&default_name)
        .add_filter(&filter_name, &ext_refs)
        .blocking_save_file()
    else {
        return Ok(false);
    };
    let path = picked
        .into_path()
        .map_err(|e| AppError::other(e.to_string()))?;
    let (_, bytes) = parse_data_url(&data_url)?;
    std::fs::write(&path, bytes)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn count_pages(tree: &GenericTree) -> i64 {
    tree.models
        .values()
        .filter(|m| m.model_type == "$page")
        .count() as i64
}

fn parse_data_url(data_url: &str) -> AppResult<(String, Vec<u8>)> {
    let rest = data_url
        .strip_prefix("data:")
        .ok_or_else(|| AppError::other("not a data URL"))?;
    let (meta, payload) = rest
        .split_once(",")
        .ok_or_else(|| AppError::other("malformed data URL"))?;
    let mime = meta.split(';').next().unwrap_or("application/octet-stream");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| AppError::other(format!("bad base64 in data URL: {e}")))?;
    Ok((mime.to_string(), bytes))
}

fn decode_data_url(value: &str) -> AppResult<Vec<u8>> {
    if value.starts_with("data:") {
        Ok(parse_data_url(value)?.1)
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(value)
            .map_err(|e| AppError::other(format!("bad base64: {e}")))
    }
}

fn mime_for_path(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".pdf") {
        "application/pdf"
    } else {
        "application/octet-stream"
    }
}

// ---------------------------------------------------------------------------
// MetaMoJi cloud sign-in
// ---------------------------------------------------------------------------
//
// The password crosses this boundary once, on its way to `cloud.rs`, and is
// never stored or echoed back. What comes back is an identity; the session
// itself is a cookie held in the Rust process (see `cloud.rs`).

#[tauri::command]
pub fn cloud_root_server(cloud: State<'_, CloudClient>) -> String {
    cloud.root_server()
}

#[tauri::command]
pub fn cloud_set_root_server(cloud: State<'_, CloudClient>, url: String) {
    cloud.set_root_server(&url);
}

/// Looks up which server a 学校ID lives on. Also the cheapest way to tell the
/// user their school id is wrong before they have typed a password.
#[tauri::command]
pub async fn cloud_resolve_school(
    cloud: State<'_, CloudClient>,
    co_login_id: String,
) -> AppResult<School> {
    cloud.resolve_school(&co_login_id).await
}

#[tauri::command]
pub async fn cloud_class_groups(
    cloud: State<'_, CloudClient>,
    co_login_id: String,
) -> AppResult<Vec<ClassGroup>> {
    cloud.class_groups(&co_login_id).await
}

#[tauri::command]
pub async fn cloud_login(
    cloud: State<'_, CloudClient>,
    co_login_id: String,
    login_name: String,
    password: String,
) -> AppResult<CloudSession> {
    cloud.login(&co_login_id, &login_name, &password).await
}

#[tauri::command]
pub async fn cloud_classroom_login(
    cloud: State<'_, CloudClient>,
    co_login_id: String,
    class_group_id: String,
    id_number: String,
    password: String,
) -> AppResult<CloudSession> {
    cloud
        .classroom_login(&co_login_id, &class_group_id, &id_number, &password)
        .await
}

#[tauri::command]
pub async fn cloud_logout(cloud: State<'_, CloudClient>) -> AppResult<()> {
    cloud.logout().await
}

#[tauri::command]
pub fn cloud_session(cloud: State<'_, CloudClient>) -> Option<CloudSession> {
    cloud.session()
}

// ---------------------------------------------------------------------------
// Classroom
// ---------------------------------------------------------------------------
//
// Two halves. Class boxes (`/users3/crbox/*`) are storage: a drive the class
// shares, with a join code. Rooms (`cosmos/*` plus the relay socket) are the
// live session inside one.
//
// Sending an edit is deliberately absent. A Direction's payload is a
// serialised `direction` model in the same `IModel` format as `.atdoc`
// (`collabo-socket-protocol.md` §6.1), and this build has a reader for that
// format but no writer — see `atdoc/`. Receiving works; sending needs the
// writer first.

#[tauri::command]
pub async fn classroom_create_room(
    cloud: State<'_, CloudClient>,
    classroom: State<'_, ClassroomState>,
    title: String,
    room_type: String,
) -> AppResult<CollaboRoom> {
    // `cosmos/*` puts the user id in its `authInfo` part, so it needs the same
    // repair the drive service does.
    cloud.ensure_complete().await?;
    classroom
        .rest(&cloud)
        .await?
        .create_room(&title, &room_type)
        .await
}

#[tauri::command]
pub async fn classroom_enter(
    app: tauri::AppHandle,
    cloud: State<'_, CloudClient>,
    classroom: State<'_, ClassroomState>,
    room_id: String,
    drive_id: String,
    nickname: String,
    room_password: Option<String>,
) -> AppResult<EnterResult> {
    cloud.ensure_complete().await?;
    classroom
        .enter(
            &app,
            &cloud,
            &room_id,
            &drive_id,
            &nickname,
            room_password.as_deref(),
        )
        .await
}

#[tauri::command]
pub async fn classroom_leave(classroom: State<'_, ClassroomState>) -> AppResult<()> {
    classroom.leave().await;
    Ok(())
}

#[tauri::command]
pub async fn classroom_members(
    cloud: State<'_, CloudClient>,
    classroom: State<'_, ClassroomState>,
    room_id: String,
) -> AppResult<Vec<CollaboMember>> {
    classroom.rest(&cloud).await?.member_list(&room_id).await
}

#[tauri::command]
pub async fn classroom_attach_booth(
    classroom: State<'_, ClassroomState>,
    booth_id: String,
    last_sequence: i64,
) -> AppResult<()> {
    classroom.attach_booth(&booth_id, last_sequence).await
}

#[tauri::command]
pub async fn classroom_detach_booth(
    classroom: State<'_, ClassroomState>,
    booth_id: String,
) -> AppResult<()> {
    classroom.detach_booth(&booth_id).await
}

#[tauri::command]
pub fn classroom_current_room(classroom: State<'_, ClassroomState>) -> Option<String> {
    classroom.room_id()
}

// ---------------------------------------------------------------------------
// Class box contents
// ---------------------------------------------------------------------------
//
// A class box is a drive, and drives live behind `SdCloudService` — a separate
// service with its own host, its own session and its own conventions. Opening
// one is therefore three steps, done together so a half-open box cannot exist:
// find the drive's home, sign into it, and read its contents.

/// The class boxes the signed-in user belongs to.
///
/// Hidden ones are filtered here rather than in the UI: `hidden` means the
/// user has already said they do not want to see this drive, and every screen
/// that showed it would have to remember to ask.
#[tauri::command]
pub async fn classbox_list(cloud: State<'_, CloudClient>) -> AppResult<Vec<DriveEntry>> {
    Ok(cloud
        .drive_entries()
        .await?
        .into_iter()
        .filter(|entry| !entry.hidden)
        .collect())
}

#[tauri::command]
pub async fn classbox_open(
    cloud: State<'_, CloudClient>,
    drive: State<'_, DriveClient>,
    drive_id: String,
) -> AppResult<Listing> {
    open_drive_listing(&cloud, &drive, &drive_id).await
}

/// Signs into a drive and reads its listing.
///
/// Shared by `classbox_open` and `classbox_download_folder`: both need the
/// same four calls to two different services before they can do anything
/// else, and naming the step in the error is what turns "could not open the
/// class" into something actionable — the steps fail for very different
/// reasons and are fixed in different files.
async fn open_drive_listing(
    cloud: &CloudClient,
    drive: &DriveClient,
    drive_id: &str,
) -> AppResult<Listing> {
    let step = |name: &'static str| move |e: AppError| step_failed(name, e);

    // A session stored by an older build may lack the user id the drive service
    // authenticates with. Repairing it here costs one request and saves the
    // user an instruction they should not have to follow.
    cloud
        .ensure_complete()
        .await
        .map_err(step("サインインの更新"))?;

    let home = cloud
        .drive_home(drive_id)
        .await
        .map_err(step("場所の取得"))?;
    let (user_id, password, qwd) = cloud
        .drive_credential()
        .ok_or_else(|| AppError::other("サインインしていません"))?;

    drive
        .login(&home, &user_id, password.as_deref(), qwd.as_deref())
        .await
        .map_err(step("クラスへのサインイン"))?;
    // Announces the sync before reading; the original always does, and a drive
    // that refuses it will refuse the read too — better to find out here.
    drive
        .sync_start(drive_id)
        .await
        .map_err(step("同期の開始"))?;

    let file = drive
        .drive_data(drive_id, None)
        .await
        .map_err(step("中身の取得"))?;
    drive::listing::parse(file.bytes).map_err(step("中身の解釈"))
}

/// Prefixes an error with the step that produced it, unless it is one the
/// caller handles rather than shows.
fn step_failed(step: &str, err: AppError) -> AppError {
    match err {
        AppError::NotLoggedIn => err,
        other => AppError::other(format!("{step}に失敗: {other}")),
    }
}

#[tauri::command]
pub async fn classbox_revision(
    drive: State<'_, DriveClient>,
    drive_id: String,
) -> AppResult<Option<String>> {
    drive.last_revision(&drive_id).await
}

/// Downloads a note from the class box and converts it to this app's model.
///
/// The bytes come back in MetaMoJi's own container format — the same one
/// `.atdoc` uses — so the importer that was written for local files reads them
/// unchanged. That is the payoff for having built import against the real
/// format rather than a simplification of it.
#[tauri::command]
// Eight arguments because four of them are injected state: the note store, the
// tenant session, the classroom registration and the drive client are four
// different services, and a struct wrapping them would only hide that.
#[allow(clippy::too_many_arguments)]
pub async fn classbox_open_note(
    state: State<'_, AppState>,
    cloud: State<'_, CloudClient>,
    classroom: State<'_, ClassroomState>,
    drive: State<'_, DriveClient>,
    drive_id: String,
    document_id: String,
    revision: Option<String>,
    new_root_id: String,
) -> AppResult<crate::AtdocImportResult> {
    let file = drive
        .document_data(&drive_id, &document_id, revision.as_deref())
        .await?;
    let result = crate::atdoc::import(file.bytes, &new_root_id)?;
    let mut tree = result.tree;
    // The note does not exist yet — `library_create` writes the tree a moment
    // from now — but its asset store does, because opening one creates it. The
    // PDF a class handout is built on lands here rather than travelling to the
    // webview as base64 and back.
    store_imported_assets(&state, &new_root_id, result.assets)?;

    // The file the drive hands out holds only what the teacher put in it.
    // Everything the student wrote is in the note's room, so opening the note
    // means asking the room for its history as well.
    let room_id = room_id_of(&drive, &drive_id, &document_id)
        .await
        .ok()
        .flatten();
    let room = match room_id.clone() {
        Some(room_id) => {
            let ledger = state.note(&new_root_id)?.lock().unwrap().room_strokes()?;
            match collabo::pull::fetch(&cloud, &classroom, &room_id, &mut tree, &ledger).await {
                Ok((pull, assets)) => {
                    let store = state.note(&new_root_id)?;
                    let store = store.lock().unwrap();
                    for (ticket, mime, bytes) in assets {
                        store.put_asset(&ticket, &mime, &bytes)?;
                    }
                    // Remembered so that erasing one of these later can be
                    // reported to the room by the id it knows it by.
                    // A stroke that came from the room is keyed by its own
                    // element id in the note too, so the two ids match here.
                    for (element_id, layer_id) in &pull.stroke_ids {
                        store.remember_room_stroke(element_id, element_id, layer_id)?;
                    }
                    Some(pull)
                }
                // A room that cannot be reached is worth saying so about, but
                // it must not stop the note opening: the handout itself is
                // already downloaded and readable.
                Err(err) => Some(collabo::pull::RoomPull {
                    error: Some(err.to_string()),
                    ..Default::default()
                }),
            }
        }
        None => None,
    };

    Ok(crate::AtdocImportResult {
        tree,
        report: result.report,
        title: result.title,
        room,
        room_id,
    })
}

/// Sends everything written on this note's personal layers to its classroom.
///
/// The note is a local copy; what makes a student's writing visible to the
/// teacher is the room, not the file. Each stroke goes as its own Direction on
/// the booth of the layer it is on, and is marked so a second send does not
/// draw it twice. A shape, text box or form travels the same way, except that
/// an edit to one already sent re-sends its whole model instead of adding a
/// second copy — see `collabo::send::unit_changes`.
#[tauri::command]
pub async fn classbox_send_strokes(
    state: State<'_, AppState>,
    cloud: State<'_, CloudClient>,
    classroom: State<'_, ClassroomState>,
    drive: State<'_, DriveClient>,
    note_id: String,
) -> AppResult<usize> {
    let Some(origin) = state.catalog.lock().unwrap().class_origin(&note_id)? else {
        // Not a copy of anything: an ordinary note has nowhere to send to.
        return Ok(0);
    };
    let tree = {
        let store = state.note(&note_id)?;
        let tree = store.lock().unwrap().read_tree()?;
        tree
    };

    let (ledger, unit_ledger) = {
        let store = state.note(&note_id)?;
        let store = store.lock().unwrap();
        (store.room_strokes()?, store.room_units()?)
    };
    let (waiting, removals, stale) = collabo::send::changes(&tree, &ledger);
    let (unit_added, unit_changed, unit_removed, unit_stale) =
        collabo::send::unit_changes(&tree, &unit_ledger);
    if waiting.is_empty()
        && removals.is_empty()
        && unit_added.is_empty()
        && unit_changed.is_empty()
        && unit_removed.is_empty()
    {
        if stale > 0 || unit_stale > 0 {
            return Err(AppError::other(
                "このノートは古い形式で取り込まれているため、教室に送れません。\
                 クラスボックスから開き直してください。",
            ));
        }
        return Ok(0);
    }

    // The room id is kept with the note precisely so this does not need the
    // drive service, which is only signed in while a class box is open. The
    // lookup is the fallback for notes taken before it was recorded.
    let room_id = match origin.room_id.clone() {
        Some(room_id) => room_id,
        None => {
            let room_id = room_id_of(&drive, &origin.drive_id, &origin.document_id)
                .await
                .map_err(|e| {
                    AppError::other(format!(
                        "このノートの教室が分かりません。クラスボックスから開き直してください ({e})"
                    ))
                })?
                .ok_or_else(|| AppError::other("このノートには教室がありません"))?;
            state.catalog.lock().unwrap().set_class_origin(
                &note_id,
                &origin.drive_id,
                &origin.document_id,
                Some(&room_id),
            )?;
            room_id
        }
    };

    // Through the note's own connection when it has one. Opening a second and
    // logging out of it ends the room session for the whole device, which is
    // what stopped updates arriving after the first send.
    let (posted, posted_units) = match classroom.watch_identity(&note_id).await {
        Some((watched_room, room_user_id)) if watched_room == room_id => {
            let Some(session) = cloud.session() else {
                return Err(AppError::other("サインインしていません"));
            };
            let author = collabo::pull::author_of(&session, &room_id, room_user_id);
            let (mut frames, posted) = collabo::pull::build_posts(&waiting, &removals, &author)?;
            let (unit_frames, posted_units) =
                collabo::pull::build_unit_posts(&unit_added, &unit_changed, &unit_removed)?;
            frames.extend(unit_frames);
            classroom.post_for_note(&note_id, frames).await;
            (posted, posted_units)
        }
        _ => {
            collabo::pull::post_strokes(
                &cloud,
                &classroom,
                &room_id,
                &waiting,
                &removals,
                &unit_added,
                &unit_changed,
                &unit_removed,
            )
            .await?
        }
    };

    // Recorded only after the relay has had them, and only as far as it got.
    // The note itself is not touched: the editor owns it while it is open, and
    // writing behind its back loses whatever it saves next.
    let store = state.note(&note_id)?;
    let store = store.lock().unwrap();
    for (pending, element_id) in waiting.iter().zip(posted.sent.iter()) {
        store.remember_room_stroke(&pending.stroke_id, element_id, &pending.layer_id)?;
    }
    for stroke_id in &posted.removed {
        store.forget_room_stroke(stroke_id)?;
    }
    for (pending, (unit_id, element_id, layer_id)) in
        unit_added.iter().zip(posted_units.added.iter())
    {
        store.remember_room_unit(
            unit_id,
            element_id,
            layer_id,
            &collabo::send::fingerprint(&pending.models),
        )?;
    }
    for (pending, (unit_id, layer_id)) in unit_changed.iter().zip(posted_units.changed.iter()) {
        // An update mints no new element id; keep whatever the room already
        // knows this unit by.
        let element_id = unit_ledger
            .iter()
            .find(|l| &l.unit_id == unit_id)
            .map(|l| l.element_id.clone())
            .unwrap_or_else(|| unit_id.clone());
        store.remember_room_unit(
            unit_id,
            &element_id,
            layer_id,
            &collabo::send::fingerprint(&pending.models),
        )?;
    }
    for unit_id in &posted_units.removed {
        store.forget_room_unit(unit_id)?;
    }
    Ok(posted.sent.len()
        + posted.removed.len()
        + posted_units.added.len()
        + posted_units.changed.len()
        + posted_units.removed.len())
}

/// What a resync found, in words the user can act on.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resync {
    /// Strokes the room holds right now.
    pub in_room: usize,
    /// Ledger rows dropped because a post never landed; they go out again.
    pub never_arrived: usize,
    /// Ledger rows dropped because the room no longer holds them.
    pub erased_elsewhere: usize,
    /// Local strokes sent because the room did not have them.
    pub sent: usize,
    /// Elements the room does not hold. The note may still be showing them —
    /// something erased elsewhere, or a stroke that never arrived — and the
    /// editor takes them out.
    pub gone: Vec<String>,
    /// What it could not fix, said plainly.
    pub problems: Vec<String>,
}

/// Compares this note with its classroom and repairs the difference.
///
/// Sync gets stuck when the two sides disagree about what has been sent: a
/// post that never landed leaves a stroke the room will never see, and a row
/// for an element the room has dropped makes this app keep trying to erase
/// something that is already gone. Neither heals on its own, because both
/// sides think they are finished.
///
/// The room's own history settles it. An element it never saw means the post
/// was lost — send it again. One it saw and dropped means someone erased it —
/// forget it. Everything else stands.
#[tauri::command]
pub async fn classnote_resync(
    state: State<'_, AppState>,
    cloud: State<'_, CloudClient>,
    classroom: State<'_, ClassroomState>,
    drive: State<'_, DriveClient>,
    note_id: String,
) -> AppResult<Resync> {
    let mut report = Resync::default();

    let Some(origin) = state.catalog.lock().unwrap().class_origin(&note_id)? else {
        report
            .problems
            .push("このノートはクラスの写しではありません".into());
        return Ok(report);
    };
    let Some(room_id) = origin.room_id else {
        report
            .problems
            .push("このノートには教室がありません。クラスボックスから開き直してください".into());
        return Ok(report);
    };

    let store = state.note(&note_id)?;
    let (tree, ledger) = {
        let held = store.lock().unwrap();
        (held.read_tree()?, held.room_strokes()?)
    };

    // Read the room from the beginning. Expensive, and the point: the marks
    // are exactly what cannot be trusted when the two sides have drifted.
    let mut scratch = tree.clone();
    let (pull, _) =
        collabo::pull::fetch(&cloud, &classroom, &room_id, &mut scratch, &ledger).await?;
    let room = collabo::pull::survey(&pull.history);
    report.in_room = room.added.difference(&room.removed).count();

    // Repair the ledger against what the room actually holds.
    {
        let held = store.lock().unwrap();
        // And forget where each booth was read to. A mark that has run ahead
        // of reality is the other way sync gets stuck: nothing after it is
        // ever asked for again.
        held.forget_booth_marks()?;
        for entry in &ledger {
            if room.holds(&entry.element_id) {
                continue;
            }
            if room.added.contains(&entry.element_id) {
                report.erased_elsewhere += 1;
            } else {
                report.never_arrived += 1;
            }
            held.forget_room_stroke(&entry.stroke_id)?;
            // The note is probably still showing it. Whether it was erased
            // elsewhere or never arrived, the room does not have it, and the
            // room is what this is syncing to.
            report.gone.push(entry.stroke_id.clone());
        }
    }
    // Anything the room saw and dropped, whether or not this note ever knew
    // about it: an erasure made on the other device has to land here too.
    for id in &room.removed {
        if !report.gone.contains(id) {
            report.gone.push(id.clone());
        }
    }

    // With the ledger honest, the ordinary send finds what is missing — but
    // never sends back something the room already has under that id. A stroke
    // that arrived from the room carries the room's own id, and pushing it
    // again is how two devices end up trading one stroke for ever.
    let sent = send_missing(&state, &cloud, &classroom, &drive, &note_id, &room).await;
    match sent {
        Ok(count) => report.sent = count,
        Err(err) => report.problems.push(err.to_string()),
    }
    Ok(report)
}

/// Sends what the room has never seen, and only that.
async fn send_missing(
    state: &State<'_, AppState>,
    cloud: &State<'_, CloudClient>,
    classroom: &State<'_, ClassroomState>,
    drive: &State<'_, DriveClient>,
    note_id: &str,
    room: &collabo::pull::RoomState,
) -> AppResult<usize> {
    let (tree, ledger) = {
        let store = state.note(note_id)?;
        let held = store.lock().unwrap();
        (held.read_tree()?, held.room_strokes()?)
    };
    let (waiting, _, _) = collabo::send::changes(&tree, &ledger);
    // The guard: a stroke whose id the room already knows came *from* the
    // room, whatever the ledger says about it.
    if waiting.iter().any(|p| room.added.contains(&p.stroke_id)) {
        let store = state.note(note_id)?;
        let held = store.lock().unwrap();
        for pending in waiting.iter().filter(|p| room.added.contains(&p.stroke_id)) {
            held.remember_room_stroke(&pending.stroke_id, &pending.stroke_id, &pending.layer_id)?;
        }
    }

    classbox_send_strokes(
        state.clone(),
        cloud.clone(),
        classroom.clone(),
        drive.clone(),
        note_id.to_string(),
    )
    .await
}

/// Watches the classroom of an open note, so edits made elsewhere show up here
/// as they happen.
///
/// Not polling: the relay pushes. `AttachBooth` says where to resume and
/// everything after that arrives on the connection, which is both cheaper and
/// more immediate than asking again on a timer would be.
#[tauri::command]
pub async fn classnote_watch(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    cloud: State<'_, CloudClient>,
    classroom: State<'_, ClassroomState>,
    note_id: String,
) -> AppResult<bool> {
    let Some(origin) = state.catalog.lock().unwrap().class_origin(&note_id)? else {
        return Ok(false);
    };
    let Some(room_id) = origin.room_id else {
        return Ok(false);
    };

    let store = state.note(&note_id)?;
    let (tree, marks) = {
        let held = store.lock().unwrap();
        (held.read_tree()?, held.booth_marks()?)
    };
    let Some(session) = cloud.session() else {
        return Ok(false);
    };
    let booths = collabo::pull::booths_for(&tree, &session.user_id);
    if booths.is_empty() {
        return Ok(false);
    }

    let watch = collabo::watch::start(
        &app,
        &cloud,
        &classroom,
        &room_id,
        &note_id,
        &booths,
        &marks.into_iter().collect(),
        store,
    )
    .await?;
    classroom.set_watch(Some(watch)).await;
    Ok(true)
}

#[tauri::command]
pub async fn classnote_unwatch(classroom: State<'_, ClassroomState>) -> AppResult<()> {
    classroom.set_watch(None).await;
    Ok(())
}

/// Runs one HTTP request for the TypeScript client.
///
/// See `transport.rs`: it goes out through this app's own client so the
/// webview and Rust share one session, and so the request has no origin for
/// the server to refuse.
#[tauri::command]
pub async fn metamoji_fetch(
    cloud: State<'_, CloudClient>,
    request: crate::transport::FetchRequest,
) -> AppResult<crate::transport::FetchResponse> {
    crate::transport::fetch(&cloud, request).await
}

/// Records that a note is a copy of a class-box document.
#[tauri::command]
pub fn classbox_link(
    state: State<'_, AppState>,
    note_id: String,
    drive_id: String,
    document_id: String,
    room_id: Option<String>,
) -> AppResult<()> {
    state.catalog.lock().unwrap().set_class_origin(
        &note_id,
        &drive_id,
        &document_id,
        room_id.as_deref(),
    )
}

/// Where a note came from, when it came from a class box.
#[tauri::command]
pub fn classbox_origin(
    state: State<'_, AppState>,
    note_id: String,
) -> AppResult<Option<crate::storage::ClassOrigin>> {
    state.catalog.lock().unwrap().class_origin(&note_id)
}

/// The room a class-box document belongs to, if it has one.
///
/// The drive's own record is the authority here; the note carries a room id in
/// its share settings too, but that one is whatever was true when the file was
/// last written.
async fn room_id_of(
    drive: &DriveClient,
    drive_id: &str,
    document_id: &str,
) -> AppResult<Option<String>> {
    let envelope = drive.document_meta(drive_id, document_id).await?;
    Ok(envelope
        .get("meta")
        .and_then(|meta| meta.get("roomId"))
        .and_then(|v| v.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string))
}

/// Writes the binaries an import lifted out of the model tree into the note's
/// own asset store, under the tickets the models now reference.
pub(crate) fn store_imported_assets(
    state: &AppState,
    note_id: &str,
    assets: Vec<crate::atdoc::ImportedAsset>,
) -> AppResult<()> {
    if assets.is_empty() {
        return Ok(());
    }
    let store = state.note(note_id)?;
    let store = store.lock().unwrap();
    for asset in assets {
        store.put_asset(&asset.ticket, &asset.mime, &asset.bytes)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Class box folder download
// ---------------------------------------------------------------------------
//
// Issue #10: a teacher wants a folder's handouts on disk, without opening each
// note by hand. Scoped deliberately small — "no writing, background only" is
// the issue's own words — so this does not reuse `classbox_open_note`: that
// one also pulls the classroom's room history, which is exactly the writing
// this is not supposed to bring along. What it shares with that command is the
// one step that matters here too: importing the `.atdoc` container lifts the
// embedded PDF/image out of the model tree (see `atdoc::assets`), and the tree
// itself — strokes included — is discarded without ever being written.

/// What downloading a folder found. A note with no background is `skipped`,
/// not `failed`: a blank page or a hand-drawn-only note is not an error.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderDownloadResult {
    /// Notes under the folder, subfolders included.
    pub total: usize,
    pub saved: Vec<SavedBackground>,
    pub skipped: Vec<SkippedNote>,
    pub failed: Vec<FailedNote>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedBackground {
    pub document_id: String,
    pub title: Option<String>,
    /// File names written, relative to the folder the user chose.
    pub files: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedNote {
    pub document_id: String,
    pub title: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedNote {
    pub document_id: String,
    pub title: Option<String>,
    pub error: String,
}

const DOWNLOAD_PROGRESS_EVENT: &str = "classbox://download-progress";

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    done: usize,
    total: usize,
    /// The note about to be downloaded, or `None` once the last one is done.
    title: Option<String>,
}

/// Downloads the background (PDF or image) of every note under a class-box
/// folder, subfolders included, into `dest_dir`.
///
/// No handwriting comes along — not the student's own, not the room's — only
/// the material the note was built on. Subfolders are mirrored under
/// `dest_dir` so notes from two different folders never collide; one note
/// failing does not stop the rest, and everything that happened is in the
/// returned report rather than in a half-finished download the user has to
/// guess about.
#[tauri::command]
pub async fn classbox_download_folder(
    app: tauri::AppHandle,
    cloud: State<'_, CloudClient>,
    drive: State<'_, DriveClient>,
    drive_id: String,
    folder_path: String,
    dest_dir: String,
) -> AppResult<FolderDownloadResult> {
    let listing = open_drive_listing(&cloud, &drive, &drive_id).await?;
    // `folder_path` and every `folder_path` on a document are both
    // `drive::listing::normalise_path` output — slash-delimited, a slash at
    // each end — so a plain prefix match is also an exact subtree match: a
    // folder name can't contain the `/` that bounds it.
    let documents: Vec<DriveDocument> = listing
        .documents
        .into_iter()
        .filter(|d| d.folder_path.starts_with(&folder_path))
        .collect();

    let total = documents.len();
    let mut result = FolderDownloadResult {
        total,
        ..Default::default()
    };
    // Keyed by destination directory rather than one set for the whole
    // download: two notes of the same name in two different subfolders are no
    // collision at all.
    let mut used: HashMap<PathBuf, HashSet<String>> = HashMap::new();

    for (done, doc) in documents.into_iter().enumerate() {
        let _ = app.emit(
            DOWNLOAD_PROGRESS_EVENT,
            &DownloadProgress {
                done,
                total,
                title: doc.title.clone(),
            },
        );

        let document_id = doc.document_id.clone();
        let title = doc.title.clone();
        match download_note_background(&drive, &drive_id, &doc, &folder_path, &dest_dir, &mut used)
            .await
        {
            Ok(Some(files)) => result.saved.push(SavedBackground {
                document_id,
                title,
                files,
            }),
            Ok(None) => result.skipped.push(SkippedNote {
                document_id,
                title,
                reason: "背景(PDF・画像)がありません".to_string(),
            }),
            Err(err) => result.failed.push(FailedNote {
                document_id,
                title,
                error: err.to_string(),
            }),
        }
    }

    let _ = app.emit(
        DOWNLOAD_PROGRESS_EVENT,
        &DownloadProgress {
            done: total,
            total,
            title: None,
        },
    );
    Ok(result)
}

/// One note's background, saved under `dest_dir`. `Ok(None)` means the note
/// has none to save, which is not a failure.
async fn download_note_background(
    drive: &DriveClient,
    drive_id: &str,
    doc: &DriveDocument,
    folder_path: &str,
    dest_dir: &str,
    used: &mut HashMap<PathBuf, HashSet<String>>,
) -> AppResult<Option<Vec<String>>> {
    let file = drive
        .document_data(drive_id, &doc.document_id, doc.revision.as_deref())
        .await?;
    // The tree this builds is thrown away unwritten — strokes included. Only
    // the assets lifted out of it are kept, which is the same lift every
    // `.atdoc` import already does (`atdoc::assets`).
    let imported = crate::atdoc::import(file.bytes, &doc.document_id)?;
    let backgrounds: Vec<_> = imported.assets.into_iter().filter(|a| a.named).collect();
    if backgrounds.is_empty() {
        return Ok(None);
    }

    let dir = sub_directory(dest_dir, folder_path, &doc.folder_path);
    std::fs::create_dir_all(&dir)?;
    let used_here = used.entry(dir.clone()).or_default();

    let title = doc.title.as_deref().unwrap_or(&doc.document_id);
    let base = sanitise_filename(title);

    let mut files = Vec::new();
    for (index, asset) in backgrounds.iter().enumerate() {
        let stem = if backgrounds.len() > 1 {
            format!("{base}_{}", index + 1)
        } else {
            base.clone()
        };
        let name = unique_filename(used_here, &dir, &stem, extension_for(&asset.mime));
        std::fs::write(dir.join(&name), &asset.bytes)?;
        used_here.insert(name.clone());
        files.push(name);
    }
    Ok(Some(files))
}

/// Where one note's files land: `dest_dir`, plus whatever the note's own
/// folder adds beyond the folder the user chose to download — the same
/// hierarchy `drive::listing` already reads, carried onto disk.
fn sub_directory(dest_dir: &str, folder_path: &str, doc_folder_path: &str) -> PathBuf {
    let rest = doc_folder_path
        .strip_prefix(folder_path)
        .unwrap_or(doc_folder_path)
        .trim_matches('/');
    if rest.is_empty() {
        PathBuf::from(dest_dir)
    } else {
        PathBuf::from(dest_dir).join(rest)
    }
}

/// Characters no mainstream filesystem accepts in a file name, plus the ones
/// only Windows refuses — this build runs on both.
const INVALID_FILENAME_CHARS: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

fn sanitise_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || INVALID_FILENAME_CHARS.contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "note".to_string()
    } else {
        trimmed.to_string()
    }
}

fn extension_for(mime: &str) -> &'static str {
    match mime {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        _ => "bin",
    }
}

/// `{stem}.{ext}`, or `{stem} (2).{ext}`, … — whichever name neither this run
/// nor the directory itself already holds.
fn unique_filename(used: &HashSet<String>, dir: &Path, stem: &str, ext: &str) -> String {
    let mut candidate = format!("{stem}.{ext}");
    let mut n = 2;
    while used.contains(&candidate) || dir.join(&candidate).exists() {
        candidate = format!("{stem} ({n}).{ext}");
        n += 1;
    }
    candidate
}

#[tauri::command]
pub async fn classbox_note_thumbnail(
    drive: State<'_, DriveClient>,
    drive_id: String,
    document_id: String,
    revision: Option<String>,
) -> AppResult<String> {
    let file = drive
        .document_thumbnail(&drive_id, &document_id, revision.as_deref())
        .await?;
    let mime = file.mime_type.unwrap_or_else(|| "image/png".to_string());
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&file.bytes)
    ))
}

#[tauri::command]
pub fn classbox_close(drive: State<'_, DriveClient>) {
    drive.sign_out();
}

#[cfg(test)]
mod folder_download_tests {
    use super::*;

    #[test]
    fn a_title_with_path_characters_becomes_a_plain_file_name() {
        assert_eq!(sanitise_filename("配布/資料?.pdf"), "配布 資料 .pdf");
    }

    #[test]
    fn a_title_that_sanitises_to_nothing_falls_back() {
        assert_eq!(sanitise_filename("   "), "note");
        assert_eq!(sanitise_filename("///"), "note");
    }

    #[test]
    fn mime_types_the_format_actually_embeds_get_their_usual_extension() {
        assert_eq!(extension_for("application/pdf"), "pdf");
        assert_eq!(extension_for("image/png"), "png");
        assert_eq!(extension_for("image/jpeg"), "jpg");
        assert_eq!(extension_for("application/octet-stream"), "bin");
    }

    #[test]
    fn a_name_already_used_this_run_gets_a_counter() {
        let mut used = HashSet::new();
        used.insert("配布.pdf".to_string());
        let dir = std::env::temp_dir().join("metamoji-download-test-nonexistent");
        assert_eq!(unique_filename(&used, &dir, "配布", "pdf"), "配布 (2).pdf");
    }

    #[test]
    fn a_note_in_the_chosen_folder_itself_lands_directly_in_dest_dir() {
        assert_eq!(
            sub_directory("/tmp/out", "/算数/", "/算数/"),
            PathBuf::from("/tmp/out")
        );
    }

    #[test]
    fn a_note_in_a_subfolder_is_mirrored_under_dest_dir() {
        assert_eq!(
            sub_directory("/tmp/out", "/算数/", "/算数/4月/"),
            PathBuf::from("/tmp/out/4月")
        );
    }

    #[test]
    fn downloading_from_the_root_mirrors_the_whole_tree() {
        assert_eq!(
            sub_directory("/tmp/out", "/", "/算数/4月/"),
            PathBuf::from("/tmp/out/算数/4月")
        );
    }
}
