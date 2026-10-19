//! Multi-file transactions (§7.1).
//!
//! 1. Stage every write under `.staging/<txid>/`, plus a `manifest.json` describing the
//!    transaction and the events to log; fsync all of it.
//! 2. Write the `COMMITTED` marker. This is the commit point.
//! 3. Apply: put files into place, delete, append events not already logged, remove staging.
//!
//! Step 3 is idempotent and is exactly what recovery runs, so a crash anywhere after step 2
//! rolls forward and a crash before it rolls back (staging deleted).
//!
//! Step 2 is the commit point in every sense: once `COMMITTED` is written the transaction has
//! happened, so a failure in step 3 is not reported as a failed write (a retry would make it
//! twice, #260). Step 3 is retried before the next commit and on the next start.
//!
//! Recovery takes each staging folder on its own. One that can't be applied (a damaged
//! manifest, a staged file gone) is moved to `.staging/broken/`, kept, never deleted, since it
//! may hold committed data, and the daemon starts anyway (#260 finding 2).

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shimmer_core::{Error, Event, Result, TxPlan, Write};

use crate::atomic::{fsync_dir, write_atomic};
use crate::log;

const COMMITTED: &str = "COMMITTED";
const MANIFEST: &str = "manifest.json";

#[derive(Serialize, Deserialize)]
struct Staged {
    namespace: String,
    ops: Vec<StagedOp>,
    events: Vec<Event>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum StagedOp {
    /// Namespace-relative destination, and the staged copy under `files/`.
    Put {
        path: String,
        file: String,
    },
    Delete {
        path: String,
    },
}

/// Damaged staging folders, moved aside by recovery, kept for a person to look at.
pub const BROKEN: &str = "broken";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    pub rolled_forward: usize,
    pub discarded: usize,
    /// Committed transactions that couldn't be applied, moved to `.staging/broken/<txid>/`:
    /// the id and why.
    pub broken: Vec<(String, String)>,
}

/// `pending`: committed transactions whose step 3 failed earlier in this run, retried first.
pub fn commit(home: &Path, ns_dir: &Path, namespace: &str, plan: TxPlan, pending: &mut Vec<PathBuf>) -> Result<()> {
    retry(home, pending);
    // Fast path: a lone file write needs no staging (§7.1, first bullet).
    if plan.events.is_empty() {
        if let [Write::Put { path, bytes }] = plan.writes.as_slice() {
            return write_atomic(&ns_dir.join(path), bytes).map_err(Into::into);
        }
    }
    let dir = stage(home, namespace, &plan)?;
    // Committed: it has happened. A failure from here is finished later, never reported as a
    // failed write, which a caller would retry into a second copy.
    if let Err(e) = apply(home, &dir, None) {
        tracing::error!(tx = %dir.display(), error = %e, "a committed write couldn't be finished; retried before the next one");
        pending.push(dir);
    }
    Ok(())
}

/// Finish what earlier commits couldn't. Still failing: kept, tried again next time (and by
/// recovery on the next start).
fn retry(home: &Path, pending: &mut Vec<PathBuf>) {
    if pending.is_empty() {
        return;
    }
    let mut logged = log::Logged::default();
    pending.retain(|dir| match apply(home, dir, Some(&mut logged)) {
        Ok(()) => false,
        Err(e) => {
            tracing::warn!(tx = %dir.display(), error = %e, "a committed write still can't be finished");
            true
        }
    });
}

fn staging_root(home: &Path) -> PathBuf {
    home.join(".staging")
}

/// Steps 1 and 2. Returns the staging directory, committed.
fn stage(home: &Path, namespace: &str, plan: &TxPlan) -> Result<PathBuf> {
    let dir = staging_root(home).join(plan.id.to_string());
    fs::create_dir(&dir).map_err(|e| Error::conflict(format!("cannot create staging dir: {e}")))?;
    fs::create_dir(dir.join("files"))?;

    let mut ops = Vec::with_capacity(plan.writes.len());
    for (i, w) in plan.writes.iter().enumerate() {
        match w {
            Write::Put { path, bytes } => {
                let file = format!("files/{i}");
                let mut f = File::create(dir.join(&file))?;
                f.write_all(bytes)?;
                f.sync_all()?;
                ops.push(StagedOp::Put { path: path.clone(), file });
            }
            Write::Delete { path } => ops.push(StagedOp::Delete { path: path.clone() }),
        }
    }
    let manifest = Staged { namespace: namespace.to_owned(), ops, events: plan.events.clone() };
    let json = serde_json::to_vec(&manifest).map_err(|e| Error::internal(format!("encode tx: {e}")))?;
    write_atomic(&dir.join(MANIFEST), &json)?;
    fsync_dir(&dir.join("files"))?;
    fsync_dir(&dir)?;

    File::create(dir.join(COMMITTED))?.sync_all()?;
    fsync_dir(&dir)?;
    fsync_dir(&staging_root(home))?;
    Ok(dir)
}

