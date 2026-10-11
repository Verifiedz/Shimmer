//! M1's definition of done (CLAUDE.md §14): a completed item survives a daemon restart, and
//! the CLI can add, list, complete and filter. Runs the real binary.

mod common;

use common::{stderr, stdout, wait_gone, Home};
use serde_json::{json, Value};

fn call(home: &Home, op: &str, params: Value) -> Value {
    let o = home.shimmer(&["call", op, &params.to_string(), "--json"]);
    assert!(o.status.success(), "{op} failed: {}", stderr(&o));
    serde_json::from_slice(&o.stdout).unwrap()
}

#[test]
fn a_completed_item_survives_a_daemon_restart() {
    let home = Home::with_leetcode();

    // The daemon registers records, which seeds the LeetCode collection.
    let m = call(&home, "core.manifest", json!({}));
    assert!(m["modules"].as_array().unwrap().iter().any(|m| m["id"] == "records"), "{m}");

    call(
        &home,
        "records.add",
        json!({"collection": "leetcode", "id": "two-sum", "fields": {"title": "Two Sum", "difficulty": "easy"}}),
    );
    let done = call(&home, "records.complete", json!({"collection": "leetcode", "id": "two-sum"}));
    assert_eq!(done["status"], "done");

    // Restart: stop the daemon, and let the next command start a fresh one.
    assert!(home.shimmer(&["shutdown"]).status.success());
    wait_gone(&home.socket());

    let after = call(&home, "records.list", json!({"collection": "leetcode", "filter": {"status": "done"}}));
    assert_eq!(after["total"], 1);
    assert_eq!(after["items"][0], done);

    // Files are the truth: the record is a readable file in $SHIMMER_HOME.
    let file = home.dir.path().join("home/data/records/items/leetcode/two-sum.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("status = \"done\""), "{text}");
}

