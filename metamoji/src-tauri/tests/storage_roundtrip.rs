//! Storage round-trip: the path the desktop app actually saves through.
//!
//! The model layer is exercised from the frontend tests, but those run against
//! an in-memory backend. These cover the real thing — SQLite files on disk —
//! because "the note reopened with everything still in it" is the property that
//! matters most and the one a schema change breaks silently.

use std::collections::HashMap;

use metamoji_lib::model::{GenericModel, GenericTree};
use metamoji_lib::storage::{now_iso, AppPaths, Catalog, ListQuery, NoteStore};

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "metamoji-test-{name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir should be creatable");
    dir
}

/// A note shaped like one the editor produces: a page, a layer, and units.
fn sample_tree(root: &str) -> GenericTree {
    let mut tree = GenericTree::new(root, "$freenote");
    tree.models.get_mut(root).unwrap().props = serde_json::json!({
        "title": "往復テスト",
        "revision": 3,
    });

    tree.insert(GenericModel {
        id: "page_1".into(),
        parent_id: Some(root.into()),
        model_type: "$page".into(),
        props: serde_json::json!({ "paperWidth": 1240.0, "paperHeight": 1754.0, "paperStyle": "grid" }),
        children: vec![],
    });
    tree.insert(GenericModel {
        id: "layer_1".into(),
        parent_id: Some("page_1".into()),
        model_type: "$layer".into(),
        props: serde_json::json!({ "layerType": "content", "visible": true }),
        children: vec![],
    });

    // Three units in a deliberate order — order is the thing most likely to be
    // lost by a naive reload, and it is paint order, so it is user-visible.
    for (i, (id, ty)) in [
        ("unit_ink", "$draw"),
        ("unit_text", "$text"),
        ("unit_img", "$image"),
    ]
    .iter()
    .enumerate()
    {
        tree.insert(GenericModel {
            id: (*id).into(),
            parent_id: Some("layer_1".into()),
            model_type: (*ty).into(),
            props: serde_json::json!({ "x": i as f64 * 10.0, "y": 5.0, "ord": i }),
            children: vec![],
        });
    }
    tree
}

