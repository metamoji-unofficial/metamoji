//! Golden-corpus test for the `.atdoc` importer.
//!
//! The repository has no `.atdoc` sample, but the decompiled APK ships real
//! MetaMoJi documents in exactly the same container format: the start guide and
//! the bundled library products. Parsing those is a far stronger regression net
//! than synthetic input, and it costs nothing to keep.
//!
//! The corpus lives outside the crate, so every test here skips cleanly when
//! `apk/` is absent — a checkout without it must still pass `cargo test`.

use std::path::{Path, PathBuf};

use metamoji_lib::atdoc;

fn corpus_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .join("apk/assets/init");
    root.is_dir().then_some(root)
}

/// Every `.state` member inside the bundled archives, plus the archives
/// themselves where they wrap a document directly.
fn corpus_files() -> Vec<PathBuf> {
    let Some(root) = corpus_root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("library")) {
        out.extend(
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "product")),
        );
    }
    let guide = root.join("guide/startguide.dat");
    if guide.is_file() {
        out.push(guide);
    }
    out
}

#[test]
fn parses_every_bundled_document_without_error() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("skipping: apk/ corpus not present in this checkout");
        return;
    }

    let mut parsed = 0usize;
    let mut total_models = 0usize;

    for path in &files {
        let bytes = std::fs::read(path).expect("corpus file should be readable");
        // Not every archive member is a document; `unwrap_container` reports so
        // rather than panicking, and that is the behaviour under test.
        let Ok(raw) = atdoc::unwrap_container(bytes) else {
            continue;
        };
        let doc = atdoc::parse_document(&raw)
            .unwrap_or_else(|e| panic!("{} failed to parse: {e}", path.display()));

        assert!(
            (1..=3).contains(&doc.format_version),
            "{}: implausible format version {}",
            path.display(),
            doc.format_version
        );
        parsed += 1;
        total_models += doc.models.len();
    }

    assert!(
        parsed > 0,
        "corpus was present but yielded no parsable documents"
    );
    eprintln!("parsed {parsed} document(s), {total_models} models");
}

#[test]
fn the_start_guide_yields_pages_types_and_ink() {
    let Some(root) = corpus_root() else {
        eprintln!("skipping: apk/ corpus not present in this checkout");
        return;
    };
    let guide = root.join("guide/startguide.dat");
    if !guide.is_file() {
        eprintln!("skipping: start guide not present");
        return;
    }

    let bytes = std::fs::read(&guide).expect("start guide should be readable");
    let result = atdoc::import(bytes, "note_test").expect("start guide should import");

    // The start guide is a real multi-page handwritten document. If the type
    // dictionary or the value decoder regressed, these all collapse to zero.
    assert!(
        result.report.model_count > 100,
        "expected a substantial model count, got {}",
        result.report.model_count
    );
    assert!(
        result.report.page_count > 0,
        "expected at least one page, got {}",
        result.report.page_count
    );
    assert!(
        result.report.undecoded_models * 4 < result.report.model_count,
        "too many models failed to decode: {} of {}",
        result.report.undecoded_models,
        result.report.model_count
    );

    eprintln!(
        "start guide: v{} | {} models | {} pages | {} strokes | {} undecoded | types: {:?}",
        result.report.format_version,
        result.report.model_count,
        result.report.page_count,
        result.report.stroke_count,
        result.report.undecoded_models,
        {
            let mut kinds: Vec<_> = result.report.unit_counts.iter().collect();
            kinds.sort_by_key(|(k, _)| k.to_string());
            kinds
        }
    );

    // Every node must be reachable from the root, or the frontend converter
    // will silently drop whatever is stranded.
    for (id, model) in &result.tree.models {
        if id == &result.tree.root_id {
            continue;
        }
        assert!(
            model.parent_id.is_some(),
            "model {id} has no parent and would be dropped on import"
        );
    }
}

#[test]
fn malformed_input_is_rejected_rather_than_panicking() {
    assert!(atdoc::unwrap_container(b"not a document".to_vec()).is_err());
    assert!(atdoc::unwrap_container(Vec::new()).is_err());

    // A valid magic number followed by garbage must fail cleanly.
    let mut truncated = b"MMJDrawModels\x01".to_vec();
    truncated.extend_from_slice(&[0u8; 8]);
    assert!(atdoc::import(truncated, "note_test").is_err());
}

