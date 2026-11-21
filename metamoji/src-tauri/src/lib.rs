pub mod atdoc;
pub mod cloud;
#[cfg(test)]
mod cloud_wire_tests;
pub mod collabo;
mod commands;
pub mod drive;
#[cfg(test)]
#[path = "drive/wire_tests.rs"]
mod drive_wire_tests;
mod error;
pub mod export;
pub mod model;
mod state;
pub mod storage;
#[cfg(test)]
mod test_support;
pub mod transport;

use tauri::{Emitter, Manager};

use crate::atdoc::ImportReport;
use crate::error::{AppError, AppResult};
use crate::model::GenericTree;
use crate::state::AppState;

#[derive(serde::Serialize)]
pub struct AtdocImportResult {
    pub tree: GenericTree,
    pub report: ImportReport,
    /// The document's own title, when it has one. The caller decides whether to
    /// use it — a class-box record's title is fresher, a file name is not.
    pub title: Option<String>,
    /// What the note's classroom room added, for a note that has one.
    pub room: Option<collabo::pull::RoomPull>,
    /// The room itself, so the caller can remember it with the note.
    #[serde(rename = "roomId")]
    pub room_id: Option<String>,
}

/// Imports a `.atdoc` file the user picked in a dialog.
///
/// See `atdoc/mod.rs` for the fidelity breakdown. Structure, properties and
/// handwriting all come across; per-point pressure and rich-text bodies do not.
/// The returned report states exactly what was recovered rather than leaving the
/// user to guess from a half-populated note.
#[tauri::command]
fn atdoc_import(
    state: tauri::State<'_, AppState>,
    path: String,
    new_root_id: String,
) -> AppResult<AtdocImportResult> {
    let bytes = std::fs::read(&path).map_err(AppError::Io)?;
    let result = atdoc::import(bytes, &new_root_id)?;
    commands::store_imported_assets(&state, &new_root_id, result.assets)?;
    Ok(AtdocImportResult {
        tree: result.tree,
        report: result.report,
        title: result.title,
        // A file on disk belongs to no room.
        room: None,
        room_id: None,
    })
}

/// Reports whether a file looks like something we can import, without reading
/// all of it — used to give the file picker honest feedback.
#[tauri::command]
fn atdoc_probe(path: String) -> AppResult<u16> {
    // The file may be a ZIP wrapping the container, so this reads the whole
    // thing rather than sniffing a fixed-size head.
    let bytes = std::fs::read(&path)?;
    let raw = atdoc::unwrap_container(bytes)?;
    Ok(atdoc::parse_header(&raw)?.format_version)
}

/// Writes the note out as a PDF. Pages arrive already rendered, so the export
/// matches the screen exactly — see `export.rs` for why that trade was made.
#[tauri::command]
fn export_pdf(path: String, title: String, pages: Vec<export::ExportPage>) -> AppResult<()> {
    export::write_pdf(&path, &title, &pages)
}

/// The environment values the ClassShare protocol wants on every request.
/// Best-effort: unlike `productName`/`productVersion`, the server treats these
/// as telemetry, so a wrong guess costs nothing and an error would cost a
/// sign-in.
///
/// `deviceName` is also where this client owns up to what it is. The protocol
/// constants have to claim to be the Android app or the server refuses the
/// login, so this is the one field a school administrator can look at and see
/// a desktop client rather than a phone.
fn device_name() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "desktop".to_string());
    format!("{host} (MetaMoJi Desktop {})", env!("CARGO_PKG_VERSION"))
}

