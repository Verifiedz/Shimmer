//! A corrupt `index.sqlite` must not stop the daemon from starting (CLAUDE.md §1.4/§7:
//! "delete it, corrupt it... and the daemon rebuilds it from the event log"). The recovery
//! mechanism lives in `crates/store/src/index.rs::Index::open`, unit-tested there against
//! synthetic error codes and real garbage/truncated/empty files in isolation — these tests
//! are the end-to-end proof that a real restart of the real binary actually benefits from it.

mod common;

use common::{stderr, wait_gone, Home};
use serde_json::{json, Value};
use shimmer_store::Index;

fn call(home: &Home, op: &str, params: Value) -> Value {
    let o = home.shimmer(&["call", op, &params.to_string(), "--json"]);
    assert!(o.status.success(), "{op} failed: {}", stderr(&o));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn index_path(home: &Home) -> std::path::PathBuf {
    home.dir.path().join("home/index.sqlite")
}

fn index_event_count(home: &Home) -> u64 {
    Index::open(&index_path(home)).unwrap().stats().unwrap().events
}

/// The topics in the event log, the truth the index is rebuilt from.
fn logged_topics(home: &Home) -> Vec<String> {
    let dir = home.dir.path().join("home/events");
    let mut topics = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        for line in std::fs::read_to_string(entry.unwrap().path()).unwrap().lines() {
            topics.push(serde_json::from_str::<Value>(line).unwrap()["topic"].as_str().unwrap().to_owned());
        }
    }
    topics
}

/// One completed record (the collection is written in by `Home::with_leetcode`, not seeded):
/// `records.item.created` + `records.item.completed`, 2 events. Shuts the daemon down so
/// the index file sits still while the test tampers with it before the next restart.
fn seed_and_stop(home: &Home) {
    call(
        home,
        "records.add",
        json!({"collection": "leetcode", "id": "two-sum", "fields": {"title": "Two Sum", "difficulty": "easy"}}),
    );
    call(home, "records.complete", json!({"collection": "leetcode", "id": "two-sum"}));
    assert!(home.shimmer(&["shutdown"]).status.success());
    wait_gone(&home.socket());
}

#[test]
fn a_corrupt_index_does_not_stop_the_daemon_starting() {
    let home = Home::with_leetcode();
    seed_and_stop(&home);
    let before = index_event_count(&home);
    let logged = logged_topics(&home);
    assert_eq!(before, logged.len() as u64, "the index holds every logged event");
    // Nothing is seeded on first start (ADR 0023): the only records events are the test's own.
    // (Others are modules' own start-up, e.g. notify's trigger being registered.)
    let records: Vec<&String> = logged.iter().filter(|t| t.starts_with("records.")).collect();
    assert_eq!(records, ["records.item.created", "records.item.completed"]);

    std::fs::write(index_path(&home), b"not a sqlite file at all, just garbage bytes").unwrap();

    // The bug this proves fixed: this used to make the daemon refuse to start at all
    // ("cannot start daemon: internal: index: file is not a database").
    let pong = home.shimmer(&["ping"]);
    assert!(pong.status.success(), "daemon failed to start over a corrupt index: {}", stderr(&pong));

    assert_eq!(index_event_count(&home), before, "rebuilt from the event log to the same count");
}

/// The gap found in PR review: damage that leaves the header and schema intact opens fine
/// and answers some queries, so it was never caught at start — only later, as a write
/// failure the indexer could log ("will heal on next catch-up") but never actually heal,
/// since nothing ever revisits it. `PRAGMA quick_check` at open closes it. Seeds enough rows
/// to span multiple pages so corrupting the back quarter of the file leaves page 1's schema
/// untouched — the store-level test (`crates/store/src/index.rs`) proves `open_conn` alone
/// really does succeed against this exact corruption; this is the end-to-end proof that a
/// real restart of the real binary benefits from catching it anyway.
#[test]
fn a_mid_file_corrupt_index_that_still_opens_gets_caught_and_rebuilt() {
    let home = Home::with_leetcode();
    for i in 0..200 {
        call(
            &home,
            "records.add",
            json!({"collection": "leetcode", "id": format!("problem-{i}"),
                   "fields": {"title": format!("Problem {i}"), "difficulty": "easy"}}),
        );
    }
    assert!(home.shimmer(&["shutdown"]).status.success());
    wait_gone(&home.socket());
    let before = index_event_count(&home);

    let mut bytes = std::fs::read(index_path(&home)).unwrap();
    let start = bytes.len() * 3 / 4;
    for b in bytes.iter_mut().skip(start) {
        *b ^= 0xFF;
    }
    std::fs::write(index_path(&home), &bytes).unwrap();

    let pong = home.shimmer(&["ping"]);
    assert!(pong.status.success(), "daemon failed to start over a mid-file-corrupt index: {}", stderr(&pong));
    assert_eq!(index_event_count(&home), before, "rebuilt from the event log to the same count");
}

#[test]
fn a_truncated_index_does_not_stop_the_daemon_starting() {
    let home = Home::with_leetcode();
    seed_and_stop(&home);
    let before = index_event_count(&home);

    // A real database, cut off partway through — SQLITE_CORRUPT ("database disk image is
    // malformed"), a distinct code from garbage bytes' SQLITE_NOTADB, and both must recover.
    let bytes = std::fs::read(index_path(&home)).unwrap();
    std::fs::write(index_path(&home), &bytes[..bytes.len() / 3]).unwrap();

    let pong = home.shimmer(&["ping"]);
    assert!(pong.status.success(), "daemon failed to start over a truncated index: {}", stderr(&pong));
    assert_eq!(index_event_count(&home), before);
}

#[test]
fn deleting_the_index_entirely_still_rebuilds_it_on_restart() {
    let home = Home::with_leetcode();
    seed_and_stop(&home);
    let before = index_event_count(&home);

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", index_path(&home).display()));
    }
    assert!(!index_path(&home).exists());

    let pong = home.shimmer(&["ping"]);
    assert!(pong.status.success(), "{}", stderr(&pong));
    assert_eq!(index_event_count(&home), before, "rebuild from the event log matches what was there before");
}

#[test]
fn an_empty_index_file_was_never_a_failure_case() {
    let home = Home::with_leetcode();
    seed_and_stop(&home);
    let before = index_event_count(&home);

    // SQLite treats a zero-length file as a fresh, valid database — there is no error here
    // to recover from. This proves the daemon still starts and still repopulates it, not
    // that the new recovery code path fires (it doesn't need to).
    std::fs::write(index_path(&home), b"").unwrap();

    let pong = home.shimmer(&["ping"]);
    assert!(pong.status.success(), "{}", stderr(&pong));
    assert_eq!(index_event_count(&home), before);
}
