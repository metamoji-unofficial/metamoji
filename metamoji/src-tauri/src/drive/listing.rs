//! Reading the drive-contents payload.
//!
//! `GET /rest/drives/{id}/data` answers with a **zip of JSON files**, not a
//! MetaMoJi model container. `SdDriveSyncProcess.doMergeDriveDataFromArchive`
//! opens it as a `ZipFile` and dispatches each entry by the prefix of its name:
//!
//! | entry | contents |
//! |---|---|
//! | `documents_N.json`      | notes — `id`, `title`, `contentsRevision`, … |
//! | `folderdefs_N.json`     | folders — `absPath`, `childrenOrder`, … |
//! | `childrenorders_N.json` | ordering |
//! | `tagdefs_N.json`, `tagorder.json` | tags |
//! | `drive.json`, `meta.json` | drive properties |
//!
//! (An earlier version ran the whole payload through the `.atdoc` importer on
//! the assumption that everything in this app is a model container. It is not,
//! and the symptom was "archive contains no MetaMoJi document", which reads
//! like a corrupt download rather than a wrong guess.)
//!
//! **Folders are paths, and membership is tags.** `SdMOFolder` is keyed by
//! `absPath`, so the hierarchy is in the strings and needs no joining. A
//! *document* names its folder in its own `tags` field, which is the same
//! `/`-delimited path — hence `SdUtils.tagsFromPath`, which the document parser
//! calls on it (`SdDriveSyncProcess$19$1`). Folders carry `tags` too, and a
//! `parentAbsPath` besides.
//!
//! `childrenOrder` is **ordering, not membership**, and it lives in its own
//! `childrenorders_` entries rather than on the folder records
//! (`SdDriveSyncProcess$18$1`). Reading membership from it put every note at
//! the top level, because the field it was read from is empty on the way down.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use serde_json::Value;

use crate::error::{AppError, AppResult};

use super::{DriveDocument, DriveFolder};

/// What a drive listing turned into.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub documents: Vec<DriveDocument>,
    pub folders: Vec<DriveFolder>,
    /// Entries in the archive that were not understood. Empty is the good case;
    /// anything here means the archive holds more than this decoder knows.
    pub unrecognised: Vec<String>,
    /// Records seen, deleted ones included — a listing that parsed but yielded
    /// nothing is a different problem from one that failed to parse.
    pub record_count: usize,
}

/// Entry prefixes this decoder acts on. The rest of the archive is real data
/// the app does not use yet, not corruption, so it is counted rather than
/// reported as a fault.
const DOCUMENTS: &str = "documents_";
const FOLDERS: &str = "folderdefs_";
const ORDERS: &str = "childrenorders_";
const KNOWN_BUT_UNUSED: [&str; 4] = ["tagdefs_", "tagorder.json", "drive.json", "meta.json"];

/// Caps how much of one entry's decompressed text this reads. Real entries
/// (`documents_N.json` etc.) are a few KB to a few hundred KB; a sync payload
/// claiming to need more than this is a decompression bomb, not real data.
const MAX_ENTRY_TEXT_LEN: u64 = 32 * 1024 * 1024;

/// Decodes the zip a drive hands back.
pub fn parse(bytes: Vec<u8>) -> AppResult<Listing> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| AppError::Unsupported(format!("クラスボックスの中身を開けません: {e}")))?;

    let mut documents: Vec<DriveDocument> = Vec::new();
    let mut folders: Vec<DriveFolder> = Vec::new();
    // Folder path → the drive's own arrangement of its children.
    let mut orders: HashMap<String, Vec<String>> = HashMap::new();
    let mut unrecognised = Vec::new();
    let mut record_count = 0usize;

    for index in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(index) else {
            continue;
        };
        let full = entry.name().to_string();
        if full.ends_with('/') {
            continue;
        }
        // Entries may be nested; the dispatch is on the file name alone.
        let name = full.rsplit('/').next().unwrap_or(&full).to_string();

        let mut text = String::new();
        if entry
            .by_ref()
            .take(MAX_ENTRY_TEXT_LEN)
            .read_to_string(&mut text)
            .is_err()
        {
            unrecognised.push(name);
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            unrecognised.push(name);
            continue;
        };

        if name.starts_with(DOCUMENTS) {
            record_count += collect_documents(&value, &mut documents);
        } else if name.starts_with(FOLDERS) {
            record_count += collect_folders(&value, &mut folders);
        } else if name.starts_with(ORDERS) {
            record_count += collect_orders(&value, &mut orders);
        } else if !KNOWN_BUT_UNUSED.iter().any(|k| name.starts_with(k)) {
            unrecognised.push(name);
        }
    }

    // The drive's own arrangement first; anything it does not mention falls to
    // the end in title order, which is at least stable.
    let rank = |path: &str, id: &str| {
        orders
            .get(path)
            .and_then(|list| list.iter().position(|child| child == id))
            .unwrap_or(usize::MAX)
    };
    // Each folder is ranked within *its own* parent. Ranking both sides against
    // `a`'s parent reads the right way round but is not a total order — swap the
    // arguments and the answer changes — and Rust's sort detects that and
    // panics, taking the whole class box down with it.
    folders.sort_by(|a, b| {
        let rank_of = |f: &DriveFolder| rank(f.parent_path.as_deref().unwrap_or("/"), &f.name);
        rank_of(a)
            .cmp(&rank_of(b))
            .then_with(|| a.abs_path.cmp(&b.abs_path))
    });
    documents.sort_by(|a, b| {
        rank(&a.folder_path, &a.document_id)
            .cmp(&rank(&b.folder_path, &b.document_id))
            .then_with(|| {
                a.title
                    .as_deref()
                    .unwrap_or(&a.document_id)
                    .cmp(b.title.as_deref().unwrap_or(&b.document_id))
            })
    });
    unrecognised.sort();
    unrecognised.dedup();

    Ok(Listing {
        documents,
        folders,
        unrecognised,
        record_count,
    })
}

