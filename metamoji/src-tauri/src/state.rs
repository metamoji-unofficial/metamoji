//! Application state and the explicit boot sequence.
//!
//! docs/14 §3 catalogues the original's startup: a long chain of static
//! singletons each with its own `initialize()`, ordered only by the order of
//! calls in `Application.onCreate()`, with no dependency injection anywhere.
//! docs/14 §4's recommendation is to replace that with one state object built
//! in an explicit order and a "ready" signal to the UI. That is what this is.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use crate::error::AppResult;
use crate::model::AppStatus;
use crate::storage::{AppPaths, Catalog, NoteStore};

pub struct AppState {
    pub paths: AppPaths,
    pub catalog: Mutex<Catalog>,
    /// Note files held open while the editor has them loaded. Keeping the
    /// connection open avoids re-opening SQLite on every autosave.
    pub open_notes: RwLock<HashMap<String, Arc<Mutex<NoteStore>>>>,
    pub schema_version: i64,
}

impl AppState {
    /// Boot, in this order and no other:
    ///   1. resolve and create directories
    ///   2. open the catalog and run migrations
    ///   3. publish state
    ///
    /// Anything that needs a later step must be a method, not a constructor
    /// side effect — that discipline is what the original was missing.
    pub fn boot(data_dir: PathBuf) -> AppResult<Self> {
        let paths = AppPaths::new(data_dir)?;
        let catalog = Catalog::open(&paths.library_db)?;
        let schema_version = catalog.schema_version();

        Ok(Self {
            paths,
            catalog: Mutex::new(catalog),
            open_notes: RwLock::new(HashMap::new()),
            schema_version,
        })
    }

    pub fn status(&self) -> AppStatus {
        AppStatus {
            ready: true,
            data_dir: self.paths.data_dir.to_string_lossy().to_string(),
            schema_version: self.schema_version,
            // Phase B's login gate reads this. It is the direct successor of the
            // original's `isNeedLogin` StateFlow (docs/14 §3), kept present from
            // the start so adding sync does not change the frontend's shape.
            needs_login: false,
        }
    }

    /// Returns the open note store for `id`, opening the file if needed.
    pub fn note(&self, id: &str) -> AppResult<Arc<Mutex<NoteStore>>> {
        if let Some(store) = self.open_notes.read().unwrap().get(id) {
            return Ok(Arc::clone(store));
        }
        let path = self.paths.note_path(id)?;
        let store = Arc::new(Mutex::new(NoteStore::open(&path)?));
        self.open_notes
            .write()
            .unwrap()
            .insert(id.to_string(), Arc::clone(&store));
        Ok(store)
    }

    pub fn close_note(&self, id: &str) {
        self.open_notes.write().unwrap().remove(id);
    }
}
