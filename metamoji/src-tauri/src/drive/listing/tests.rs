use super::*;

use std::io::Write;

/// Builds a zip the way the drive service does: named entries, JSON inside.
fn archive(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut buffer = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut buffer));
        let options: zip::write::FileOptions<'_, '_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }
    buffer
}

fn ids(listing: &Listing) -> Vec<&str> {
    listing
        .documents
        .iter()
        .map(|d| d.document_id.as_str())
        .collect()
}

#[test]
fn reads_notes_from_a_documents_entry() {
    let listing = parse(archive(&[(
        "documents_1.json",
        r#"[{"id":"doc-9","title":"算数 4月12日","contentsRevision":"17",
             "lastUpdate":"2026-04-12T01:00:00Z"}]"#,
    )]))
    .unwrap();

    assert_eq!(listing.documents.len(), 1);
    assert_eq!(listing.documents[0].title.as_deref(), Some("算数 4月12日"));
    assert_eq!(listing.documents[0].revision.as_deref(), Some("17"));
    // Nothing claimed it, so it sits at the top rather than nowhere.
    assert_eq!(listing.documents[0].folder_path, "/");
}

#[test]
fn folders_come_from_their_paths() {
    // `SdMOFolder` is keyed by `absPath`, so the hierarchy is already in the
    // strings — there is nothing to join.
    let listing = parse(archive(&[(
        "folderdefs_1.json",
        r#"[{"absPath":"/算数/"},{"absPath":"/算数/4月/"},{"absPath":"/国語/"}]"#,
    )]))
    .unwrap();

    let paths: Vec<&str> = listing
        .folders
        .iter()
        .map(|f| f.abs_path.as_str())
        .collect();
    assert_eq!(paths, ["/国語/", "/算数/", "/算数/4月/"]);

    let april = listing.folders.iter().find(|f| f.name == "4月").unwrap();
    assert_eq!(april.parent_path.as_deref(), Some("/算数/"));
    assert_eq!(april.depth, 2);

    let maths = listing.folders.iter().find(|f| f.name == "算数").unwrap();
    assert_eq!(maths.parent_path, None, "a top-level folder has no parent");
    assert_eq!(maths.depth, 1);
}

#[test]
fn a_note_names_its_own_folder_through_tags() {
    // Membership is the document's `tags`, `/`-delimited — the same path the
    // folder is keyed by. `SdDriveSyncProcess$19$1` reads it as a string and
    // hands it to `SdUtils.tagsFromPath`.
    //
    // Reading membership from the folder's `childrenOrder` instead put every
    // note at the top level: that field is empty on the way down, and the
    // hierarchy still looked right, so only the notes were wrong.
    let listing = parse(archive(&[
        ("folderdefs_1.json", r#"[{"absPath":"/算数/"}]"#),
        (
            "documents_1.json",
            r#"[{"id":"doc-a","tags":"/算数/"},
                {"id":"doc-b","tags":"算数"},
                {"id":"doc-c"},
                {"id":"doc-d","tags":""}]"#,
        ),
    ]))
    .unwrap();

    let folder_of = |id: &str| {
        listing
            .documents
            .iter()
            .find(|d| d.document_id == id)
            .unwrap()
            .folder_path
            .clone()
    };
    assert_eq!(folder_of("doc-a"), "/算数/");
    // The slashes are not always there; the path is what matters.
    assert_eq!(folder_of("doc-b"), "/算数/");
    assert_eq!(folder_of("doc-c"), "/", "no tags means the top level");
    assert_eq!(folder_of("doc-d"), "/", "and so does an empty one");
}

#[test]
fn a_nested_folder_holds_its_own_notes() {
    let listing = parse(archive(&[
        (
            "folderdefs_1.json",
            r#"[{"absPath":"/算数/"},{"absPath":"/算数/4月/","parentAbsPath":"/算数/"}]"#,
        ),
        (
            "documents_1.json",
            r#"[{"id":"a","tags":"/算数/"},{"id":"b","tags":"/算数/4月/"}]"#,
        ),
    ]))
    .unwrap();

    let folder_of = |id: &str| {
        listing
            .documents
            .iter()
            .find(|d| d.document_id == id)
            .unwrap()
            .folder_path
            .clone()
    };
    assert_eq!(folder_of("a"), "/算数/");
    assert_eq!(folder_of("b"), "/算数/4月/");
}

#[test]
fn a_parent_given_outright_is_used_rather_than_inferred() {
    // No risk of inferring it differently from the server.
    let listing = parse(archive(&[(
        "folderdefs_1.json",
        r#"[{"absPath":"/算数/4月/","parentAbsPath":"算数"}]"#,
    )]))
    .unwrap();
    assert_eq!(listing.folders[0].parent_path.as_deref(), Some("/算数/"));
}

#[test]
fn the_drives_own_arrangement_orders_a_folders_contents() {
    // `childrenorders_` is ordering, not membership — its own entries, not a
    // field on the folder record.
    let listing = parse(archive(&[
        ("folderdefs_1.json", r#"[{"absPath":"/算数/"}]"#),
        (
            "childrenorders_1.json",
            r#"[{"absPath":"/算数/","childrenOrder":"/c/a/b/"}]"#,
        ),
        (
            "documents_1.json",
            r#"[{"id":"a","title":"あ","tags":"/算数/"},
                {"id":"b","title":"い","tags":"/算数/"},
                {"id":"c","title":"う","tags":"/算数/"}]"#,
        ),
    ]))
    .unwrap();

    // Not title order: the drive said otherwise.
    assert_eq!(ids(&listing), ["c", "a", "b"]);
}