/// A stable per-install identity for the collabo relay.
///
/// The *code* is ours to invent — `NsCollaboBgTaskForCreateUniqueID` uses a
/// random positive `int` — and is written next to the library rather than
/// derived from the machine, so a reinstall is a new device rather than the
/// same one. The *id* is not ours: the service issues it in exchange for the
/// code, and an invented one is refused. It is fetched on first use and saved
/// here beside the code.
fn device_identity(data_dir: &std::path::Path) -> (String, Option<String>, std::path::PathBuf) {
    let path = data_dir.join("device.json");
    let saved = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let get = |key: &str| {
        saved
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    // A code the service will accept is a positive `int` as decimal digits —
    // `NsCollaboBgTaskForCreateUniqueID` makes one with `Random.nextInt()`.
    // Anything else was invented by an older build of this app, which also
    // invented the id that goes with it, and the pair is refused. Dropping
    // both is what lets the next call register properly.
    let code = get("deviceCode").filter(|c| c.chars().all(|b| b.is_ascii_digit()));
    let id = code.as_ref().and_then(|_| get("deviceId"));
    let code = code.unwrap_or_else(|| {
        let bytes = uuid::Uuid::new_v4().as_u128() as u32 & 0x7fff_ffff;
        bytes.to_string()
    });
    (code, id, path)
}

fn locale() -> String {
    std::env::var("LANG")
        .ok()
        .and_then(|lang| lang.split('.').next().map(str::to_string))
        .filter(|s| !s.is_empty() && s != "C" && s != "POSIX")
        .unwrap_or_else(|| "ja_JP".to_string())
}

/// `X-DM-Device`, which `SdHttpClient` formats as
/// `"{Build.MODEL};Android;{SDK_INT}"` — three `;`-separated fields, the last
/// an integer.
///
/// There is no honest Android answer from a desktop, so this says what the
/// machine is. It keeps the *shape* though: an empty third field is the kind of
/// thing a server-side `parseInt` turns into a 500.
fn device_header() -> String {
    format!("Desktop;{};1", std::env::consts::OS)
}

fn timezone() -> String {
    // `/etc/localtime` is a symlink into the zoneinfo tree on macOS and Linux;
    // its tail is the IANA name. Windows has no equivalent, hence the fallback.
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|path| {
            let path = path.to_string_lossy().to_string();
            path.split_once("zoneinfo/").map(|(_, tz)| tz.to_string())
        })
        .unwrap_or_else(|| "Asia/Tokyo".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // Boot in one explicit sequence rather than a chain of static
            // singletons, per docs/14 §4's recommendation.
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let state_dir = data_dir.clone();

            let state = AppState::boot(data_dir)?;
            let status = state.status();
            app.manage(state);

            // The cloud client is separate from `AppState` on purpose: it holds
            // a network session, not application data, and nothing in the
            // library needs it.
            app.manage(cloud::CloudClient::new(
                device_name(),
                locale(),
                timezone(),
                Some(state_dir.join("session.json")),
            )?);

            // The relay recognises a returning device by these, so they are
            // generated once and kept — the original stores the same pair
            // under `CollaboDeviceId` / `CollaboDeviceCode`.
            let (device_code, device_id, device_path) = device_identity(&state_dir);
            app.manage(collabo::session::ClassroomState::new(
                device_code,
                device_id,
                Some(device_path),
            ));

            // The drive service keeps its own session, so it gets its own
            // client rather than a few more methods on the cloud one.
            app.manage(drive::DriveClient::new(locale(), device_header())?);

            // The frontend gates on this exactly as the original gated on
            // `StartupViewModel.isNeedLogin` (docs/14 §3).
            app.emit("app://ready", status)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_status,
            commands::library_list,
            commands::library_set_folder,
            commands::library_set_sync_state,
            commands::sync_drive_revision,
            commands::sync_set_drive_revision,
            commands::folder_list,
            commands::folder_create,
            commands::folder_rename,
            commands::folder_delete,
            commands::tag_list,
            commands::tag_create,
            commands::tag_delete,
            commands::tag_set_on_document,
            commands::library_create,
            commands::library_rename,
            commands::library_set_trashed,
            commands::library_delete,
            commands::library_duplicate,
            commands::note_load,
            commands::note_save,
            commands::note_close,
            commands::note_set_thumbnail,
            commands::asset_put,
            commands::asset_get,
            commands::asset_list,
            commands::pick_and_read_file,
            commands::pick_and_write_file,
            commands::cloud_root_server,
            commands::cloud_set_root_server,
            commands::cloud_resolve_school,
            commands::cloud_class_groups,
            commands::cloud_login,
            commands::cloud_classroom_login,
            commands::cloud_logout,
            commands::cloud_session,
            commands::classroom_create_room,
            commands::classroom_enter,
            commands::classroom_leave,
            commands::classroom_members,
            commands::classroom_attach_booth,
            commands::classroom_detach_booth,
            commands::classroom_current_room,
            commands::classbox_list,
            commands::classbox_open,
            commands::classbox_revision,
            commands::classbox_open_note,
            commands::classbox_download_folder,
            commands::classbox_send_strokes,
            commands::metamoji_fetch,
            commands::classbox_link,
            commands::classbox_origin,
            commands::classnote_watch,
            commands::classnote_resync,
            commands::classnote_unwatch,
            commands::classbox_note_thumbnail,
            commands::classbox_close,
            atdoc_import,
            atdoc_probe,
            export_pdf,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