/// The full user flow: import a real document, persist it, reopen it.
///
/// The importer and the store are each tested on their own, but this is the
/// path that actually matters — an import that cannot be saved and reopened
/// with its strokes intact has not imported anything.
#[test]
fn an_imported_document_survives_being_saved_and_reopened() {
    use metamoji_lib::storage::{now_iso, AppPaths, NoteStore};

    let Some(root) = corpus_root() else {
        eprintln!("skipping: apk/ corpus not present in this checkout");
        return;
    };
    let guide = root.join("guide/startguide.dat");
    if !guide.is_file() {
        eprintln!("skipping: start guide not present");
        return;
    }

    let bytes = std::fs::read(&guide).expect("start guide should be readable");
    let imported = atdoc::import(bytes, "note_imported").expect("import");
    assert!(imported.report.stroke_count > 0, "expected strokes");

    let dir = std::env::temp_dir().join(format!(
        "metamoji-import-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let paths = AppPaths::new(dir.clone()).expect("paths");
    let now = now_iso();

    {
        let mut store = NoteStore::open(&paths.note_path("note_imported").expect("valid note_id")).expect("open");
        store
            .write_tree(&imported.tree, "取り込み", &now, &now, 0)
            .expect("write");
    }

    let reopened = {
        let store = NoteStore::open(&paths.note_path("note_imported").expect("valid note_id")).expect("reopen");
        store.read_tree().expect("read")
    };

    assert_eq!(reopened.models.len(), imported.tree.models.len());
    assert_eq!(reopened.root_id, imported.tree.root_id);

    // Strokes are the whole point; count them on both sides and compare.
    let count_strokes = |tree: &metamoji_lib::model::GenericTree| -> usize {
        tree.models
            .values()
            .filter(|m| m.model_type == "$draw")
            .filter_map(|m| m.props.get("strokes"))
            .filter_map(|v| v.as_array())
            .map(|a| a.len())
            .sum()
    };
    let before = count_strokes(&imported.tree);
    let after = count_strokes(&reopened);
    assert_eq!(before, after, "strokes were lost across the save/reload");
    assert!(
        before > 500,
        "expected a rich document, got {before} strokes"
    );

    // A stroke must still carry usable geometry and styling after the trip.
    let sample = reopened
        .models
        .values()
        .filter(|m| m.model_type == "$draw")
        .filter_map(|m| m.props.get("strokes"))
        .filter_map(|v| v.as_array())
        .flatten()
        .next()
        .expect("at least one stroke");
    assert!(
        sample["points"]["$points"]
            .as_array()
            .expect("points")
            .len()
            >= 4
    );
    assert!(sample["color"].as_str().expect("color").starts_with('#'));
    assert!(sample["width"].as_f64().expect("width") > 0.0);

    eprintln!("imported and round-tripped {before} strokes");
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Every model as `type|version|properties`, with references named by what
/// they point at rather than by table index.
///
/// Indices are allocation order and mean nothing across two writes of the same
/// document, so comparing them would fail on files that are otherwise
/// identical. Comparing this as a multiset is the real question: are the same
/// models, with the same properties, still there?
fn model_bag(bytes: &[u8]) -> Vec<String> {
    use serde_json::{Map, Value};

    let doc = atdoc::parse_document(bytes).expect("parse");

    fn resolve(value: &Value, types: &std::collections::HashMap<usize, String>) -> Value {
        match value {
            Value::Object(map) => {
                if let Some(Value::Number(n)) = map.get("$ref") {
                    let target = n.as_u64().unwrap_or(0) as usize;
                    // Named by what it points at, not by where it landed.
                    return Value::String(format!(
                        "->{}",
                        types.get(&target).cloned().unwrap_or_else(|| "?".into())
                    ));
                }
                let mut out = Map::new();
                for (k, v) in map {
                    out.insert(k.clone(), resolve(v, types));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(|v| resolve(v, types)).collect()),
            other => other.clone(),
        }
    }

    let types: std::collections::HashMap<usize, String> = doc
        .models
        .iter()
        .map(|(i, m)| (*i, m.model_type.clone()))
        .collect();

    let mut bag: Vec<String> = doc
        .models
        .values()
        .map(|m| {
            format!(
                "{}|{}|{}",
                m.model_type,
                m.version,
                resolve(&m.props, &types)
            )
        })
        .collect();
    bag.sort();
    bag
}

#[test]
fn every_bundled_document_survives_being_written_back() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("no corpus; skipping");
        return;
    }

    for path in files {
        let raw = std::fs::read(&path).expect("read");
        let original = match atdoc::unwrap_container(raw) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };

        let imported = atdoc::import(original.clone(), "root").expect("import");
        let mut tree = imported.tree;
        let assets: std::collections::HashMap<String, (String, Vec<u8>)> = imported
            .assets
            .into_iter()
            .map(|a| (a.ticket, (a.mime, a.bytes)))
            .collect();
        let missing = atdoc::restore_assets(&mut tree, &assets);
        assert!(
            missing.is_empty(),
            "{}: unresolved assets {missing:?}",
            path.display()
        );

        let written = atdoc::write_document(&tree, &atdoc::write_meta(&tree))
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));

        assert_eq!(
            model_bag(&written),
            model_bag(&original),
            "{} did not survive the round trip",
            path.display()
        );

        // A document the app wrote is one the app can read.
        let reimported = atdoc::import(written, "root").expect("re-import");
        assert_eq!(reimported.tree.models.len(), tree.models.len());
        assert_eq!(reimported.report.stroke_count, imported.report.stroke_count);
    }
}
