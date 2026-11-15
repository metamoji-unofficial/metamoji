//! Embedded binaries: out of the model tree, into the asset store.
//!
//! A MetaMoJi document carries its PDF backgrounds and page thumbnails inline,
//! as `$blob` properties on `attachment` / `pagethumbnail` models. That is fine
//! on disk and ruinous in memory: a nine-page PDF handout imports as a 1.5 MB
//! model tree, and every load of that note would carry the whole thing across
//! IPC as base64 for the sake of one property.
//!
//! So each blob is lifted out into an `ImportedAsset` and the property left
//! behind becomes a reference. Tickets are not invented where the document
//! already has one: a `$pdf` unit points at its PDF by the ticket in the
//! `attachments` map, and that string has to keep resolving after the import.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::model::GenericTree;

/// One binary lifted out of the model tree.
#[derive(Debug, Clone)]
pub struct ImportedAsset {
    pub ticket: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    /// Whether this ticket is the document's own — the first blob on a model
    /// the `attachments` map names, which is the PDF or image a page is drawn
    /// on. `false` is everything else this format also embeds, chiefly page
    /// thumbnails: real data, just not what "the background" means to a
    /// caller that wants the handout and nothing this app or a room added.
    pub named: bool,
}

const ATTACHMENTS_TYPE: &str = "attachments";

/// Moves every `$blob` into the returned assets, replacing it with a reference.
pub fn extract(tree: &mut GenericTree) -> Vec<ImportedAsset> {
    let tickets = attachment_tickets(tree);

    let mut assets: Vec<ImportedAsset> = Vec::new();
    let mut ids: Vec<String> = tree.models.keys().cloned().collect();
    // Deterministic order, so the same document always yields the same
    // synthesised tickets.
    ids.sort();

    for id in ids {
        let named = tickets.get(&id).cloned();
        let Some(model) = tree.models.get_mut(&id) else {
            continue;
        };
        let mut found: Vec<(String, ImportedAsset)> = Vec::new();
        collect(
            &mut model.props,
            &id,
            &named,
            &mut String::new(),
            &mut found,
        );
        assets.extend(found.into_iter().map(|(_, asset)| asset));
    }

    assets
}

/// `model id -> ticket`, read from every `attachments` map in the document.
///
/// The map is the document's own index — `{ticket: {$ref: model}}` — so it is
/// the only place the ticket a `$pdf` unit quotes is written down.
fn attachment_tickets(tree: &GenericTree) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for model in tree.models.values() {
        if model.model_type != ATTACHMENTS_TYPE {
            continue;
        }
        let Value::Object(map) = &model.props else {
            continue;
        };
        for (ticket, target) in map {
            if let Some(Value::String(id)) = target.get("$ref") {
                out.insert(id.clone(), ticket.clone());
            }
        }
    }
    out
}

/// Walks one model's properties, swapping blobs for references as it goes.
fn collect(
    value: &mut Value,
    model_id: &str,
    named: &Option<String>,
    path: &mut String,
    out: &mut Vec<(String, ImportedAsset)>,
) {
    match value {
        Value::Object(map) => {
            if let Some(asset) = take_blob(map, model_id, named, path, out.len()) {
                let reference = reference_to(&asset);
                out.push((asset.ticket.clone(), asset));
                *value = reference;
                return;
            }
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let mark = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(&key);
                if let Some(child) = map.get_mut(&key) {
                    collect(child, model_id, named, path, out);
                }
                path.truncate(mark);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                let mark = path.len();
                path.push_str(&format!("[{i}]"));
                collect(item, model_id, named, path, out);
                path.truncate(mark);
            }
        }
        _ => {}
    }
}