/// Step 3. Safe to run any number of times on a committed staging directory. `logged`: check
/// each event against the log before appending it, for a transaction a crash or a failure may
/// have half applied; `None` for a fresh commit, whose events can't be logged yet.
fn apply(home: &Path, dir: &Path, logged: Option<&mut log::Logged>) -> Result<()> {
    let staged: Staged = serde_json::from_slice(&fs::read(dir.join(MANIFEST))?)
        .map_err(|e| Error::internal(format!("corrupt staged transaction {}: {e}", dir.display())))?;
    let ns_dir = home.join("data").join(&staged.namespace);

    for op in &staged.ops {
        match op {
            StagedOp::Put { path, file } => {
                // Copy then rename inside the destination directory, so this also works
                // if staging and data ever sit on different filesystems.
                write_atomic(&ns_dir.join(path), &fs::read(dir.join(file))?)?;
            }
            StagedOp::Delete { path } => {
                let dest = ns_dir.join(path);
                match fs::remove_file(&dest) {
                    Ok(()) => {
                        if let Some(parent) = dest.parent() {
                            fsync_dir(parent)?;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    match logged {
        Some(logged) => {
            for event in &staged.events {
                logged.append_once(home, event)?;
            }
        }
        None => {
            for event in &staged.events {
                log::append(home, event)?;
            }
        }
    }
    fs::remove_dir_all(dir)?;
    fsync_dir(&staging_root(home))?;
    Ok(())
}

pub fn recover(home: &Path) -> Result<Recovery> {
    let mut r = Recovery::default();
    let mut logged = log::Logged::default();
    for entry in fs::read_dir(staging_root(home))? {
        let dir = entry?.path();
        if !dir.is_dir() || dir.file_name().is_some_and(|n| n == BROKEN) {
            continue;
        }
        let id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if dir.join(COMMITTED).exists() {
            match apply(home, &dir, Some(&mut logged)) {
                Ok(()) => r.rolled_forward += 1,
                Err(e) => {
                    let why = e.message.clone();
                    let to = staging_root(home).join(BROKEN).join(&id);
                    let moved =
                        fs::create_dir_all(staging_root(home).join(BROKEN)).and_then(|()| fs::rename(&dir, &to));
                    match moved {
                        Ok(()) => {
                            tracing::error!(tx = %id, error = %why, kept = %to.display(), "a committed transaction couldn't be applied; kept aside")
                        }
                        Err(m) => {
                            tracing::error!(tx = %id, error = %why, move_error = %m, "a committed transaction couldn't be applied or moved aside; left in place")
                        }
                    }
                    r.broken.push((id, why));
                }
            }
        } else if let Err(e) = fs::remove_dir_all(&dir) {
            // Never committed, so nothing in it happened; a leftover can't stop the daemon.
            tracing::warn!(tx = %id, error = %e, "couldn't delete an uncommitted staging folder");
        } else {
            r.discarded += 1;
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shimmer_core::{Clock, StoreBackend};
    use tempfile::TempDir;
    use ulid::Ulid;

    use super::*;
    use crate::Store;

    fn event(topic: &str) -> Event {
        Event::new("records".into(), topic, json!({}), Clock::system().now())
    }

    fn plan(events: Vec<Event>) -> TxPlan {
        TxPlan {
            id: Ulid::new(),
            writes: vec![
                Write::Put { path: "a.toml".into(), bytes: b"a".to_vec() },
                Write::Put { path: "sub/b.toml".into(), bytes: b"b".to_vec() },
            ],
            events,
        }
    }

    fn logged(store: &Store) -> Vec<Event> {
        store.read_events().unwrap().events
    }

    #[test]
    fn commit_applies_files_and_logs_events() {
        let home = TempDir::new().unwrap();
        let store = Store::open(home.path()).unwrap();
        let ev = event("records.item.created");
        store.commit("records", plan(vec![ev.clone()])).unwrap();

        assert_eq!(store.read("records", "sub/b.toml").unwrap().unwrap(), b"b");
        assert_eq!(store.list("records", "").unwrap(), ["a.toml", "sub/b.toml"]);
        assert_eq!(store.list("records", "sub").unwrap(), ["sub/b.toml"]);
        assert_eq!(logged(&store), [ev]);
        assert_eq!(fs::read_dir(home.path().join(".staging")).unwrap().count(), 0);
    }

    #[test]
    fn crash_before_commit_marker_rolls_back() {
        let home = TempDir::new().unwrap();
        let store = Store::open(home.path()).unwrap();
        let dir = stage(home.path(), "records", &plan(vec![event("records.item.created")])).unwrap();
        fs::remove_file(dir.join(COMMITTED)).unwrap(); // crash landed just before the marker

        let r = store.recover().unwrap();
        assert_eq!(r, Recovery { rolled_forward: 0, discarded: 1, broken: vec![] });
        assert!(store.read("records", "a.toml").unwrap().is_none());
        assert!(logged(&store).is_empty());
    }

    #[test]
    fn crash_after_commit_marker_rolls_forward_exactly_once() {
        let home = TempDir::new().unwrap();
        let store = Store::open(home.path()).unwrap();
        let ev = event("records.item.created");
        stage(home.path(), "records", &plan(vec![ev.clone()])).unwrap(); // marker written, nothing applied

        assert_eq!(store.recover().unwrap().rolled_forward, 1);
        assert_eq!(store.read("records", "a.toml").unwrap().unwrap(), b"a");
        assert_eq!(logged(&store), [ev]);
        assert_eq!(store.recover().unwrap(), Recovery::default(), "recovery is idempotent");
    }

    #[test]
    fn crash_mid_apply_does_not_duplicate_the_event() {
        let home = TempDir::new().unwrap();
        let store = Store::open(home.path()).unwrap();
        let ev = event("records.item.created");
        let dir = stage(home.path(), "records", &plan(vec![ev.clone()])).unwrap();

        // Simulate dying after files and the event were applied but before staging was removed.
        let ns = home.path().join("data/records");
        write_atomic(&ns.join("a.toml"), b"a").unwrap();
        log::append(home.path(), &ev).unwrap();
        assert!(dir.exists());

        store.recover().unwrap();
        assert_eq!(logged(&store), [ev], "event id already in the log is not appended again");
        assert_eq!(store.read("records", "sub/b.toml").unwrap().unwrap(), b"b");
    }

    #[test]
    fn delete_and_single_write_fast_path() {
        let home = TempDir::new().unwrap();
        let store = Store::open(home.path()).unwrap();
        let put = |bytes: &[u8]| TxPlan {
            id: Ulid::new(),
            writes: vec![Write::Put { path: "x".into(), bytes: bytes.to_vec() }],
            events: vec![],
        };
        store.commit("records", put(b"1")).unwrap();
        store.commit("records", put(b"2")).unwrap();
        assert_eq!(store.read("records", "x").unwrap().unwrap(), b"2");
        assert_eq!(fs::read_dir(home.path().join(".staging")).unwrap().count(), 0);

        let del = TxPlan { id: Ulid::new(), writes: vec![Write::Delete { path: "x".into() }], events: vec![] };
        store.commit("records", del).unwrap();
        assert!(store.read("records", "x").unwrap().is_none());
    }

    #[test]
    fn namespaces_are_isolated_and_validated() {
        let home = TempDir::new().unwrap();
        let store = Store::open(home.path()).unwrap();
        assert!(store.read("../etc", "passwd").is_err());
        assert!(store.read("records", "../calendar/x").is_err());
    }

    #[test]
    fn open_ships_gitignore_and_survives_torn_log_line() {
        let home = TempDir::new().unwrap();
        let store = Store::open(home.path()).unwrap();
        let gi = fs::read_to_string(home.path().join(".gitignore")).unwrap();
        for entry in ["index.sqlite*", "cache/", "logs/", ".staging/"] {
            assert!(gi.contains(entry), "{entry}");
        }

        let e1 = event("records.item.created");
        store.append_event(&e1).unwrap();
        let path = home.path().join("events").join(format!("{}.jsonl", e1.at.format("%Y-%m-%d")));
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"id\":\"torn").unwrap();
        let e2 = event("records.item.updated");
        store.append_event(&e2).unwrap();

        let scan = store.read_events().unwrap();
        assert_eq!(scan.events, [e1, e2], "event after a torn line is not lost");
        assert_eq!(scan.skipped, 1);
    }

    /// #260 finding 2: a committed staging folder that can't be applied no longer stops every
    /// start. It's moved to `.staging/broken/`, kept whole, and every other one still recovers.
    #[test]
    fn a_damaged_staging_folder_is_kept_aside_and_the_rest_recovers() {
        let home = TempDir::new().unwrap();
        let store = Store::open(home.path()).unwrap();
        let bad_manifest = stage(home.path(), "records", &plan(vec![])).unwrap();
        fs::write(bad_manifest.join(MANIFEST), b"{ not json").unwrap();
        let missing_file = stage(home.path(), "records", &plan(vec![])).unwrap();
        fs::remove_file(missing_file.join("files/1")).unwrap();
        let ev = event("records.item.created");
        let good = stage(home.path(), "records", &plan(vec![ev.clone()])).unwrap();

        let r = store.recover().expect("recovery never fails on one bad folder");
        assert_eq!(r.rolled_forward, 1);
        let mut broken: Vec<String> = r.broken.iter().map(|(id, _)| id.clone()).collect();
        broken.sort();
        let mut want: Vec<String> = [&bad_manifest, &missing_file]
            .iter()
            .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        want.sort();
        assert_eq!(broken, want);
        for id in &want {
            let kept = home.path().join(".staging/broken").join(id);
            assert!(kept.join(COMMITTED).exists() && kept.join(MANIFEST).exists(), "kept whole: {}", kept.display());
        }
        assert!(!good.exists());
        assert_eq!(logged(&store), [ev], "the good one still applied");
        // Kept aside, so the next start doesn't trip on them again.
        assert_eq!(store.recover().unwrap(), Recovery::default());
    }

    /// #260's second pass, finding 10: once `COMMITTED` is written the write has happened. A
    /// failure finishing it is not reported as a failed write (a retry would make a second copy);
    /// it's finished before the next commit, its event logged exactly once.
    #[cfg(unix)]
    #[test]
    fn a_committed_write_that_cant_be_finished_is_a_success_finished_later() {
        use std::os::unix::fs::PermissionsExt;
        let home = TempDir::new().unwrap();
        let store = Store::open(home.path()).unwrap();
        let events = home.path().join("events");
        fs::set_permissions(&events, fs::Permissions::from_mode(0o500)).unwrap();
        if File::create(events.join("probe")).is_ok() {
            return; // root ignores permissions: nothing to test here
        }
        let ev = event("records.item.created");
        store.commit("records", plan(vec![ev.clone()])).expect("committed, so it succeeded");
        assert_eq!(store.read("records", "a.toml").unwrap().unwrap(), b"a", "the files are in place");
        assert_eq!(fs::read_dir(home.path().join(".staging")).unwrap().count(), 1, "its last step waits");

        fs::set_permissions(&events, fs::Permissions::from_mode(0o700)).unwrap();
        let next = event("records.item.updated");
        store.commit("records", plan(vec![next.clone()])).unwrap();
        assert_eq!(logged(&store), [ev, next], "the first event once, before the second");
        assert_eq!(fs::read_dir(home.path().join(".staging")).unwrap().count(), 0);
    }

    /// #260 finding 3: two writers of one file at once each use their own temp file, so neither
    /// fails and the file is always one writer's whole content.
    #[test]
    fn many_writers_of_one_file_at_once_never_fail_or_mix() {
        let home = TempDir::new().unwrap();
        let file = home.path().join("cache/one.bin");
        let writers: Vec<_> = (0..20u8)
            .map(|i| {
                let file = file.clone();
                std::thread::spawn(move || write_atomic(&file, &vec![i; 64 * 1024]))
            })
            .collect();
        for w in writers {
            w.join().unwrap().expect("no writer fails");
        }
        let got = fs::read(&file).unwrap();
        assert_eq!(got.len(), 64 * 1024);
        assert!(got.iter().all(|b| *b == got[0]), "one writer's content, whole");
        let left: Vec<_> = fs::read_dir(file.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, [std::ffi::OsString::from("one.bin")], "no temp files left");
    }
}