#[test]
fn the_records_commands_track_a_problem_across_a_restart() {
    let home = Home::with_leetcode();
    let ok = |args: &[&str]| {
        let o = home.shimmer(args);
        assert!(o.status.success(), "shimmer {args:?} failed: {}", stderr(&o));
        stdout(&o)
    };

    assert!(ok(&["records", "collections"]).contains("leetcode  LeetCode  title*, difficulty (easy|medium|hard)"));

    assert_eq!(
        ok(&["records", "add", "leetcode", "two-sum", "--title", "Two Sum", "--difficulty", "easy"]).trim(),
        "added leetcode/two-sum"
    );
    ok(&["records", "add", "leetcode/lru-cache", "--title=LRU Cache", "--difficulty=medium"]);

    let table = ok(&["records", "list", "leetcode"]);
    assert!(table.starts_with("ID         STATUS  TITLE      DIFFICULTY"), "{table}");
    assert!(table.trim_end().ends_with("2 records"), "{table}");

    let done = ok(&["records", "complete", "leetcode/two-sum"]);
    assert!(done.starts_with("✓ leetcode/two-sum done (last_solved "), "{done}");

    // A bad value is refused with the daemon's message and changes nothing.
    let o = home.shimmer(&["records", "add", "leetcode/x", "--title", "X", "--difficulty", "trivial"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("invalid_params: field 'difficulty' must be one of easy, medium, hard"));

    ok(&["shutdown"]);
    wait_gone(&home.socket());

    // After the restart, filtering finds exactly the completed one.
    let o = home.shimmer(&["records", "list", "leetcode", "--status", "done", "--json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let data: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(data["total"], 1);
    assert_eq!(data["items"][0]["id"], "two-sum");
    assert_eq!(data["items"][0]["status"], "done");

    let todo = ok(&["records", "list", "leetcode", "--status", "todo"]);
    assert!(todo.contains("lru-cache") && !todo.contains("two-sum"), "{todo}");
}

/// Issue #29: one malformed collection file used to break every `shimmer records` command,
/// because the CLI fetches `records.collections` first and that op failed as a whole.
#[test]
fn a_broken_collection_file_breaks_only_itself() {
    let home = Home::with_leetcode();
    let ok = |args: &[&str]| {
        let o = home.shimmer(args);
        assert!(o.status.success(), "shimmer {args:?} failed: {}", stderr(&o));
        stdout(&o)
    };
    ok(&["records", "add", "leetcode", "two-sum", "--title", "Two Sum", "--difficulty", "easy"]);
    let broken = home.dir.path().join("home/data/records/collections/broken.toml");
    std::fs::write(&broken, "not [valid toml\n").unwrap();
    ok(&["shutdown"]);
    wait_gone(&home.socket());

    // The daemon restarts (init must survive the bad file) and leetcode still works.
    assert!(ok(&["records", "list", "leetcode"]).contains("two-sum"));
    ok(&["records", "complete", "leetcode/two-sum"]);
    assert!(ok(&["records", "get", "leetcode/two-sum"]).contains("(done)"));

    let collections = ok(&["records", "collections"]);
    assert!(collections.contains("leetcode  LeetCode"), "{collections}");
    assert!(collections.contains("skipped: collection file 'broken.toml': "), "{collections}");

    // Using the broken collection still fails, naming the file.
    let o = home.shimmer(&["records", "list", "broken"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("invalid_params: collection file 'broken.toml'"), "{}", stderr(&o));
}

#[test]
fn an_unreadable_record_file_never_blocks_the_others() {
    // #99, through the real binary: a record file that isn't valid UTF-8 (a bad hand-edit, a
    // half-synced file) is skipped and named, and every command keeps working.
    let home = Home::with_leetcode();
    let ok = |args: &[&str]| {
        let o = home.shimmer(args);
        assert!(o.status.success(), "shimmer {args:?} failed: {}", stderr(&o));
        stdout(&o)
    };
    ok(&["records", "add", "leetcode", "two-sum", "--title", "Two Sum", "--difficulty", "easy"]);
    ok(&["records", "add", "leetcode", "gone", "--title", "Gone", "--difficulty", "easy"]);
    ok(&["records", "remove", "leetcode/gone"]);
    let items = home.dir.path().join("home/data/records/items/leetcode");
    std::fs::write(items.join("garbled.toml"), [0xff, 0xfe, b'x']).unwrap();
    let trash = home.dir.path().join("home/data/records/trash/leetcode");
    std::fs::write(trash.join("garbled-too.toml"), [0xff, 0xfe, b'y']).unwrap();

    // Writes, which check unique values against every record, still work.
    assert_eq!(
        ok(&["records", "add", "leetcode", "lru", "--title", "LRU", "--difficulty", "medium"]).trim(),
        "added leetcode/lru"
    );
    ok(&["records", "update", "leetcode/lru", "--difficulty", "hard"]);
    ok(&["records", "rename", "leetcode/lru", "lru-cache"]);
    ok(&["records", "restore", "leetcode/gone"]);
    ok(&["records", "remove", "leetcode/gone"]);

    // The list and the trash show what they can, and name what they can't.
    let list = ok(&["records", "list", "leetcode"]);
    assert!(list.contains("two-sum") && list.contains("lru-cache"), "{list}");
    assert!(list.contains("skipped: record file 'garbled.toml'"), "{list}");
    let shown = ok(&["records", "trash", "leetcode"]);
    assert!(shown.contains("gone") && shown.contains("skipped: record file 'garbled-too.toml'"), "{shown}");

    // Renaming a field renames everywhere it can and says which files keep the old name.
    let renamed = ok(&["records", "rename-field", "leetcode", "difficulty", "level"]);
    assert!(renamed.starts_with("✓ leetcode: field 'difficulty' is now 'level' (3 record(s) updated)"), "{renamed}");
    assert!(renamed.contains("skipped, still has 'difficulty': record file 'garbled.toml'"), "{renamed}");
    assert!(renamed.contains("skipped, still has 'difficulty': record file 'garbled-too.toml'"), "{renamed}");
    assert!(renamed.contains("fix those files, then: shimmer records check leetcode"), "{renamed}");

    // And check names it as a problem, as before.
    let check = ok(&["records", "check", "leetcode"]);
    assert!(check.contains("garbled") && check.contains("can't be read"), "{check}");
}