#[test]
fn a_note_reopens_with_its_tree_intact() {
    let dir = tmp_dir("roundtrip");
    let paths = AppPaths::new(dir.clone()).expect("paths");
    let path = paths.note_path("note_1").expect("valid note_id");
    let tree = sample_tree("note_1");
    let now = now_iso();

    {
        let mut store = NoteStore::open(&path).expect("open");
        store
            .write_tree(&tree, "往復テスト", &now, &now, 3)
            .expect("write");
    }

    let reopened = {
        let store = NoteStore::open(&path).expect("reopen");
        store.read_tree().expect("read")
    };

    assert_eq!(reopened.root_id, "note_1");
    assert_eq!(reopened.models.len(), tree.models.len());

    for (id, original) in &tree.models {
        let restored = reopened
            .models
            .get(id)
            .unwrap_or_else(|| panic!("model {id} was lost"));
        assert_eq!(&restored.model_type, &original.model_type, "type of {id}");
        assert_eq!(&restored.parent_id, &original.parent_id, "parent of {id}");
        assert_eq!(&restored.props, &original.props, "props of {id}");
    }

    // Sibling order is paint order; losing it reorders the page.
    assert_eq!(
        reopened.models["layer_1"].children,
        vec!["unit_ink", "unit_text", "unit_img"],
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn saving_twice_replaces_rather_than_accumulates() {
    let dir = tmp_dir("resave");
    let paths = AppPaths::new(dir.clone()).expect("paths");
    let path = paths.note_path("note_2").expect("valid note_id");
    let now = now_iso();

    let mut store = NoteStore::open(&path).expect("open");
    store
        .write_tree(&sample_tree("note_2"), "一回目", &now, &now, 1)
        .expect("first write");

    // Second save with a unit removed — the row must actually go away.
    let mut smaller = sample_tree("note_2");
    smaller.models.remove("unit_img");
    smaller
        .models
        .get_mut("layer_1")
        .unwrap()
        .children
        .retain(|c| c != "unit_img");
    store
        .write_tree(&smaller, "二回目", &now, &now, 2)
        .expect("second write");

    let reopened = store.read_tree().expect("read");
    assert!(!reopened.models.contains_key("unit_img"));
    assert_eq!(reopened.models.len(), smaller.models.len());

    let (title, _, _, revision, page_count) = store.meta().expect("meta");
    assert_eq!(title, "二回目");
    assert_eq!(revision, 2);
    assert_eq!(page_count, 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn assets_survive_a_reopen() {
    let dir = tmp_dir("assets");
    let paths = AppPaths::new(dir.clone()).expect("paths");
    let path = paths.note_path("note_3").expect("valid note_id");
    let bytes = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

    {
        let store = NoteStore::open(&path).expect("open");
        store
            .put_asset("asset_1", "image/png", &bytes)
            .expect("put");
    }

    let store = NoteStore::open(&path).expect("reopen");
    let (mime, restored) = store.get_asset("asset_1").expect("get");
    assert_eq!(mime, "image/png");
    assert_eq!(restored, bytes);
    assert_eq!(store.asset_ids().expect("ids"), vec!["asset_1"]);

    // A ticket that was never stored must be an error, not an empty image.
    assert!(store.get_asset("missing").is_err());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_catalog_lists_indexes_and_trashes_notes() {
    let dir = tmp_dir("catalog");
    let paths = AppPaths::new(dir.clone()).expect("paths");
    let catalog = Catalog::open(&paths.library_db).expect("catalog");
    let now = now_iso();

    catalog
        .upsert("n1", &paths.note_path("n1").expect("valid note_id"), "一つ目", &now, &now, 2, 0)
        .expect("upsert");
    catalog
        .upsert("n2", &paths.note_path("n2").expect("valid note_id"), "二つ目", &now, &now, 1, 0)
        .expect("upsert");

    assert_eq!(catalog.list(&ListQuery::default()).expect("list").len(), 2);

    // Upsert on an existing id updates rather than duplicating.
    catalog
        .upsert("n1", &paths.note_path("n1").expect("valid note_id"), "改題", &now, &now, 5, 7)
        .expect("re-upsert");
    let notes = catalog.list(&ListQuery::default()).expect("list");
    assert_eq!(notes.len(), 2);
    let n1 = notes.iter().find(|n| n.id == "n1").expect("n1");
    assert_eq!(n1.title, "改題");
    assert_eq!(n1.page_count, 5);
    assert_eq!(n1.revision, 7);

    catalog.set_trashed("n1", true).expect("trash");
    // The default list hides trashed notes; `trashed: true` is the trash view,
    // which shows only them.
    assert_eq!(catalog.list(&ListQuery::default()).expect("list").len(), 1);
    let trash = catalog
        .list(&ListQuery {
            trashed: true,
            ..Default::default()
        })
        .expect("trash view");
    assert_eq!(trash.len(), 1);
    assert_eq!(trash[0].id, "n1");
    assert!(trash[0].trashed);

    catalog.set_trashed("n1", false).expect("restore");
    assert_eq!(catalog.list(&ListQuery::default()).expect("list").len(), 2);

    // A thumbnail comes back as a data URL the library grid can use directly.
    catalog.set_thumbnail("n2", &[1, 2, 3]).expect("thumb");
    let n2 = catalog
        .list(&ListQuery::default())
        .expect("list")
        .into_iter()
        .find(|n| n.id == "n2")
        .expect("n2");
    assert!(n2
        .thumbnail
        .expect("thumbnail")
        .starts_with("data:image/png;base64,"));

    assert!(catalog.path_of("missing").is_err());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_tree_with_no_metadata_row_is_an_error_not_a_panic() {
    let dir = tmp_dir("empty");
    let paths = AppPaths::new(dir.clone()).expect("paths");
    let store = NoteStore::open(&paths.note_path("note_4").expect("valid note_id")).expect("open");
    // Freshly created file: schema exists, but nothing was ever written.
    assert!(store.read_tree().is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn walk_visits_parents_before_children() {
    let tree = sample_tree("note_5");
    let order: Vec<&str> = tree.walk().iter().map(|m| m.id.as_str()).collect();

    let index_of = |id: &str| order.iter().position(|o| *o == id).expect("present");
    assert!(index_of("note_5") < index_of("page_1"));
    assert!(index_of("page_1") < index_of("layer_1"));
    assert!(index_of("layer_1") < index_of("unit_ink"));
    // Children keep document order within their parent.
    assert!(index_of("unit_ink") < index_of("unit_text"));
    assert!(index_of("unit_text") < index_of("unit_img"));

    let mut seen: HashMap<&str, usize> = HashMap::new();
    for id in &order {
        *seen.entry(id).or_insert(0) += 1;
    }
    assert!(seen.values().all(|c| *c == 1), "walk visited a model twice");
}

#[test]
fn folders_hold_notes_and_release_them_when_deleted() {
    let dir = tmp_dir("folders");
    let paths = AppPaths::new(dir.clone()).expect("paths");
    let catalog = Catalog::open(&paths.library_db).expect("catalog");
    let now = now_iso();

    catalog
        .upsert("n1", &paths.note_path("n1").expect("valid note_id"), "一つ目", &now, &now, 1, 0)
        .expect("upsert");
    catalog
        .upsert("n2", &paths.note_path("n2").expect("valid note_id"), "二つ目", &now, &now, 1, 0)
        .expect("upsert");

    catalog.create_folder("f1", "授業", None).expect("folder");
    catalog.set_folder("n1", Some("f1")).expect("assign");

    let folders = catalog.folders().expect("folders");
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].note_count, 1);

    let in_folder = catalog
        .list(&ListQuery {
            folder_id: Some("f1".into()),
            ..Default::default()
        })
        .expect("list");
    assert_eq!(in_folder.len(), 1);
    assert_eq!(in_folder[0].id, "n1");

    // Only notes with no folder at all, for the library root view.
    let root = catalog
        .list(&ListQuery {
            root_only: true,
            ..Default::default()
        })
        .expect("list");
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].id, "n2");

    // Deleting a folder must not delete the notes inside it.
    catalog.delete_folder("f1").expect("delete folder");
    assert_eq!(catalog.list(&ListQuery::default()).expect("list").len(), 2);
    assert!(catalog.folders().expect("folders").is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn tags_attach_to_notes_and_filter_the_list() {
    let dir = tmp_dir("tags");
    let paths = AppPaths::new(dir.clone()).expect("paths");
    let catalog = Catalog::open(&paths.library_db).expect("catalog");
    let now = now_iso();

    catalog
        .upsert("n1", &paths.note_path("n1").expect("valid note_id"), "一つ目", &now, &now, 1, 0)
        .expect("upsert");
    catalog
        .upsert("n2", &paths.note_path("n2").expect("valid note_id"), "二つ目", &now, &now, 1, 0)
        .expect("upsert");

    let tag = catalog.create_tag("t1", "重要", "#d93025").expect("tag");
    assert_eq!(tag.name, "重要");

    // Creating the same name twice returns the existing tag rather than failing.
    let again = catalog.create_tag("t2", "重要", "#000000").expect("tag");
    assert_eq!(again.id, "t1");
    assert_eq!(catalog.tags().expect("tags").len(), 1);

    catalog.set_document_tag("n1", "t1", true).expect("attach");
    let tagged = catalog
        .list(&ListQuery {
            tag_id: Some("t1".into()),
            ..Default::default()
        })
        .expect("list");
    assert_eq!(tagged.len(), 1);
    assert_eq!(tagged[0].id, "n1");
    assert_eq!(tagged[0].tags.len(), 1);
    assert_eq!(tagged[0].tags[0].name, "重要");

    catalog.set_document_tag("n1", "t1", false).expect("detach");
    assert!(catalog
        .list(&ListQuery {
            tag_id: Some("t1".into()),
            ..Default::default()
        })
        .expect("list")
        .is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn full_text_search_finds_notes_by_title_and_body() {
    let dir = tmp_dir("search");
    let paths = AppPaths::new(dir.clone()).expect("paths");
    let catalog = Catalog::open(&paths.library_db).expect("catalog");
    let now = now_iso();

    for (id, title) in [("n1", "algebra notes"), ("n2", "history essay")] {
        catalog
            .upsert(id, &paths.note_path(id).expect("valid note_id"), title, &now, &now, 1, 0)
            .expect("upsert");
    }
    catalog
        .index_document("n1", "algebra notes", "quadratic equations and factoring")
        .expect("index");
    catalog
        .index_document("n2", "history essay", "the industrial revolution")
        .expect("index");

    let by_title = |text: &str| {
        catalog
            .list(&ListQuery {
                text: Some(text.into()),
                ..Default::default()
            })
            .expect("list")
    };

    assert_eq!(by_title("algebra").len(), 1);
    assert_eq!(by_title("algebra")[0].id, "n1");
    // Body text is searchable too, not just the title.
    assert_eq!(by_title("quadratic")[0].id, "n1");
    assert_eq!(by_title("industrial")[0].id, "n2");
    assert!(by_title("nonexistent").is_empty());

    // Re-indexing replaces rather than accumulating, so stale text stops matching.
    catalog
        .index_document("n1", "algebra notes", "completely different content")
        .expect("reindex");
    assert!(by_title("quadratic").is_empty());

    // Punctuation is FTS5 syntax; it must not blow up the query.
    for junk in ["\"", "*", "a:b", "^^^", "-", ""] {
        catalog
            .list(&ListQuery {
                text: Some(junk.into()),
                ..Default::default()
            })
            .unwrap_or_else(|e| panic!("query {junk:?} failed: {e}"));
    }

    // Deleting a note clears its search entry.
    catalog.remove("n1").expect("remove");
    assert!(by_title("algebra").is_empty());

    std::fs::remove_dir_all(&dir).ok();
}
