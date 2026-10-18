//! What the MVP promises a user and nothing else tests end to end (#162): the real `shimmer`
//! binary and daemon, the way a user runs them. Each test starts from an empty Shimmer folder.
//!
//! These check promises, not internals, so new features shouldn't break them. When a new
//! mechanism stores data (fetchers, notify, calendar…), add it to the portability test, and give
//! the CLAUDE.md §1.3 chain its own test here once its parts exist.

mod common;

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{fails, ok, stderr, stdout, wait_gone, Home};
use serde_json::Value;

fn json(home: &Home, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.push("--json");
    serde_json::from_str(&ok(home, &args)).unwrap()
}

fn state(home: &Home, id: &str) -> String {
    json(home, &["workspaces", "status", id])["state"].as_str().unwrap_or_default().to_owned()
}

/// Poll `cond` for up to 60 s (the scheduler ticks once a second, but a cold first run can be
/// slow). On a timeout, show what the daemon was doing, so a CI failure explains itself.
fn wait_until(home: &Home, what: &str, mut cond: impl FnMut() -> bool) {
    let started = Instant::now();
    while !cond() {
        if started.elapsed() > Duration::from_secs(60) {
            let show = |args: &[&str]| {
                let o = home.shimmer(args);
                format!("$ shimmer {}\n{}{}", args.join(" "), stdout(&o), stderr(&o))
            };
            let logs = home.dir.path().join("home/logs");
            let daemon_log = std::fs::read_dir(&logs)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("daemon.log"))
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .collect::<String>();
            let tail: Vec<&str> = daemon_log.lines().rev().take(40).collect();
            panic!(
                "timed out after 60 s waiting for {what}\n{}\n{}\n{}\n--- end of the daemon log ---\n{}",
                show(&["--json", "scheduler", "list"]),
                show(&["--json", "queue", "list"]),
                show(&["workspaces", "list"]),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The trigger id in `scheduler add`'s reply: "✓ added trigger usr-…: …".
/// A trigger made with `shimmer call scheduler.add` (ADR 0031 §6: the raw form went from
/// `scheduler add`): its id.
fn raw_trigger(home: &Home, add: &str) -> String {
    let out: serde_json::Value = serde_json::from_str(&ok(home, &["call", "scheduler.add", add, "--json"])).unwrap();
    out["trigger_id"].as_str().unwrap().to_owned()
}

/// `secs` from now as UTC RFC 3339, for `scheduler add --once`.
fn utc_in(secs: u64) -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + secs;
    let (days, rest) = ((t / 86_400) as i64, t % 86_400);
    // Days since 1970-01-01 → civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3_600, rest % 3_600 / 60, rest % 60)
}

/// Copy a Shimmer folder the way a person moves it between machines: everything except what its
/// own `.gitignore` leaves out (CLAUDE.md §7: the index, the HTTP cache, logs, staging).
fn copy_shimmer_folder(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("index.sqlite") || ["cache", "logs", ".staging"].contains(&name.as_str()) {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type().unwrap().is_dir() {
            copy_shimmer_folder(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Every file under `dir` whose text mentions `needle`.
fn files_mentioning(dir: &Path, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_mentioning(&path, needle));
        } else if std::fs::read_to_string(&path).is_ok_and(|t| t.contains(needle)) {
            found.push(path.display().to_string());
        }
    }
    found
}

#[test]
fn a_shimmer_folder_copied_elsewhere_keeps_working() {
    // CLAUDE.md §1.6 "Portable": copy $SHIMMER_HOME to another machine and it works.
    let old = Home::new();
    ok(&old, &["records", "new", "jobs", "--from", "job-applications"]);
    ok(&old, &["records", "add", "jobs", "acme", "--position", "SDE Intern", "--company", "Acme"]);
    ok(&old, &["records", "complete", "jobs/acme"]);
    ok(&old, &["workspaces", "new", "smoke", "--from", "smoke-test", "--set", "FAIL_AT=none"]);
    ok(&old, &["workspaces", "activate", "smoke", "--wait"]);
    ok(&old, &["workspaces", "stop", "smoke", "--wait"]);
    let trigger = raw_trigger(
        &old,
        r#"{"schedule":{"every":86400},"op":"records.list","params":{"collection":"jobs"},"catch_up":"skip"}"#,
    );
    ok(&old, &["shutdown"]);
    wait_gone(&old.socket());

    // Move it, and make sure the old place is really gone.
    let new = Home::new();
    let old_home = old.dir.path().join("home");
    copy_shimmer_folder(&old_home, &new.dir.path().join("home"));
    let old_path = old_home.display().to_string();
    std::fs::remove_dir_all(&old_home).unwrap();
    let stale = files_mentioning(&new.dir.path().join("home"), &old_path);
    assert!(stale.is_empty(), "files still naming the old folder: {stale:?}");

    // Records, with their status and stamp; and new writes work.
    let acme = json(&new, &["records", "get", "jobs/acme"]);
    assert_eq!((acme["status"].as_str(), acme["company"].as_str()), (Some("done"), Some("Acme")), "{acme}");
    assert!(acme["applied_on"].is_string(), "the completion stamp came along: {acme}");
    ok(&new, &["records", "add", "jobs", "globex", "--position", "SWE", "--company", "Globex"]);
    assert_eq!(json(&new, &["records", "list", "jobs"])["total"], 2);

    // The workspace launches from its new place and stops again.
    assert_eq!(state(&new, "smoke"), "ready");
    ok(&new, &["workspaces", "activate", "smoke", "--wait"]);
    assert_eq!(state(&new, "smoke"), "active");
    ok(&new, &["workspaces", "stop", "smoke", "--wait"]);

    // The schedule is still there, unchanged.
    let shown = ok(&new, &["scheduler", "show", &trigger]);
    assert!(shown.contains("records.list") && shown.contains("\"collection\":\"jobs\""), "{shown}");
}

#[test]
fn a_schedule_launches_a_workspace_and_a_dirty_one_never_blocks_the_lane() {
    // CLAUDE.md §1.2 and §11.1: a trigger only enqueues; the queue runs the launch; a launch that
    // fails frees its lane at once.
    let home = Home::new();
    ok(&home, &["workspaces", "new", "smoke", "--from", "smoke-test", "--set", "FAIL_AT=none"]);
    ok(&home, &["workspaces", "new", "broken", "--from", "smoke-test", "--set", "FAIL_AT=check"]);

    // A one-shot schedule a few seconds from now launches it (with room for a slow machine).
    let add = |id: &str| {
        ok(&home, &["scheduler", "add", "once", &utc_in(5), "--catch-up", "run-once", "workspaces", "activate", id]);
    };
    add("smoke");
    wait_until(&home, "the scheduled launch of smoke", || state(&home, "smoke") == "active");

    // A scheduled launch that fails leaves that workspace dirty...
    add("broken");
    wait_until(&home, "the scheduled launch of broken to fail", || state(&home, "broken") == "dirty");
    let status = ok(&home, &["workspaces", "status", "broken"]);
    assert!(status.contains("step 1/3 check"), "{status}");

    // ...and the workspaces lane goes straight on: the next launch runs.
    ok(&home, &["workspaces", "stop", "smoke", "--wait"]);
    ok(&home, &["workspaces", "activate", "smoke", "--wait"]);
    assert_eq!(state(&home, "smoke"), "active");
    ok(&home, &["workspaces", "stop", "smoke", "--wait"]);
    ok(&home, &["workspaces", "cleanup", "broken", "--wait"]);

    // Both one-shot triggers ran and are done.
    let triggers = json(&home, &["scheduler", "list"])["triggers"].clone();
    let mine: Vec<&Value> = triggers
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["id"].as_str().is_some_and(|id| id.starts_with("usr-")))
        .collect();
    assert_eq!(mine.len(), 2, "{triggers}");
    assert!(mine.iter().all(|t| t["done"] == true), "{triggers}");
}

#[test]
fn records_export_and_import_round_trip_through_files() {
    let home = Home::new();
    ok(&home, &["records", "new", "jobs", "--from", "job-applications"]);
    ok(&home, &["records", "new", "copy-csv", "--from", "job-applications"]);
    ok(&home, &["records", "new", "copy-json", "--from", "job-applications"]);
    ok(
        &home,
        &[
            "records",
            "add",
            "jobs",
            "acme",
            "--position",
            "SDE, Intern",
            "--company",
            "Acme \"Co\"",
            "--stage",
            "interviewing",
            "--tech-stack",
            "rust, go",
            "--apply-by",
            "2026-11-01",
        ],
    );
    ok(&home, &["records", "add", "jobs", "globex", "--position", "SWE", "--company", "Globex"]);
    ok(&home, &["records", "complete", "jobs/globex"]);
    let original = json(&home, &["records", "list", "jobs"])["items"].clone();
    assert_eq!(original.as_array().unwrap().len(), 2);

    for (format, collection) in [("csv", "copy-csv"), ("json", "copy-json")] {
        let file = home.dir.path().join(format!("jobs.{format}"));
        std::fs::write(&file, ok(&home, &["records", "export", "jobs", "--format", format])).unwrap();
        let path = file.display().to_string();
        // A dry run changes nothing; then the real import adds both.
        ok(&home, &["records", "import", collection, &path, "--dry-run"]);
        assert_eq!(json(&home, &["records", "list", collection])["total"], 0, "{format}: dry run wrote");
        let out = ok(&home, &["records", "import", collection, &path, "--yes"]);
        assert!(out.contains('2'), "{format}: {out}");
        let copied = json(&home, &["records", "list", collection])["items"].clone();
        assert_eq!(copied, original, "{format}: the round trip changed something");
        // Importing the same file again adds nothing: every row is already there.
        ok(&home, &["records", "import", collection, &path, "--yes"]);
        assert_eq!(json(&home, &["records", "list", collection])["total"], 2, "{format}: duplicated");
    }
}

#[test]
fn reopen_and_purge_do_what_they_say_and_purge_cannot_be_undone() {
    let home = Home::new();
    ok(&home, &["records", "new", "lc", "--from", "leetcode"]);
    for title in ["Two Sum", "LRU Cache", "Word Ladder"] {
        ok(&home, &["records", "add", "lc", "--title", title]);
    }

    // Reopen: done → todo, keeping the stamp unless asked to clear it.
    ok(&home, &["records", "complete", "lc/two-sum"]);
    assert!(json(&home, &["records", "get", "lc/two-sum"])["last_solved"].is_string());
    ok(&home, &["records", "reopen", "lc/two-sum"]);
    let kept = json(&home, &["records", "get", "lc/two-sum"]);
    assert_eq!(kept["status"], "todo");
    assert!(kept["last_solved"].is_string(), "plain reopen keeps the stamp: {kept}");
    ok(&home, &["records", "complete", "lc/two-sum"]);
    ok(&home, &["records", "reopen", "lc/two-sum", "--clear-stamp"]);
    let cleared = json(&home, &["records", "get", "lc/two-sum"]);
    assert_eq!((cleared["status"].as_str(), cleared["last_solved"].is_null()), (Some("todo"), true), "{cleared}");

    // Purge one record, then the whole trash; nothing purged comes back.
    ok(&home, &["records", "remove", "lc/lru-cache"]);
    ok(&home, &["records", "remove", "lc/word-ladder"]);
    assert!(fails(&home, &["records", "purge", "lc/lru-cache"]).contains("--yes"), "asks first, without a terminal");
    ok(&home, &["records", "purge", "lc/lru-cache", "--yes"]);
    assert!(fails(&home, &["records", "restore", "lc/lru-cache"]).contains("lru-cache"));
    ok(&home, &["records", "purge", "lc", "--yes"]);
    assert!(ok(&home, &["records", "trash", "lc"]).contains("nothing in lc's trash"));
    assert!(fails(&home, &["records", "restore", "lc/word-ladder"]).contains("word-ladder"));
    assert_eq!(json(&home, &["records", "list", "lc"])["total"], 1, "only two-sum is left");
}

#[test]
fn a_removed_workspace_comes_back_with_restore_and_still_launches() {
    let home = Home::new();
    ok(&home, &["workspaces", "new", "smoke", "--from", "smoke-test", "--set", "FAIL_AT=none"]);
    let out = ok(&home, &["workspaces", "remove", "smoke"]);
    assert!(out.contains("shimmer workspaces restore smoke"), "says how to undo: {out}");
    let list = ok(&home, &["workspaces", "list"]);
    assert!(!list.lines().any(|l| l.starts_with("smoke ")), "{list}");
    fails(&home, &["workspaces", "status", "smoke"]);

    ok(&home, &["workspaces", "restore", "smoke"]);
    assert_eq!(state(&home, "smoke"), "ready");
    ok(&home, &["workspaces", "activate", "smoke", "--wait"]);
    assert_eq!(state(&home, "smoke"), "active");
    ok(&home, &["workspaces", "stop", "smoke", "--wait"]);
    // Restoring what isn't removed says so.
    fails(&home, &["workspaces", "restore", "smoke"]);

    // Purge deletes a removed one for good; without a person there it needs --yes (ADR 0038).
    ok(&home, &["workspaces", "remove", "smoke"]);
    fails(&home, &["workspaces", "purge", "smoke"]);
    assert_eq!(ok(&home, &["workspaces", "purge", "smoke", "--yes"]).trim(), "✓ smoke is deleted for good");
    fails(&home, &["workspaces", "restore", "smoke"]);
    assert_eq!(ok(&home, &["workspaces", "purge"]).trim(), "no removed workspaces to purge");
}
