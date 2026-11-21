//! Persistence.
//!
//! Two kinds of database, following the split the original app arrived at
//! (docs/07): a catalog of lightweight metadata for the library screen, and the
//! documents themselves. Here the catalog is `library.db` and each note is its
//! own `<id>.mmnote` SQLite file under `notes/`.
//!
//! One file per note is what makes copy, backup, export and (later) per-document
//! sync revisions straightforward, and it keeps a corrupted file from taking the
//! whole library with it. Inside a note, the model rows use the
//! `(model_id, parent_id, model_type, props_json)` shape docs/04 §10 recommends.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{AppError, AppResult};
use crate::model::{GenericModel, GenericTree, NoteSummary};

pub const SCHEMA_VERSION: i64 = 1;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub notes_dir: PathBuf,
    pub library_db: PathBuf,
}

impl AppPaths {
    pub fn new(data_dir: PathBuf) -> AppResult<Self> {
        let notes_dir = data_dir.join("notes");
        std::fs::create_dir_all(&notes_dir)?;
        let library_db = data_dir.join("library.db");
        Ok(Self {
            data_dir,
            notes_dir,
            library_db,
        })
    }

    pub fn note_path(&self, id: &str) -> AppResult<PathBuf> {
        // `id` is an IPC-supplied string. This app only ever issues UUID-shaped
        // ids itself, so traversal/separator characters here can only mean the
        // webview boundary is being abused.
        if id.is_empty()
            || id.contains('/')
            || id.contains('\\')
            || id.contains("..")
            || Path::new(id).is_absolute()
        {
            return Err(AppError::other("不正な note_id です"));
        }
        Ok(self.notes_dir.join(format!("{id}.mmnote")))
    }
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    pub fn open(path: &Path) -> AppResult<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let catalog = Self { conn };
        catalog.migrate()?;
        Ok(catalog)
    }

    fn migrate(&self) -> AppResult<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS documents (
                id          TEXT PRIMARY KEY,
                path        TEXT NOT NULL UNIQUE,
                title       TEXT NOT NULL,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                last_access TEXT,
                page_count  INTEGER NOT NULL DEFAULT 1,
                revision    INTEGER NOT NULL DEFAULT 0,
                trashed     INTEGER NOT NULL DEFAULT 0,
                thumbnail   BLOB
            );
            CREATE INDEX IF NOT EXISTS idx_documents_updated
                ON documents(trashed, updated_at DESC);

            CREATE TABLE IF NOT EXISTS folders (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                parent_id  TEXT REFERENCES folders(id) ON DELETE CASCADE,
                ord        INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_folders_parent ON folders(parent_id, ord);

            CREATE TABLE IF NOT EXISTS tags (
                id    TEXT PRIMARY KEY,
                name  TEXT NOT NULL UNIQUE,
                color TEXT NOT NULL DEFAULT '#32a5ff'
            );
            CREATE TABLE IF NOT EXISTS document_tags (
                document_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
                tag_id      TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                PRIMARY KEY (document_id, tag_id)
            );
            CREATE INDEX IF NOT EXISTS idx_document_tags_tag ON document_tags(tag_id);

            -- Full-text index over note titles and text-unit content. The
            -- original kept a separate tag/search database for exactly this
            -- reason (docs/07 §3): scanning every note file to answer a search
            -- does not scale. FTS5 replaces that whole subsystem.
            CREATE VIRTUAL TABLE IF NOT EXISTS document_search
                USING fts5(document_id UNINDEXED, title, body);
            "#,
        )?;
        // `folder_id` arrived after the first release, so add it to tables that
        // predate it. SQLite has no "ADD COLUMN IF NOT EXISTS", and the error
        // for a duplicate column is the expected outcome on an up-to-date file.
        if !self.has_column("documents", "folder_id")? {
            self.conn
                .execute("ALTER TABLE documents ADD COLUMN folder_id TEXT", [])?;
        }
        // Sync bookkeeping. `server_revision` is the revision the server last
        // accepted from us — the optimistic-lock token a later write sends as
        // `check` (docs/06 §5). Absent means the note has never been synced.
        if !self.has_column("documents", "server_revision")? {
            self.conn.execute(
                "ALTER TABLE documents ADD COLUMN server_revision INTEGER",
                [],
            )?;
        }
        if !self.has_column("documents", "synced_revision")? {
            self.conn.execute(
                "ALTER TABLE documents ADD COLUMN synced_revision INTEGER",
                [],
            )?;
        }
        // Where a note came from, when it came from a class box. A local copy
        // is not much use without it: sending what the user writes back means
        // knowing which document in which drive it belongs to.
        if !self.has_column("documents", "class_drive_id")? {
            self.conn
                .execute("ALTER TABLE documents ADD COLUMN class_drive_id TEXT", [])?;
        }
        if !self.has_column("documents", "class_document_id")? {
            self.conn.execute(
                "ALTER TABLE documents ADD COLUMN class_document_id TEXT",
                [],
            )?;
        }
        // Kept with the rest rather than looked up when needed: the lookup
        // goes through the drive service, which is only signed in while a
        // class box is being browsed, and sending happens whenever the user
        // saves.
        if !self.has_column("documents", "class_room_id")? {
            self.conn
                .execute("ALTER TABLE documents ADD COLUMN class_room_id TEXT", [])?;
        }

        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    fn has_column(&self, table: &str, column: &str) -> AppResult<bool> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn schema_version(&self) -> i64 {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    pub fn list(&self, query: &ListQuery) -> AppResult<Vec<NoteSummary>> {
        let mut sql = String::from(
            "SELECT d.id, d.title, d.created_at, d.updated_at, d.page_count, d.revision,
                    d.thumbnail, d.folder_id, d.trashed, d.server_revision, d.synced_revision
             FROM documents d",
        );
        let mut wheres: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(tag_id) = &query.tag_id {
            sql.push_str(" JOIN document_tags dt ON dt.document_id = d.id");
            wheres.push("dt.tag_id = ?".into());
            args.push(Box::new(tag_id.clone()));
        }

        wheres.push(if query.trashed {
            "d.trashed = 1".into()
        } else {
            "d.trashed = 0".into()
        });

        // A folder filter only applies outside the trash: the trash is flat, so
        // a note keeps its folder but is listed regardless of it.
        if !query.trashed {
            match &query.folder_id {
                Some(folder) => {
                    wheres.push("d.folder_id = ?".into());
                    args.push(Box::new(folder.clone()));
                }
                None if query.root_only => wheres.push("d.folder_id IS NULL".into()),
                None => {}
            }
        }

        // A query that reduces to nothing — all punctuation, say — must not
        // become an empty MATCH, which FTS5 rejects as a syntax error.
        if let Some(match_expr) = query.text.as_deref().and_then(fts_query) {
            wheres.push(
                "d.id IN (SELECT document_id FROM document_search WHERE document_search MATCH ?)"
                    .into(),
            );
            args.push(Box::new(match_expr));
        }

        if !wheres.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&wheres.join(" AND "));
        }

        sql.push_str(match query.sort.as_deref() {
            Some("title") => " ORDER BY d.title COLLATE NOCASE ASC",
            Some("created") => " ORDER BY d.created_at DESC",
            _ => " ORDER BY d.updated_at DESC",
        });

        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            let png: Option<Vec<u8>> = row.get(6)?;
            Ok(NoteSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
                page_count: row.get(4)?,
                revision: row.get(5)?,
                thumbnail: png.map(|b| to_png_data_url(&b)),
                folder_id: row.get(7)?,
                trashed: row.get::<_, i64>(8)? != 0,
                server_revision: row.get(9)?,
                synced_revision: row.get(10)?,
                tags: Vec::new(),
            })
        })?;

        let mut out = rows.collect::<Result<Vec<_>, _>>()?;
        self.attach_tags(&mut out)?;
        Ok(out)
    }

    /// Fills in each summary's tag list in one query rather than one per note.
    fn attach_tags(&self, notes: &mut [NoteSummary]) -> AppResult<()> {
        if notes.is_empty() {
            return Ok(());
        }
        let mut stmt = self.conn.prepare(
            "SELECT dt.document_id, t.id, t.name, t.color
             FROM document_tags dt JOIN tags t ON t.id = dt.tag_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Tag {
                    id: row.get(1)?,
                    name: row.get(2)?,
                    color: row.get(3)?,
                },
            ))
        })?;

        let mut by_doc: HashMap<String, Vec<Tag>> = HashMap::new();
        for row in rows {
            let (doc_id, tag) = row?;
            by_doc.entry(doc_id).or_default().push(tag);
        }
        for note in notes.iter_mut() {
            if let Some(tags) = by_doc.remove(&note.id) {
                note.tags = tags;
            }
        }
        Ok(())
    }

    pub fn upsert(
        &self,
        id: &str,
        path: &Path,
        title: &str,
        created_at: &str,
        updated_at: &str,
        page_count: i64,
        revision: i64,
    ) -> AppResult<()> {
        self.conn.execute(
            r#"
            INSERT INTO documents(id, path, title, created_at, updated_at, page_count, revision)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(id) DO UPDATE SET
                title      = excluded.title,
                updated_at = excluded.updated_at,
                page_count = excluded.page_count,
                revision   = excluded.revision
            "#,
            params![
                id,
                path.to_string_lossy(),
                title,
                created_at,
                updated_at,
                page_count,
                revision
            ],
        )?;
        Ok(())
    }

    pub fn set_thumbnail(&self, id: &str, png: &[u8]) -> AppResult<()> {
        self.conn.execute(
            "UPDATE documents SET thumbnail = ?2 WHERE id = ?1",
            params![id, png],
        )?;
        Ok(())
    }

    pub fn touch_access(&self, id: &str, when: &str) -> AppResult<()> {
        self.conn.execute(
            "UPDATE documents SET last_access = ?2 WHERE id = ?1",
            params![id, when],
        )?;
        Ok(())
    }

    pub fn set_trashed(&self, id: &str, trashed: bool) -> AppResult<()> {
        self.conn.execute(
            "UPDATE documents SET trashed = ?2 WHERE id = ?1",
            params![id, if trashed { 1 } else { 0 }],
        )?;
        Ok(())
    }

    pub fn rename(&self, id: &str, title: &str, updated_at: &str) -> AppResult<()> {
        self.conn.execute(
            "UPDATE documents SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title, updated_at],
        )?;
        Ok(())
    }

    pub fn path_of(&self, id: &str) -> AppResult<PathBuf> {
        let path: Option<String> = self
            .conn
            .query_row(
                "SELECT path FROM documents WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        path.map(PathBuf::from)
            .ok_or_else(|| AppError::NoteNotFound(id.to_string()))
    }

    pub fn remove(&self, id: &str) -> AppResult<()> {
        self.conn
            .execute("DELETE FROM documents WHERE id = ?1", params![id])?;
        self.conn.execute(
            "DELETE FROM document_search WHERE document_id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Records that the server accepted our copy at `server_revision`, taken
    /// from our own `synced_revision`. A note whose local revision has since
    /// moved past `synced_revision` has unsynced changes.
    pub fn set_sync_state(
        &self,
        id: &str,
        server_revision: i64,
        synced_revision: i64,
    ) -> AppResult<()> {
        self.conn.execute(
            "UPDATE documents SET server_revision = ?2, synced_revision = ?3 WHERE id = ?1",
            params![id, server_revision, synced_revision],
        )?;
        Ok(())
    }

    /// The drive revision this client has consumed, so a delta asks for the
    /// right window.
    pub fn drive_revision(&self) -> i64 {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'drive_revision'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    pub fn set_drive_revision(&self, revision: i64) -> AppResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('drive_revision', ?1)",
            params![revision.to_string()],
        )?;
        Ok(())
    }

    pub fn set_folder(&self, id: &str, folder_id: Option<&str>) -> AppResult<()> {
        self.conn.execute(
            "UPDATE documents SET folder_id = ?2 WHERE id = ?1",
            params![id, folder_id],
        )?;
        Ok(())
    }

    // -- search index --------------------------------------------------------

    /// Replaces a note's search entry.
    ///
    /// Called on save with the note's text-unit content flattened by the
    /// frontend, which is the only place that knows how to read a unit.
    pub fn index_document(&self, id: &str, title: &str, body: &str) -> AppResult<()> {
        self.conn.execute(
            "DELETE FROM document_search WHERE document_id = ?1",
            params![id],
        )?;
        self.conn.execute(
            "INSERT INTO document_search(document_id, title, body) VALUES (?1, ?2, ?3)",
            params![id, title, body],
        )?;
        Ok(())
    }

    // -- folders -------------------------------------------------------------

    pub fn folders(&self) -> AppResult<Vec<Folder>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, parent_id, ord FROM folders ORDER BY ord, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Folder {
                id: row.get(0)?,
                name: row.get(1)?,
                parent_id: row.get(2)?,
                ord: row.get(3)?,
                note_count: 0,
            })
        })?;
        let mut folders = rows.collect::<Result<Vec<_>, _>>()?;

        let mut counts = self.conn.prepare(
            "SELECT folder_id, COUNT(*) FROM documents
             WHERE trashed = 0 AND folder_id IS NOT NULL GROUP BY folder_id",
        )?;
        let mut map: HashMap<String, i64> = HashMap::new();
        let mut rows = counts.query([])?;
        while let Some(row) = rows.next()? {
            map.insert(row.get(0)?, row.get(1)?);
        }
        for folder in folders.iter_mut() {
            folder.note_count = map.get(&folder.id).copied().unwrap_or(0);
        }
        Ok(folders)
    }

    pub fn create_folder(&self, id: &str, name: &str, parent_id: Option<&str>) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO folders(id, name, parent_id, ord, created_at)
             VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(ord), 0) + 1 FROM folders), ?4)",
            params![id, name, parent_id, now_iso()],
        )?;
        Ok(())
    }

    pub fn rename_folder(&self, id: &str, name: &str) -> AppResult<()> {
        self.conn.execute(
            "UPDATE folders SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Deletes a folder. Its notes are not deleted — they move back to the
    /// root, because losing notes to a folder operation would be indefensible.
    pub fn delete_folder(&self, id: &str) -> AppResult<()> {
        self.conn.execute(
            "UPDATE documents SET folder_id = NULL WHERE folder_id = ?1",
            params![id],
        )?;
        self.conn
            .execute("DELETE FROM folders WHERE id = ?1", params![id])?;
        Ok(())
    }

    // -- tags ----------------------------------------------------------------

    pub fn tags(&self) -> AppResult<Vec<Tag>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, color FROM tags ORDER BY name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |row| {
            Ok(Tag {
                id: row.get(0)?,
                name: row.get(1)?,
                color: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn create_tag(&self, id: &str, name: &str, color: &str) -> AppResult<Tag> {
        self.conn.execute(
            "INSERT OR IGNORE INTO tags(id, name, color) VALUES (?1, ?2, ?3)",
            params![id, name, color],
        )?;
        // Tag names are unique, so an existing name wins rather than erroring —
        // the caller wanted a tag with this name and now there is one.
        let tag = self.conn.query_row(
            "SELECT id, name, color FROM tags WHERE name = ?1",
            params![name],
            |row| {
                Ok(Tag {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    color: row.get(2)?,
                })
            },
        )?;
        Ok(tag)
    }

    pub fn delete_tag(&self, id: &str) -> AppResult<()> {
        self.conn
            .execute("DELETE FROM tags WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn set_document_tag(&self, doc_id: &str, tag_id: &str, on: bool) -> AppResult<()> {
        if on {
            self.conn.execute(
                "INSERT OR IGNORE INTO document_tags(document_id, tag_id) VALUES (?1, ?2)",
                params![doc_id, tag_id],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM document_tags WHERE document_id = ?1 AND tag_id = ?2",
                params![doc_id, tag_id],
            )?;
        }
        Ok(())
    }

    // -- class-box origin ------------------------------------------------------

    /// Remembers which class-box document a note is a copy of.
    ///
    /// A local copy is not much use without it: sending what the user writes
    /// back means knowing which document in which drive it belongs to, and the
    /// note itself has nowhere to keep that — the editor rewrites the root
    /// model's properties on every save.
    pub fn set_class_origin(
        &self,
        id: &str,
        drive_id: &str,
        document_id: &str,
        room_id: Option<&str>,
    ) -> AppResult<()> {
        self.conn.execute(
            "UPDATE documents
                SET class_drive_id = ?2, class_document_id = ?3, class_room_id = ?4
              WHERE id = ?1",
            params![id, drive_id, document_id, room_id],
        )?;
        Ok(())
    }

    pub fn class_origin(&self, id: &str) -> AppResult<Option<ClassOrigin>> {
        let row = self
            .conn
            .query_row(
                "SELECT class_drive_id, class_document_id, class_room_id
                   FROM documents WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(match row {
            Some((Some(drive_id), Some(document_id), room_id)) => Some(ClassOrigin {
                drive_id,
                document_id,
                room_id,
            }),
            _ => None,
        })
    }
}

/// Where a note came from, when it came from a class box.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassOrigin {
    pub drive_id: String,
    pub document_id: String,
    /// The room that carries what everyone writes on it. `None` for a document
    /// that has none, which is a note nobody can share.
    pub room_id: Option<String>,
}

/// Filters for the library list.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub trashed: bool,
    #[serde(rename = "folderId")]
    pub folder_id: Option<String>,
    /// When no folder is given, restrict to notes that are in no folder.
    #[serde(default, rename = "rootOnly")]
    pub root_only: bool,
    #[serde(rename = "tagId")]
    pub tag_id: Option<String>,
    pub text: Option<String>,
    /// "updated" (default), "created" or "title".
    pub sort: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    pub ord: i64,
    #[serde(rename = "noteCount")]
    pub note_count: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tag {
    pub id: String,
    pub name: String,
    pub color: String,
}

/// Turns user input into an FTS5 prefix query, or `None` if there is nothing
/// left to search for.
///
/// Raw input cannot go into MATCH: FTS5 treats `"`, `*`, `:`, `^` and `-` as
/// syntax, so a stray quote is a syntax error rather than a search for a quote.
/// Each surviving term is quoted and given a prefix wildcard, which is what
/// people expect from a search box.
fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        // Strip the characters FTS5 would read as operators, then keep only
        // terms that still have something in them.
        .map(|term| {
            term.chars()
                .filter(|c| !matches!(c, '"' | '*' | ':' | '^' | '-' | '(' | ')'))
                .collect::<String>()
        })
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{term}\"*"))
        .collect();

    (!terms.is_empty()).then(|| terms.join(" "))
}

#[cfg(test)]
mod fts_tests {
    use super::fts_query;

    #[test]
    fn builds_a_prefix_query_per_term() {
        assert_eq!(fts_query("algebra"), Some("\"algebra\"*".into()));
        assert_eq!(
            fts_query("algebra notes"),
            Some("\"algebra\"* \"notes\"*".into())
        );
    }

    #[test]
    fn strips_fts_operators_rather_than_passing_them_through() {
        assert_eq!(fts_query("a:b"), Some("\"ab\"*".into()));
        assert_eq!(fts_query("-foo"), Some("\"foo\"*".into()));
    }

    #[test]
    fn returns_none_when_nothing_searchable_remains() {
        // These would each build an empty MATCH expression, which FTS5 rejects.
        for junk in ["", "   ", "\"", "*", "^^^", "-", "()"] {
            assert_eq!(fts_query(junk), None, "expected None for {junk:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Document store — one `.mmnote` SQLite file per note
// ---------------------------------------------------------------------------

pub struct NoteStore {
    conn: Connection,
}

impl NoteStore {
    pub fn open(path: &Path) -> AppResult<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> AppResult<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS doc_meta (
                k              INTEGER PRIMARY KEY CHECK (k = 0),
                schema_version INTEGER NOT NULL,
                document_id    TEXT NOT NULL,
                root_id        TEXT NOT NULL,
                title          TEXT NOT NULL,
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL,
                revision       INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS models (
                id         TEXT PRIMARY KEY,
                parent_id  TEXT,
                ord        INTEGER NOT NULL DEFAULT 0,
                model_type TEXT NOT NULL,
                props_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_models_parent ON models(parent_id, ord);
            -- Which strokes the note's classroom already has, by the id it
            -- knows them by. The note itself cannot answer this: a stroke the
            -- user erased is gone from the tree, and erasing is exactly what
            -- has to be reported.
            -- How far this note has read each of its classroom booths, so a
            -- reconnect asks for what is new rather than replaying a term's
            -- worth of lessons into an open editor.
            CREATE TABLE IF NOT EXISTS room_booths (
                booth_id      TEXT PRIMARY KEY,
                last_sequence INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS room_strokes (
                stroke_id  TEXT PRIMARY KEY,
                element_id TEXT NOT NULL,
                layer_id   TEXT NOT NULL
            );

            -- The same bookkeeping as room_strokes, for units that can be
            -- edited in place rather than only drawn once and erased: a
            -- shape, text box or form keeps its own id for life, so telling
            -- "unchanged" from "edited" needs more than presence — hence
            -- fingerprint, a hash of the unit's own subtree as it stood the
            -- last time this note told the room about it.
            CREATE TABLE IF NOT EXISTS room_units (
                unit_id     TEXT PRIMARY KEY,
                element_id  TEXT NOT NULL,
                layer_id    TEXT NOT NULL,
                fingerprint TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS assets (
                id       TEXT PRIMARY KEY,
                mime     TEXT NOT NULL,
                byte_len INTEGER NOT NULL,
                data     BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS thumbnails (
                page_id  TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                png      BLOB NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    /// Replaces the whole model tree in one transaction.
    ///
    /// Rewriting every row on save is the right trade here: a note is a few
    /// thousand rows at most, SQLite absorbs that in a single transaction in a
    /// few milliseconds, and it removes an entire class of "the tree on disk
    /// drifted from the tree in memory" bugs. Row-level diffing only becomes
    /// worth its complexity once documents get large enough to notice.
    pub fn write_tree(
        &mut self,
        tree: &GenericTree,
        title: &str,
        created_at: &str,
        updated_at: &str,
        revision: i64,
    ) -> AppResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM models", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO models(id, parent_id, ord, model_type, props_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for node in tree.walk() {
                let ord = node
                    .parent_id
                    .as_ref()
                    .and_then(|p| tree.models.get(p))
                    .and_then(|p| p.children.iter().position(|c| c == &node.id))
                    .unwrap_or(0) as i64;
                stmt.execute(params![
                    node.id,
                    node.parent_id,
                    ord,
                    node.model_type,
                    serde_json::to_string(&node.props)?
                ])?;
            }
        }
        tx.execute(
            r#"
            INSERT INTO doc_meta(k, schema_version, document_id, root_id,
                                 title, created_at, updated_at, revision)
            VALUES (0, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(k) DO UPDATE SET
                schema_version = excluded.schema_version,
                document_id    = excluded.document_id,
                root_id        = excluded.root_id,
                title          = excluded.title,
                updated_at     = excluded.updated_at,
                revision       = excluded.revision
            "#,
            params![
                SCHEMA_VERSION,
                tree.root_id,
                tree.root_id,
                title,
                created_at,
                updated_at,
                revision
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn read_tree(&self) -> AppResult<GenericTree> {
        let root_id: String = self
            .conn
            .query_row("SELECT root_id FROM doc_meta WHERE k = 0", [], |r| r.get(0))
            .optional()?
            .ok_or_else(|| AppError::other("note file has no document metadata"))?;

        let mut stmt = self.conn.prepare(
            "SELECT id, parent_id, model_type, props_json FROM models
             ORDER BY parent_id IS NULL DESC, parent_id, ord",
        )?;
        let rows = stmt.query_map([], |row| {
            let props_json: String = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                props_json,
            ))
        })?;

        let mut models: HashMap<String, GenericModel> = HashMap::new();
        let mut order: Vec<(String, Option<String>)> = Vec::new();
        for row in rows {
            let (id, parent_id, model_type, props_json) = row?;
            let props: serde_json::Value =
                serde_json::from_str(&props_json).unwrap_or(serde_json::json!({}));
            order.push((id.clone(), parent_id.clone()));
            models.insert(
                id.clone(),
                GenericModel {
                    id,
                    parent_id,
                    model_type,
                    props,
                    children: Vec::new(),
                },
            );
        }

        // Rebuild child lists from the ordered scan, so sibling order is the
        // order the rows were written in.
        for (id, parent_id) in order {
            if let Some(parent_id) = parent_id {
                if let Some(parent) = models.get_mut(&parent_id) {
                    parent.children.push(id);
                }
            }
        }

        Ok(GenericTree { root_id, models })
    }

    pub fn meta(&self) -> AppResult<(String, String, String, i64, i64)> {
        let row = self.conn.query_row(
            "SELECT title, created_at, updated_at, revision,
                    (SELECT COUNT(*) FROM models WHERE model_type = '$page')
             FROM doc_meta WHERE k = 0",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )?;
        Ok(row)
    }

    // -- classroom bookkeeping -------------------------------------------------

    pub fn remember_room_stroke(
        &self,
        stroke_id: &str,
        element_id: &str,
        layer_id: &str,
    ) -> AppResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO room_strokes(stroke_id, element_id, layer_id)
             VALUES (?1, ?2, ?3)",
            params![stroke_id, element_id, layer_id],
        )?;
        Ok(())
    }

    pub fn room_strokes(&self) -> AppResult<Vec<crate::collabo::send::Ledger>> {
        let mut stmt = self.conn.prepare(
            "SELECT stroke_id, element_id, layer_id FROM room_strokes ORDER BY stroke_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::collabo::send::Ledger {
                stroke_id: r.get(0)?,
                element_id: r.get(1)?,
                layer_id: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The sequence each booth has been read up to.
    pub fn booth_marks(&self) -> AppResult<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT booth_id, last_sequence FROM room_booths")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Moves a booth's mark forward. Never backwards: a direction that arrives
    /// out of order must not make the note ask for everything again.
    pub fn mark_booth(&self, booth_id: &str, sequence: i64) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO room_booths(booth_id, last_sequence) VALUES (?1, ?2)
             ON CONFLICT(booth_id) DO UPDATE SET
                 last_sequence = MAX(last_sequence, excluded.last_sequence)",
            params![booth_id, sequence],
        )?;
        Ok(())
    }

    /// Forgets how far every booth was read, so the next connection asks for
    /// the room from the beginning. What comes back is deduplicated on the way
    /// in; the point is to stop a mark that has run ahead of reality from
    /// hiding everything after it.
    pub fn forget_booth_marks(&self) -> AppResult<()> {
        self.conn.execute("DELETE FROM room_booths", [])?;
        Ok(())
    }

    pub fn forget_room_stroke(&self, stroke_id: &str) -> AppResult<()> {
        self.conn.execute(
            "DELETE FROM room_strokes WHERE stroke_id = ?1",
            params![stroke_id],
        )?;
        Ok(())
    }

    pub fn remember_room_unit(
        &self,
        unit_id: &str,
        element_id: &str,
        layer_id: &str,
        fingerprint: &str,
    ) -> AppResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO room_units(unit_id, element_id, layer_id, fingerprint)
             VALUES (?1, ?2, ?3, ?4)",
            params![unit_id, element_id, layer_id, fingerprint],
        )?;
        Ok(())
    }

    pub fn room_units(&self) -> AppResult<Vec<crate::collabo::send::UnitLedger>> {
        let mut stmt = self.conn.prepare(
            "SELECT unit_id, element_id, layer_id, fingerprint FROM room_units ORDER BY unit_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::collabo::send::UnitLedger {
                unit_id: r.get(0)?,
                element_id: r.get(1)?,
                layer_id: r.get(2)?,
                fingerprint: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn forget_room_unit(&self, unit_id: &str) -> AppResult<()> {
        self.conn.execute(
            "DELETE FROM room_units WHERE unit_id = ?1",
            params![unit_id],
        )?;
        Ok(())
    }

    // -- assets --------------------------------------------------------------

    pub fn put_asset(&self, id: &str, mime: &str, data: &[u8]) -> AppResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO assets(id, mime, byte_len, data) VALUES (?1, ?2, ?3, ?4)",
            params![id, mime, data.len() as i64, data],
        )?;
        Ok(())
    }

    pub fn get_asset(&self, id: &str) -> AppResult<(String, Vec<u8>)> {
        self.conn
            .query_row(
                "SELECT mime, data FROM assets WHERE id = ?1",
                params![id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
            .ok_or_else(|| AppError::AssetNotFound(id.to_string()))
    }

    pub fn asset_ids(&self) -> AppResult<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT id FROM assets")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // -- thumbnails ----------------------------------------------------------

    pub fn put_thumbnail(&self, page_id: &str, revision: i64, png: &[u8]) -> AppResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO thumbnails(page_id, revision, png) VALUES (?1, ?2, ?3)",
            params![page_id, revision, png],
        )?;
        Ok(())
    }

    pub fn get_thumbnail(&self, page_id: &str) -> AppResult<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT png FROM thumbnails WHERE page_id = ?1",
                params![page_id],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?)
    }
}

pub fn to_png_data_url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}
