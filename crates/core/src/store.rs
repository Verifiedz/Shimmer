//! Namespaced, transactional access to a module's own data.
//!
//! A module holds a [`NamespacedStore`]; the daemon supplies the [`StoreBackend`] behind it.
//! The backend trait lives here so `core` stays free of internal dependencies and modules
//! can be tested against an in-memory backend (§12 rule 13). Nothing here exposes a path
//! to the data root (§5, forbidden in `Ctx`).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use ulid::Ulid;

use crate::bus::validate_topic;
use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::event::Event;
use crate::ids::{ModuleId, TaskId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    Put { path: String, bytes: Vec<u8> },
    Delete { path: String },
}

/// Everything one transaction will do. All of it happens or none of it does (§7.1).
#[derive(Clone, Debug, PartialEq)]
pub struct TxPlan {
    /// Transaction id, used to name the staging directory.
    pub id: Ulid,
    /// At most one write per path, paths relative to the module's namespace.
    pub writes: Vec<Write>,
    /// Ids assigned at staging time so roll-forward can tell whether one was already logged.
    pub events: Vec<Event>,
}

/// Implemented by the store (via the daemon). All paths are relative to `namespace`.
pub trait StoreBackend: Send + Sync {
    fn read(&self, namespace: &str, path: &str) -> Result<Option<Vec<u8>>>;
    /// Every file at or under directory `prefix` (`""` = all), sorted, namespace-relative.
    fn list(&self, namespace: &str, prefix: &str) -> Result<Vec<String>>;
    fn commit(&self, namespace: &str, plan: TxPlan) -> Result<()>;
}

/// Reject anything that could leave the namespace or confuse the filesystem.
pub fn validate_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.len() > 512
        || path.starts_with('/')
        || path.ends_with('/')
        || path.split('/').any(|s| s.is_empty() || s == "." || s == ".." || s.contains(['\\', '\0']));
    if bad {
        return Err(Error::invalid_params(format!("invalid store path '{path}'")));
    }
    Ok(())
}

fn validate_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty() {
        Ok(())
    } else {
        validate_path(prefix)
    }
}

#[derive(Clone)]
pub struct NamespacedStore {
    namespace: String,
    source: ModuleId,
    backend: Arc<dyn StoreBackend>,
    clock: Clock,
    /// Stamped on every event a transaction emits: the task this handle runs in (ADR 0035 §5.1).
    cause: Option<TaskId>,
}

impl NamespacedStore {
    pub fn new(namespace: impl Into<String>, source: ModuleId, backend: Arc<dyn StoreBackend>, clock: Clock) -> Self {
        Self { namespace: namespace.into(), source, backend, clock, cause: None }
    }

    /// A copy whose transactions' events say they came from `task`.
    pub fn for_task(&self, task: TaskId) -> Self {
        Self { cause: Some(task), ..self.clone() }
    }

    pub fn read(&self, path: &str) -> Result<Option<Vec<u8>>> {
        validate_path(path)?;
        self.backend.read(&self.namespace, path)
    }

    pub fn read_string(&self, path: &str) -> Result<Option<String>> {
        self.read(path)?
            .map(|b| String::from_utf8(b).map_err(|_| Error::internal(format!("'{path}' is not UTF-8"))))
            .transpose()
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<String>> {
        validate_prefix(prefix)?;
        self.backend.list(&self.namespace, prefix)
    }

    /// Atomic single-file write. Use [`transaction`](Self::transaction) for anything more.
    pub fn write(&self, path: &str, bytes: impl Into<Vec<u8>>) -> Result<()> {
        let bytes = bytes.into();
        self.transaction(|tx| tx.put(path, bytes))
    }

    pub fn delete(&self, path: &str) -> Result<()> {
        self.transaction(|tx| tx.delete(path))
    }

    /// Stage writes and events in `f`; they commit together when it returns `Ok`, and none of
    /// it is visible if it returns `Err`. Make each transaction a meaningful whole.
    pub fn transaction<T>(&self, f: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        let mut tx = Tx { store: self, staged: BTreeMap::new(), events: Vec::new() };
        let out = f(&mut tx)?;
        let Tx { staged, events, .. } = tx;
        if staged.is_empty() && events.is_empty() {
            return Ok(out);
        }
        let writes = staged
            .into_iter()
            .map(|(path, bytes)| match bytes {
                Some(bytes) => Write::Put { path, bytes },
                None => Write::Delete { path },
            })
            .collect();
        let plan = TxPlan { id: Ulid::from_datetime(self.clock.now().into()), writes, events };
        self.backend.commit(&self.namespace, plan)?;
        Ok(out)
    }
}

/// A transaction being staged. Reads see this transaction's own uncommitted writes.
pub struct Tx<'a> {
    store: &'a NamespacedStore,
    /// `None` = delete.
    staged: BTreeMap<String, Option<Vec<u8>>>,
    events: Vec<Event>,
}

impl Tx<'_> {
    pub fn read(&self, path: &str) -> Result<Option<Vec<u8>>> {
        validate_path(path)?;
        match self.staged.get(path) {
            Some(staged) => Ok(staged.clone()),
            None => self.store.backend.read(&self.store.namespace, path),
        }
    }

    pub fn put(&mut self, path: &str, bytes: impl Into<Vec<u8>>) -> Result<()> {
        validate_path(path)?;
        self.staged.insert(path.to_owned(), Some(bytes.into()));
        Ok(())
    }

    pub fn delete(&mut self, path: &str) -> Result<()> {
        validate_path(path)?;
        self.staged.insert(path.to_owned(), None);
        Ok(())
    }

    /// Queue an event to be logged and published atomically with the writes.
    pub fn emit(&mut self, topic: &str, payload: Value) -> Result<()> {
        validate_topic(&self.store.source, topic)?;
        self.events.push(
            Event::new(self.store.source.clone(), topic, payload, self.store.clock.now()).caused_by(self.store.cause),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing::MemBackend;

    fn store() -> (NamespacedStore, Arc<MemBackend>) {
        let mem = Arc::new(MemBackend::default());
        let s = NamespacedStore::new("records", "records".into(), mem.clone(), Clock::system());
        (s, mem)
    }

    #[test]
    fn paths_cannot_escape() {
        let (s, _) = store();
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "a/", "a\\b", "."] {
            assert!(s.read(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(s.read("items/two-sum.toml").unwrap().is_none());
    }

    #[test]
    fn transaction_is_all_or_nothing() {
        let (s, mem) = store();
        let r: Result<()> = s.transaction(|tx| {
            tx.put("a", "1")?;
            tx.emit("records.item.created", json!({}))?;
            Err(Error::module_error("boom"))
        });
        assert!(r.is_err());
        assert!(s.read("a").unwrap().is_none());
        assert!(mem.events().is_empty());

        s.transaction(|tx| {
            tx.put("a", "1")?;
            tx.put("b", "2")?;
            assert_eq!(tx.read("a")?.as_deref(), Some(&b"1"[..]), "read-your-writes");
            tx.emit("records.item.created", json!({"id": "a"}))
        })
        .unwrap();
        assert_eq!(s.list("").unwrap(), ["a", "b"]);
        assert_eq!(mem.events().len(), 1);
    }

    #[test]
    fn cannot_emit_another_modules_topic() {
        let (s, _) = store();
        let r = s.transaction(|tx| tx.emit("calendar.date.registered", json!({})));
        assert!(r.is_err());
    }
}
