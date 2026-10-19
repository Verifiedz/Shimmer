//! `shimmer-store`: files are the truth, SQLite is a disposable index (CLAUDE.md §1.4, §7).
//!
//! * [`Store`] owns `$SHIMMER_HOME`: atomic writes, staged transactions with crash recovery
//!   (§7.1) and the append-only JSONL event log. It implements `shimmer_core::StoreBackend`.
//! * [`Index`] is a derived projection of the event log. Delete it and it rebuilds.
//!
//! Only the daemon opens a `Store` for writing: one writer, always.

mod atomic;
mod index;
mod log;
mod tx;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use shimmer_core::ids::is_valid_name;
use shimmer_core::store::validate_path;
use shimmer_core::{Error, Event, Result, StoreBackend, TxPlan};

pub use atomic::write_atomic;
pub use index::{Index, IndexStats};
pub use log::EventScan;
pub use tx::Recovery;

/// Shipped so `$SHIMMER_HOME` is committable as-is: the whole machine-to-machine story.
const GITIGNORE: &str = "\
# Derived or machine-local. Everything else in this directory is the source of truth.
index.sqlite*
cache/
logs/
.staging/
.daemon.lock
daemon.sock
";

pub struct Store {
    home: PathBuf,
    /// Serialises commits and log appends. Cheap: one writer process anyway. Holds the
    /// committed transactions whose last step failed, retried before the next commit (#260).
    write_lock: Mutex<Vec<PathBuf>>,
}

impl Store {
    /// Create the directory skeleton if needed. Does not run recovery; call
    /// [`recover`](Self::recover) before serving.
    pub fn open(home: impl Into<PathBuf>) -> Result<Self> {
        let home = home.into();
        for dir in ["data", "events", ".staging", "logs"] {
            fs::create_dir_all(home.join(dir))?;
        }
        let ignore = home.join(".gitignore");
        if !ignore.exists() {
            atomic::write_atomic(&ignore, GITIGNORE.as_bytes())?;
        }
        Ok(Self { home, write_lock: Mutex::new(Vec::new()) })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn index_path(&self) -> PathBuf {
        self.home.join("index.sqlite")
    }

    /// Finish or discard every in-flight transaction left by a crash. Idempotent.
    pub fn recover(&self) -> Result<Recovery> {
        let _g = self.lock();
        tx::recover(&self.home)
    }

    /// Append to the event log. Used for events that are not part of a store transaction.
    pub fn append_event(&self, event: &Event) -> Result<()> {
        let _g = self.lock();
        log::append(&self.home, event)
    }

    /// Every logged event, oldest first, tolerating torn or corrupt lines.
    pub fn read_events(&self) -> Result<EventScan> {
        log::read_all(&self.home)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<PathBuf>> {
        // A panic mid-commit leaves staging state that recovery handles; the lock itself
        // guards no data, so poisoning carries no information.
        self.write_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn namespace_dir(&self, namespace: &str) -> Result<PathBuf> {
        if !is_valid_name(namespace) {
            return Err(Error::invalid_params(format!("invalid namespace '{namespace}'")));
        }
        Ok(self.home.join("data").join(namespace))
    }
}

impl StoreBackend for Store {
    fn read(&self, namespace: &str, path: &str) -> Result<Option<Vec<u8>>> {
        validate_path(path)?;
        match fs::read(self.namespace_dir(namespace)?.join(path)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn list(&self, namespace: &str, prefix: &str) -> Result<Vec<String>> {
        let root = self.namespace_dir(namespace)?;
        let start = if prefix.is_empty() {
            root.clone()
        } else {
            validate_path(prefix)?;
            root.join(prefix)
        };
        let mut out = Vec::new();
        atomic::walk_files(&root, &start, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn commit(&self, namespace: &str, plan: TxPlan) -> Result<()> {
        let dir = self.namespace_dir(namespace)?;
        for w in &plan.writes {
            let (shimmer_core::Write::Put { path, .. } | shimmer_core::Write::Delete { path }) = w;
            validate_path(path)?;
        }
        let mut pending = self.lock();
        tx::commit(&self.home, &dir, namespace, plan, &mut pending)
    }
}
