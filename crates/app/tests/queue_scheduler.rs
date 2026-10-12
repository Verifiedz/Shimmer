//! `shimmer queue` and `shimmer scheduler` end to end (ADR 0015), through the real binary and a
//! real daemon: real workspace launches waiting in the queue, a real trigger firing.

mod common;

use std::time::{Duration, Instant};

use common::{ok, stderr, Home};

/// Poll `shimmer args…` until its output contains `want`, for up to 10 s.
fn wait_for(home: &Home, args: &[&str], want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = ok(home, args);
        if out.contains(want) {
            return out;
        }
        assert!(Instant::now() < deadline, "{args:?} never showed {want:?}; last output:\n{out}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A workspace whose one step runs for a while, so its launch holds the `workspaces` lane.
fn slow_workspace(home: &Home, id: &str) {
    let dir = home.dir.path().join("home/data/workspaces").join(id);
    std::fs::create_dir_all(dir.join("steps")).unwrap();
    std::fs::write(
        dir.join("workspace.toml"),
        "[workspace]\nlabel = \"Slow\"\n\n[[step]]\nname = \"setup\"\nmode = \"supervised\"\ntimeout_s = 30\n",
    )
    .unwrap();
    std::fs::write(dir.join("steps/01-setup.sh"), "sleep 20\n").unwrap();
}

/// `queued the launch of a (task 01ABC…)` → `01ABC…`.
fn task_id(activate_output: &str) -> String {
    let start = activate_output.find("(task ").expect("a task id") + "(task ".len();
    activate_output[start..].split(')').next().unwrap().to_string()
}

#[test]
fn bare_words_show_help() {
    let home = Home::with_leetcode();
    assert!(ok(&home, &["queue"]).starts_with("usage: shimmer queue <command>"));
    assert!(ok(&home, &["scheduler", "--help"]).contains("--catch-up says what happens"));
    let help = ok(&home, &["--help"]);
    assert!(help.contains("queue …") && help.contains("scheduler …"), "{help}");
}

#[test]
fn an_idle_queue_says_so_and_names_its_lanes() {
    let home = Home::with_leetcode();
    let out = ok(&home, &["queue", "list"]);
    assert!(out.starts_with("nothing queued or running\nlanes: "), "{out}");
    assert!(out.contains("workspaces (1 at a time)") && out.contains("default (4 at a time)"), "{out}");
}

#[test]
fn real_launches_wait_in_the_queue_and_can_be_moved_and_cancelled() {
    let home = Home::with_leetcode();
    ok(&home, &["ping"]);
    for id in ["a", "b", "c"] {
        slow_workspace(&home, id);
    }
    // One lane slot (§6.1): a runs, b and c wait behind it, in that order.
    let ids: Vec<String> =
        ["a", "b", "c"].iter().map(|w| task_id(&ok(&home, &["workspaces", "activate", w]))).collect();
    let (a, b, c) = (&ids[0], &ids[1], &ids[2]);
    wait_for(&home, &["queue", "show", a], "(running)");

    let list = ok(&home, &["queue", "list"]);
    let order: Vec<&str> = list.lines().filter(|l| l.starts_with("workspaces")).collect();
    assert_eq!(order.len(), 3, "{list}");
    assert!(order[0].contains(a.as_str()) && order[0].contains("running") && order[0].contains(" you "), "{list}");
    assert!(order[1].contains(b.as_str()) && order[2].contains(c.as_str()), "{list}");
    assert!(list.contains("3 tasks;"), "{list}");

    let show = ok(&home, &["queue", "show", b]);
    assert!(show.starts_with(&format!("{b}  (queued)")) && show.contains("  op        workspaces.activate"), "{show}");

    // c jumps ahead of b.
    assert_eq!(
        ok(&home, &["queue", "move", c, "--before", b]).trim(),
        format!("✓ moved {c} ahead of {b} in lane workspaces")
    );
    let list = ok(&home, &["queue", "list", "--lane", "workspaces"]);
    let order: Vec<&str> = list.lines().filter(|l| l.starts_with("workspaces")).collect();
    assert!(order[1].contains(c.as_str()) && order[2].contains(b.as_str()), "{list}");

    // A queued task is simply gone; a running one is asked to stop.
    assert!(ok(&home, &["queue", "cancel", b]).contains("it was still queued, so it never ran"));
    assert!(ok(&home, &["queue", "cancel", a]).contains("it was running"));
    ok(&home, &["queue", "cancel", c]);
    wait_for(&home, &["queue", "list"], "nothing queued or running");

    // A finished task can still be looked at, but not cancelled again.
    assert!(ok(&home, &["queue", "show", b]).starts_with(&format!("{b}  (cancelled)")));
    let o = home.shimmer(&["queue", "cancel", b]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("not_cancellable"), "{}", stderr(&o));
}

#[test]
fn a_trigger_fires_and_can_be_paused_resumed_and_removed() {
    let home = Home::with_leetcode();
    let out = ok(
        &home,
        &["scheduler", "add", "records.list", r#"{"collection":"leetcode"}"#, "--every", "1s", "--catch-up", "skip"],
    );
    assert!(out.starts_with("✓ added trigger usr-") && out.contains(": records.list every 1s, catch-up skip"), "{out}");
    assert!(out.contains("\n  first run: "), "{out}");
    let id = out["✓ added trigger ".len()..].split(':').next().unwrap().to_string();

    let list = ok(&home, &["scheduler", "list"]);
    let row = list.lines().find(|l| l.starts_with(&id)).unwrap_or_else(|| panic!("{list}"));
    assert!(
        row.contains("records.list")
            && row.contains("every 1s")
            && row.contains("skip")
            && row.contains("active")
            && row.ends_with("you"),
        "{row}"
    );

    // The scheduler ticks every second: it fires, and its task runs through the queue.
    let show = wait_for(&home, &["scheduler", "show", &id], "  last run  20"); // a date, not "never"
    assert!(show.contains("  params    {\"collection\":\"leetcode\"}") && show.contains("  catch-up  skip"), "{show}");

    assert!(ok(&home, &["scheduler", "pause", &id]).starts_with(&format!("✓ paused {id}")));
    assert!(ok(&home, &["scheduler", "show", &id]).starts_with(&format!("{id}  (paused)")));
    assert!(ok(&home, &["scheduler", "resume", &id]).starts_with(&format!("✓ resumed {id}")));
    assert!(ok(&home, &["scheduler", "show", &id]).starts_with(&format!("{id}  (active)")));
    assert_eq!(ok(&home, &["scheduler", "remove", &id]).trim(), format!("✓ removed {id}"));
    assert!(ok(&home, &["scheduler", "list"]).starts_with("no triggers yet"));
}

#[test]
fn scheduler_mistakes_are_caught_before_or_by_the_daemon() {
    let home = Home::with_leetcode();
    let o = home.shimmer(&["scheduler", "add", "records.list", "--every", "1d"]);
    assert_eq!(o.status.code(), Some(2), "missing --catch-up is a usage error");
    assert!(stderr(&o).contains("needs --catch-up: skip (drop missed runs)"), "{}", stderr(&o));

    let o = home.shimmer(&["scheduler", "add", "nope.op", "--every", "1d", "--catch-up", "skip"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("unknown_op"), "the daemon checks the op: {}", stderr(&o));

    let o = home.shimmer(&["scheduler", "show", "usr-nope"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("not_found: no trigger 'usr-nope'"), "{}", stderr(&o));
}

#[test]
fn a_pack_can_alias_the_new_commands() {
    let home = Home::with_leetcode();
    let dir = home.dir.path().join("home/packs/mine");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pack.toml"),
        "[pack]\nid = \"mine\"\nlabel = \"Mine\"\n[alias]\n\"jobs\" = \"queue.list\"\n",
    )
    .unwrap();
    assert!(ok(&home, &["--pack", "mine", "jobs"]).starts_with("nothing queued or running"));
}