fn collect_documents(value: &Value, out: &mut Vec<DriveDocument>) -> usize {
    let Some(records) = records(value) else {
        return 0;
    };

    let mut seen = 0;
    for record in records {
        let Some(object) = record.as_object() else {
            continue;
        };
        let Some(id) = text(object.get("id")) else {
            continue;
        };
        seen += 1;
        // Deleted records stay in the archive; listing them would offer notes
        // that cannot be opened.
        if deleted(object) {
            continue;
        }
        out.push(DriveDocument {
            document_id: id,
            title: text(object.get("title")),
            revision: text(object.get("contentsRevision")).or_else(|| text(object.get("revision"))),
            updated_at: text(object.get("contentsUpdate"))
                .or_else(|| text(object.get("lastUpdate"))),
            // A note names its own folder. `SdDriveSyncProcess$19$1` reads this
            // as a string and hands it to `SdUtils.tagsFromPath`, so it is the
            // folder's path in the same `/`-delimited form.
            folder_path: object
                .get("tags")
                .and_then(Value::as_str)
                .map(normalise_path)
                // A note with no tags is at the top level, which is also where
                // one whose folder is missing belongs — better than hidden.
                .unwrap_or_else(|| "/".to_string()),
        });
    }
    seen
}

fn collect_folders(value: &Value, out: &mut Vec<DriveFolder>) -> usize {
    let Some(records) = records(value) else {
        return 0;
    };

    let mut seen = 0;
    for record in records {
        let Some(object) = record.as_object() else {
            continue;
        };
        // Not `text`: an empty `absPath` is the root, not a missing value.
        let Some(raw_path) = object.get("absPath").and_then(Value::as_str) else {
            continue;
        };
        seen += 1;

        let path = normalise_path(raw_path);
        // The root is where everything already is; a row for it would be a loop.
        if deleted(object) || path == "/" {
            continue;
        }

        let name = path
            .trim_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        out.push(DriveFolder {
            name: name.to_string(),
            // Given outright, so there is no need to infer it — and no risk of
            // inferring it differently from the server.
            parent_path: object
                .get("parentAbsPath")
                .and_then(Value::as_str)
                .map(normalise_path)
                .or_else(|| parent_of(&path)),
            depth: path.trim_matches('/').split('/').count(),
            abs_path: path,
        });
    }
    seen
}

/// `childrenorders_N.json`: the drive's own arrangement, one record per folder.
///
/// Ordering only. Membership is the document's `tags`; reading it from here
/// would put every note at the top level, since these records name children in
/// a form that does not always match a document id.
fn collect_orders(value: &Value, out: &mut HashMap<String, Vec<String>>) -> usize {
    let Some(records) = records(value) else {
        return 0;
    };

    let mut seen = 0;
    for record in records {
        let Some(object) = record.as_object() else {
            continue;
        };
        let Some(raw_path) = object.get("absPath").and_then(Value::as_str) else {
            continue;
        };
        seen += 1;
        out.insert(
            normalise_path(raw_path),
            split_children(text(object.get("childrenOrder")).as_deref()),
        );
    }
    seen
}

/// The array a `*_N.json` entry holds, whether at the top level or wrapped.
fn records(value: &Value) -> Option<Vec<&Value>> {
    match value {
        Value::Array(items) => Some(items.iter().collect()),
        Value::Object(map) => map
            .values()
            .find_map(Value::as_array)
            .map(|items| items.iter().collect()),
        _ => None,
    }
}

fn deleted(record: &serde_json::Map<String, Value>) -> bool {
    record
        .get("deleteFlag")
        .map(|v| v.as_bool() == Some(true) || v.as_i64() == Some(1) || v.as_str() == Some("true"))
        .unwrap_or(false)
}

/// `SdUtils.tagsFromPath`: drop the outer slashes, split on the rest.
fn split_children(value: Option<&str>) -> Vec<String> {
    value
        .map(|text| {
            text.trim_matches('/')
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A folder path with exactly one slash at each end.
///
/// `.`/`..` segments are dropped outright: this is a display hierarchy coming
/// straight off a document's `tags` field (sync data, not trusted input), and
/// it later gets joined onto a real filesystem path in
/// `commands::sub_directory`, so a traversal segment surviving here would
/// become one there.
fn normalise_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');
    let cleaned: Vec<&str> = trimmed
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != "." && *seg != "..")
        .collect();
    if cleaned.is_empty() {
        "/".to_string()
    } else {
        format!("/{}/", cleaned.join("/"))
    }
}

fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_matches('/');
    let (parent, _) = trimmed.rsplit_once('/')?;
    Some(normalise_path(parent))
}

/// A scalar as a string. This API is not consistent about types — revisions and
/// dates arrive as numbers in some records and strings in others.
fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::String(_) => None,
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[path = "listing/tests.rs"]
#[cfg(test)]
mod tests;