fn take_blob(
    map: &Map<String, Value>,
    model_id: &str,
    named: &Option<String>,
    path: &str,
    seen: usize,
) -> Option<ImportedAsset> {
    use base64::Engine as _;

    let Some(Value::String(payload)) = map.get("$blob") else {
        return None;
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;

    let mime = match map.get("$mime") {
        Some(Value::String(m)) if !m.is_empty() => m.clone(),
        _ => sniff(&bytes).to_string(),
    };

    // The document's own ticket for the first blob on an attachment model;
    // anything else gets one derived from where it was found, which is stable
    // across imports of the same file.
    let is_named = named.is_some() && seen == 0;
    let ticket = match named {
        Some(ticket) if seen == 0 => ticket.clone(),
        _ if path.is_empty() => model_id.to_string(),
        _ => format!("{model_id}:{path}"),
    };

    Some(ImportedAsset {
        ticket,
        mime,
        bytes,
        named: is_named,
    })
}

fn reference_to(asset: &ImportedAsset) -> Value {
    let mut map = Map::new();
    map.insert("$asset".into(), Value::String(asset.ticket.clone()));
    map.insert("$mime".into(), Value::String(asset.mime.clone()));
    map.insert("$size".into(), Value::from(asset.bytes.len()));
    Value::Object(map)
}

/// Enough of a sniff to name the three things a MetaMoJi document embeds.
fn sniff(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else {
        "application/octet-stream"
    }
}

/// Puts the binaries back, turning `{"$asset": …}` into the blob it replaced.
///
/// The inverse of `extract`, and the step that has to run before a document
/// this app imported can be written back out: the original has no notion of an
/// asset store, so a reference to one is a hole in the file.
pub fn restore_assets(
    tree: &mut GenericTree,
    assets: &HashMap<String, (String, Vec<u8>)>,
) -> Vec<String> {
    let mut missing = Vec::new();
    for model in tree.models.values_mut() {
        put_back(&mut model.props, assets, &mut missing);
    }
    missing.sort();
    missing.dedup();
    missing
}

fn put_back(
    value: &mut Value,
    assets: &HashMap<String, (String, Vec<u8>)>,
    missing: &mut Vec<String>,
) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(ticket)) = map.get("$asset") {
                let ticket = ticket.clone();
                match assets.get(&ticket) {
                    Some((mime, bytes)) => {
                        use base64::Engine as _;
                        let mut blob = Map::new();
                        blob.insert(
                            "$blob".into(),
                            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
                        );
                        blob.insert("$mime".into(), Value::String(mime.clone()));
                        *value = Value::Object(blob);
                    }
                    // Named rather than guessed at: a document written with a
                    // hole where its PDF was is worse than one not written.
                    None => missing.push(ticket),
                }
                return;
            }
            for child in map.values_mut() {
                put_back(child, assets, missing);
            }
        }
        Value::Array(items) => {
            for item in items {
                put_back(item, assets, missing);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GenericModel;
    use serde_json::json;

    fn tree_with(models: Vec<GenericModel>) -> GenericTree {
        let mut tree = GenericTree::new("root", "$freenote");
        for model in models {
            tree.insert(model);
        }
        tree
    }

    fn model(id: &str, model_type: &str, props: Value) -> GenericModel {
        GenericModel {
            id: id.into(),
            parent_id: Some("root".into()),
            model_type: model_type.into(),
            props,
            children: Vec::new(),
        }
    }

    #[test]
    fn an_attachment_keeps_the_ticket_the_document_quotes() {
        // `$pdf` units reference the PDF by this exact string; inventing our
        // own here would leave every one of them pointing at nothing.
        let mut tree = tree_with(vec![
            model(
                "root_m5",
                "attachments",
                json!({ "tkt-1": { "$ref": "root_m6" } }),
            ),
            model(
                "root_m6",
                "attachment",
                json!({ "$blob": { "$blob": "JVBERi0=", "$mime": "" } }),
            ),
        ]);

        let assets = extract(&mut tree);

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].ticket, "tkt-1");
        assert_eq!(assets[0].mime, "application/pdf");
        assert_eq!(assets[0].bytes, b"%PDF-");
        // This is the one bit a caller that wants only the background (no
        // thumbnails, no room writing) relies on to tell it apart.
        assert!(assets[0].named);
        assert_eq!(
            tree.models["root_m6"].props["$blob"],
            json!({ "$asset": "tkt-1", "$mime": "application/pdf", "$size": 5 })
        );
    }

    #[test]
    fn an_unnamed_blob_gets_a_ticket_from_where_it_was_found() {
        let mut tree = tree_with(vec![model(
            "root_m9",
            "pagethumbnail",
            json!({ "v": { "$blob": "iVBORw0KGgo=", "$mime": "image/png" } }),
        )]);

        let assets = extract(&mut tree);

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].ticket, "root_m9:v");
        assert_eq!(assets[0].mime, "image/png");
        // A page thumbnail, not a background — not in the `attachments` map,
        // so it never gets a document-given ticket.
        assert!(!assets[0].named);
        assert_eq!(
            tree.models["root_m9"].props["v"]["$asset"],
            json!("root_m9:v")
        );
    }

    #[test]
    fn a_blob_nested_in_an_array_is_found_too() {
        let mut tree = tree_with(vec![model(
            "root_m9",
            "map",
            json!({ "list": [{ "img": { "$blob": "AQID" } }] }),
        )]);

        let assets = extract(&mut tree);

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].ticket, "root_m9:list[0].img");
        assert_eq!(assets[0].mime, "application/octet-stream");
        assert_eq!(assets[0].bytes, vec![1, 2, 3]);
    }

    #[test]
    fn an_asset_reference_becomes_the_blob_it_replaced() {
        let mut tree = tree_with(vec![
            model(
                "root_m5",
                "attachments",
                json!({ "tkt-1": { "$ref": "root_m6" } }),
            ),
            model(
                "root_m6",
                "attachment",
                json!({ "$blob": { "$blob": "JVBERi0=", "$mime": "application/pdf" } }),
            ),
        ]);
        let assets = extract(&mut tree);
        let stored: HashMap<String, (String, Vec<u8>)> = assets
            .into_iter()
            .map(|a| (a.ticket, (a.mime, a.bytes)))
            .collect();

        assert!(restore_assets(&mut tree, &stored).is_empty());
        assert_eq!(
            tree.models["root_m6"].props["$blob"],
            json!({ "$blob": "JVBERi0=", "$mime": "application/pdf" })
        );
    }

    #[test]
    fn a_missing_asset_is_named_rather_than_written_as_a_hole() {
        let mut tree = tree_with(vec![model(
            "root_m6",
            "attachment",
            json!({ "$blob": { "$asset": "gone", "$mime": "application/pdf" } }),
        )]);
        assert_eq!(
            restore_assets(&mut tree, &HashMap::new()),
            vec!["gone".to_string()]
        );
        assert!(tree.models["root_m6"].props["$blob"]
            .get("$asset")
            .is_some());
    }

    #[test]
    fn a_model_without_blobs_is_left_alone() {
        let mut tree = tree_with(vec![model("root_m9", "docmeta", json!({ "title": "x" }))]);
        assert!(extract(&mut tree).is_empty());
        assert_eq!(tree.models["root_m9"].props, json!({ "title": "x" }));
    }
}