#[test]
fn a_note_the_arrangement_omits_falls_to_the_end() {
    let listing = parse(archive(&[
        (
            "childrenorders_1.json",
            r#"[{"absPath":"/","childrenOrder":"/b/"}]"#,
        ),
        (
            "documents_1.json",
            r#"[{"id":"a","title":"あ"},{"id":"b","title":"ん"},{"id":"c","title":"う"}]"#,
        ),
    ]))
    .unwrap();

    // `b` is placed; the rest keep a stable title order behind it.
    assert_eq!(ids(&listing), ["b", "a", "c"]);
}

#[test]
fn the_root_is_not_listed_as_a_folder() {
    // It is where everything already is; a row for it would be a loop.
    let listing = parse(archive(&[(
        "folderdefs_1.json",
        r#"[{"absPath":"/"},{"absPath":""}]"#,
    )]))
    .unwrap();
    assert!(listing.folders.is_empty());
    assert_eq!(listing.record_count, 2, "still counted");
}

#[test]
fn deleted_records_are_counted_but_not_offered() {
    let listing = parse(archive(&[
        (
            "documents_1.json",
            r#"[{"id":"a"},{"id":"b","deleteFlag":true},{"id":"c","deleteFlag":1}]"#,
        ),
        (
            "folderdefs_1.json",
            r#"[{"absPath":"/消した/","deleteFlag":true}]"#,
        ),
    ]))
    .unwrap();

    assert_eq!(ids(&listing), ["a"]);
    assert!(listing.folders.is_empty());
    // "four records, one visible" and "one record" are different situations.
    assert_eq!(listing.record_count, 4);
}

#[test]
fn the_archives_other_entries_are_not_reported_as_faults() {
    // They are real data this build does not use yet, not corruption.
    let listing = parse(archive(&[
        ("documents_1.json", r#"[{"id":"a"}]"#),
        ("tagdefs_1.json", r#"[{"name":"重要"}]"#),
        ("tagorder.json", r#"{"tagOrder":""}"#),
        ("drive.json", r#"{"lastUpdateRevision":"9"}"#),
        ("meta.json", r#"{}"#),
    ]))
    .unwrap();

    assert_eq!(ids(&listing), ["a"]);
    assert!(
        listing.unrecognised.is_empty(),
        "{:?}",
        listing.unrecognised
    );
}

#[test]
fn a_genuinely_unknown_entry_is_named() {
    // An archive holding more than this understands is worth saying so about;
    // silently showing fewer notes than the class has is not.
    let listing = parse(archive(&[
        ("documents_1.json", r#"[{"id":"a"}]"#),
        ("surprise_1.json", r#"[{"id":"b"}]"#),
    ]))
    .unwrap();

    assert_eq!(ids(&listing), ["a"]);
    assert_eq!(listing.unrecognised, ["surprise_1.json"]);
}

#[test]
fn entries_may_be_nested_in_the_archive() {
    // The dispatch is on the file name, not the whole path.
    let listing = parse(archive(&[("data/documents_1.json", r#"[{"id":"a"}]"#)])).unwrap();
    assert_eq!(ids(&listing), ["a"]);
}

#[test]
fn notes_are_ordered_by_title_not_by_archive_order() {
    let listing = parse(archive(&[(
        "documents_1.json",
        r#"[{"id":"z","title":"あ"},{"id":"a","title":"い"}]"#,
    )]))
    .unwrap();
    assert_eq!(ids(&listing), ["z", "a"]);
}

#[test]
fn a_numeric_revision_is_read_as_a_string() {
    let listing = parse(archive(&[(
        "documents_1.json",
        r#"[{"id":"a","contentsRevision":17}]"#,
    )]))
    .unwrap();
    assert_eq!(listing.documents[0].revision.as_deref(), Some("17"));
}

#[test]
fn an_empty_archive_is_empty_rather_than_an_error() {
    let listing = parse(archive(&[])).unwrap();
    assert!(listing.documents.is_empty());
    assert!(listing.folders.is_empty());
    assert_eq!(listing.record_count, 0);
}

#[test]
fn something_that_is_not_a_zip_says_so() {
    let err = parse(b"not a zip at all".to_vec()).unwrap_err().to_string();
    assert!(err.contains("開けません"), "{err}");
}

#[test]
fn folders_in_different_parents_do_not_crash_the_sort() {
    // Ranking both sides against the *first* folder's parent reads the right
    // way round but is not a total order: a nested folder ranks first inside
    // its own parent, a top-level one ranks first inside the root, and asked
    // either way round the comparator answers "less". Rust's sort notices and
    // panics, taking the whole class box down with it — which is what 中学1年3組
    // did, with 48 folders across a dozen parents.
    let mut defs = Vec::new();
    let mut top = String::new();
    let mut nested = String::new();
    for i in 0..20 {
        defs.push(format!(r#"{{"absPath":"/t{i}/"}}"#));
        top.push_str(&format!("t{i}/"));
        defs.push(format!(r#"{{"absPath":"/t0/n{i}/"}}"#));
        nested.push_str(&format!("n{i}/"));
    }
    let defs = format!("[{}]", defs.join(","));
    let orders = format!(
        r#"[{{"absPath":"/","childrenOrder":"/{top}"}},
            {{"absPath":"/t0/","childrenOrder":"/{nested}"}}]"#
    );

    let listing = parse(archive(&[
        ("folderdefs_1.json", &defs),
        ("childrenorders_1.json", &orders),
    ]))
    .unwrap();

    assert_eq!(listing.folders.len(), 40);
    // Each folder ranks within its own parent, so the nested ones come out in
    // the order /t0/ gave them.
    let nested_order: Vec<&str> = listing
        .folders
        .iter()
        .filter(|f| f.parent_path.as_deref() == Some("/t0/"))
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(nested_order[..3], ["n0", "n1", "n2"]);
}
