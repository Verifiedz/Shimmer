//! Record templates end to end (ADRs 0022, 0030): the real `shimmer` binary and daemon in a
//! throwaway `$SHIMMER_HOME`. Browsing and peeking change nothing; every template's own "How it
//! works" examples run as written.

mod common;

use common::{fails, ok, stderr, Home};
use serde_json::Value;

fn json(home: &Home, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.push("--json");
    serde_json::from_str(&ok(home, &args)).unwrap()
}

/// Every built-in template's id, as the daemon lists them.
fn template_ids(home: &Home) -> Vec<String> {
    json(home, &["records", "templates"])["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Nothing written by a read: no collections, and no event but the daemon's own start-up.
fn nothing_written(home: &Home) {
    let collections = home.dir.path().join("home/data/records/collections");
    let made: Vec<_> = std::fs::read_dir(&collections).map(|d| d.flatten().collect()).unwrap_or_default();
    assert!(made.is_empty(), "browsing created {made:?}");
    let events = home.dir.path().join("home/events");
    for day in std::fs::read_dir(events).into_iter().flatten().flatten() {
        let text = std::fs::read_to_string(day.path()).unwrap();
        assert!(!text.contains("\"records."), "browsing emitted a records event: {text}");
    }
}

#[test]
fn templates_are_listed_by_category_and_each_can_be_peeked() {
    let home = Home::new();
    let listing = ok(&home, &["records", "templates"]);
    let headings: Vec<&str> = listing.lines().filter(|l| !l.starts_with(' ') && l.ends_with(')')).collect();
    assert_eq!(
        headings,
        ["Job hunt (job-hunt)", "Practice (practice)", "Career history (career)", "Life admin (life)"]
    );
    assert!(listing.contains("shimmer records peek TEMPLATE"), "{listing}");

    let ids = template_ids(&home);
    assert_eq!(ids.len(), 16, "{ids:?}");
    for id in &ids {
        assert!(listing.contains(&format!("\n  {id} ")), "{id} missing from:\n{listing}");
        let peek = ok(&home, &["records", "peek", id]);
        assert!(peek.starts_with(&format!("{id}: ")), "{peek}");
        for part in
            ["\nFields (", "\nWhen you complete a record\n", "\nHow it works (", "shimmer records new ID --from"]
        {
            assert!(peek.contains(part), "{id}: no '{part}' in:\n{peek}");
        }
        // `templates TEMPLATE` is the same as `peek TEMPLATE`.
        assert_eq!(ok(&home, &["records", "templates", id]), peek, "{id}");
    }

    // One category on its own.
    let practice = ok(&home, &["records", "templates", "practice"]);
    assert!(
        practice.starts_with("Practice (practice)\n  leetcode") && !practice.contains("job-applications"),
        "{practice}"
    );

    // What completing does, in words, from each kind of collection.
    assert!(ok(&home, &["records", "peek", "leetcode"]).contains("stamps last_solved with today's date"));
    assert!(ok(&home, &["records", "peek", "job-applications"]).contains("It happens once"));
    assert!(ok(&home, &["records", "peek", "addresses"]).contains("it's a reference list"));
    // Notes from the header, privacy first among them.
    assert!(ok(&home, &["records", "peek", "documents"]).contains("• Never store document numbers here"));

    // Mistakes say what there is, and fail.
    assert!(fails(&home, &["records", "peek", "chess"]).contains("no template 'chess' (there are: "));
    assert!(fails(&home, &["records", "peek", "practice"]).contains("'practice' is a category, not a template"));
    assert!(fails(&home, &["records", "templates", "games"]).contains("no template or category 'games'"));
    assert!(fails(&home, &["records", "peek"]).contains("usage: shimmer records peek TEMPLATE"));

    nothing_written(&home);
}

#[test]
fn peek_file_is_exactly_what_new_creates() {
    let home = Home::new();
    for id in template_ids(&home) {
        let file = ok(&home, &["records", "peek", &id, "--file"]);
        assert!(file.contains(&format!("id = \"{id}\"")), "{id}");
        let one = json(&home, &["records", "peek", &id, "--file"]);
        assert_eq!(one["file"].as_str().unwrap().trim_end(), file.trim_end(), "{id}: --json carries the same file");

        // `new` copies it with only the id changed.
        let mine = format!("my-{id}");
        ok(&home, &["records", "new", &mine, "--from", &id]);
        let written =
            std::fs::read_to_string(home.dir.path().join(format!("home/data/records/collections/{mine}.toml")))
                .unwrap();
        let expected = file.replacen(&format!("id = \"{id}\""), &format!("id = \"{mine}\""), 1);
        assert_eq!(written.trim_end(), expected.trim_end(), "{id}");
    }
    let summary = json(&home, &["records", "peek", "leetcode"]);
    assert_eq!((summary["id"].as_str(), summary["category"].as_str()), (Some("leetcode"), Some("practice")));
    assert!(summary.get("file").is_none(), "the file only with --file");
}

/// The shell words of one example command: quotes grouped, `\` continuations joined, and the
/// description after it (three or more spaces outside quotes) left off.
fn example_commands(examples: &[Value]) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut lines = examples.iter().filter_map(Value::as_str).peekable();
    while let Some(line) = lines.next() {
        if !line.starts_with("shimmer records ") {
            continue; // a description on its own line
        }
        let mut text = line.to_owned();
        while text.trim_end().ends_with('\\') {
            text = format!("{} {}", text.trim_end().trim_end_matches('\\'), lines.next().unwrap_or_default().trim());
        }
        let (mut words, mut word, mut quoted, mut spaces) = (Vec::new(), String::new(), false, 0);
        for c in text.chars() {
            match c {
                '"' => quoted = !quoted,
                ' ' if !quoted => {
                    spaces += 1;
                    if spaces == 3 {
                        break;
                    }
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    continue;
                }
                _ => word.push(c),
            }
            spaces = 0;
        }
        if !word.is_empty() {
            words.push(word);
        }
        commands.push(words.into_iter().skip(1).collect());
    }
    commands
}

#[test]
fn every_templates_examples_work_as_written() {
    let home = Home::new();
    let all = json(&home, &["records", "templates"]);
    // Each under its own id: that's the name the templates' refs use for each other.
    for t in all["templates"].as_array().unwrap() {
        let id = t["id"].as_str().unwrap();
        ok(&home, &["records", "new", id, "--from", id]);
    }
    for t in all["templates"].as_array().unwrap() {
        let id = t["id"].as_str().unwrap();
        let commands = example_commands(t["examples"].as_array().unwrap());
        assert!(commands.iter().any(|c| c.first().is_some_and(|w| w == "records")), "{id}: {commands:?}");
        let mut added = String::new();
        for words in commands {
            let words: Vec<String> = words
                .iter()
                .map(|w| w.replace("<collection>/<id>", &format!("{id}/{added}")).replace("<collection>", id))
                .collect();
            let args: Vec<&str> = words.iter().map(String::as_str).collect();
            let out = ok(&home, &args);
            if let Some(made) = out.trim().strip_prefix(&format!("added {id}/")) {
                added = made.to_owned();
            }
        }
        let check = ok(&home, &["records", "check", id]);
        assert!(check.contains("all fit"), "{id}: {check}");
    }
}

// ---------------------------------------------------------------- ADR 0030 §2: schedules

/// The schedules you added (`usr-…`), not any a module registered for itself.
fn schedules(home: &Home) -> Vec<Value> {
    json(home, &["scheduler", "list"])["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["id"].as_str().is_some_and(|id| id.starts_with("usr-")))
        .cloned()
        .collect()
}

fn schedule(home: &Home, op: &str, params: &str, every: &str) -> String {
    let out = ok(home, &["scheduler", "add", op, params, "--every", every, "--catch-up", "skip"]);
    out["✓ added trigger ".len()..].split(':').next().unwrap().to_owned()
}

#[test]
fn renaming_moves_schedules_only_with_a_yes_and_removing_says_what_will_fail() {
    let home = Home::new();
    ok(&home, &["records", "new", "jobs", "--from", "job-applications"]);
    ok(&home, &["records", "new", "leetcode", "--from", "leetcode"]);
    ok(&home, &["records", "add", "jobs", "amazon-sde", "--position", "SDE Intern", "--company", "Amazon"]);
    schedule(&home, "records.list", r#"{"collection":"jobs"}"#, "1d");
    let get = schedule(&home, "records.get", r#"{"collection":"jobs","id":"amazon-sde"}"#, "2d");
    ok(&home, &["scheduler", "pause", &get]);
    let other = schedule(&home, "records.list", r#"{"collection":"leetcode"}"#, "1d");
    let collection_of = |id: &str| -> Vec<(String, String)> {
        let mut found: Vec<(String, String)> = schedules(&home)
            .iter()
            .filter(|t| t["id"] != other)
            .map(|t| {
                (
                    t["params"]["collection"].as_str().unwrap().to_owned(),
                    t["params"]["id"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        found.sort();
        assert!(found.iter().all(|(c, _)| c == id), "{found:?}");
        found
    };

    // A record. No terminal and no --move-schedules: renamed, its schedule named but left alone.
    let out = ok(&home, &["records", "rename", "jobs/amazon-sde", "amazon"]);
    assert!(
        out.starts_with(
            "✓ jobs/amazon-sde renamed to jobs/amazon\n  1 schedule still uses the old name amazon-sde:\n    usr-"
        ),
        "{out}"
    );
    assert!(out.contains("records.get, every 172800s (paused)"), "{out}");
    assert!(
        out.contains("they'll fail until moved: run 'shimmer records rename jobs/amazon amazon-sde' to undo"),
        "{out}"
    );
    assert!(!out.contains("records.list"), "the collection's list doesn't name the record: {out}");
    assert_eq!(collection_of("jobs"), [("jobs".into(), "".into()), ("jobs".into(), "amazon-sde".into())]);
    // The undo it gives works; then with --move-schedules it moves, still paused.
    ok(&home, &["records", "rename", "jobs/amazon", "amazon-sde"]);
    let out = ok(&home, &["records", "rename", "jobs/amazon-sde", "amazon", "--move-schedules"]);
    assert_eq!(out.matches("\n  moved usr-").count(), 1, "{out}");
    assert_eq!(collection_of("jobs"), [("jobs".into(), "".into()), ("jobs".into(), "amazon".into())]);
    let moved = schedules(&home).into_iter().find(|t| t["op"] == "records.get").unwrap();
    assert_eq!((moved["paused"].as_bool(), moved["schedule"]["every"].as_u64()), (Some(true), Some(172_800)));
    assert_ne!(moved["id"], get.as_str(), "added again, the old one removed");

    // A collection: both its schedules, never the other collection's.
    let out = ok(&home, &["records", "rename-collection", "jobs", "apps"]);
    assert!(out.starts_with("✓ jobs is now apps\n  2 schedules still use the old name jobs:"), "{out}");
    assert!(out.contains("run 'shimmer records rename-collection apps jobs' to undo"), "{out}");
    collection_of("jobs");
    ok(&home, &["records", "rename-collection", "apps", "jobs"]);
    let out = ok(&home, &["records", "rename-collection", "jobs", "apps", "--move-schedules"]);
    assert_eq!(out.matches("\n  moved usr-").count(), 2, "{out}");
    assert_eq!(collection_of("apps"), [("apps".into(), "".into()), ("apps".into(), "amazon".into())]);
    assert_eq!(
        schedules(&home).iter().filter(|t| t["id"] == other && t["params"]["collection"] == "leetcode").count(),
        1
    );
    // The moved schedules work: their ops find what they name.
    ok(&home, &["call", "records.get", r#"{"collection":"apps","id":"amazon"}"#]);

    // --json stays JSON: the rename's own reply, nothing appended.
    let o = home.shimmer(&["records", "rename-collection", "leetcode", "lc", "--json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let reply: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(reply["id"], "lc");

    // Removing lists what will fail and keeps the schedules: restoring makes them work again.
    let out = ok(&home, &["records", "remove-collection", "apps", "--yes"]);
    assert!(out.contains("\n  2 schedules still use apps:"), "{out}");
    assert!(out.contains("they'll fail until you restore it (shimmer records restore-collection apps)"), "{out}");
    assert_eq!(schedules(&home).len(), 3, "nothing removed");
    ok(&home, &["records", "restore-collection", "apps"]);
    ok(&home, &["call", "records.get", r#"{"collection":"apps","id":"amazon"}"#]);
    // A removal nothing names says nothing about schedules.
    ok(&home, &["records", "new", "spare", "--from", "documents"]);
    assert!(!ok(&home, &["records", "remove-collection", "spare", "--yes"]).contains("schedule"));
}

// ---------------------------------------------------------------- ADR 0030 §3: copy

/// Every event the daemon has logged, in order.
fn logged(home: &Home) -> Vec<Value> {
    let mut days: Vec<_> =
        std::fs::read_dir(home.dir.path().join("home/events")).unwrap().flatten().map(|d| d.path()).collect();
    days.sort();
    days.iter()
        .flat_map(|d| {
            std::fs::read_to_string(d)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect::<Vec<Value>>()
        })
        .collect()
}

#[test]
fn a_copied_collection_is_independent_of_its_original_and_survives_a_restart() {
    let home = Home::new();
    ok(&home, &["records", "new", "jobs", "--from", "job-applications"]);
    for (id, company) in [("amazon", "Amazon"), ("shopify", "Shopify")] {
        ok(&home, &["records", "add", "jobs", id, "--position", "SDE Intern", "--company", company]);
    }
    ok(&home, &["records", "complete", "jobs/amazon"]);
    ok(&home, &["records", "add", "jobs", "old", "--position", "x", "--company", "y"]);
    ok(&home, &["records", "remove", "jobs/old"]);

    // Empty: the same fields, ready to add to.
    let out = ok(&home, &["records", "copy-collection", "jobs", "jobs-2027"]);
    assert_eq!(
        out.trim(),
        "✓ copied jobs to jobs-2027: same fields, no records (--with-records copies them too)\n  \
         add one: shimmer records add jobs-2027 --position … --company …"
    );
    assert!(ok(&home, &["records", "list", "jobs-2027"]).contains("no records"), "starts empty");

    // With records: the same records, statuses kept, the trash left behind.
    let out = ok(&home, &["records", "copy-collection", "jobs", "backup", "--with-records", "--label", "Jobs backup"]);
    assert_eq!(out.trim(), "✓ copied jobs to backup with 2 record(s)");
    let list = |c: &str| json(&home, &["records", "list", c])["items"].clone();
    assert_eq!(list("backup"), list("jobs"));
    assert!(ok(&home, &["records", "trash", "backup"]).contains("nothing in backup's trash"));
    let collections = json(&home, &["records", "collections"]);
    let label =
        collections["collections"].as_array().unwrap().iter().find(|c| c["id"] == "backup").unwrap()["label"].clone();
    assert_eq!(label, "Jobs backup");

    // Independent: changing the copy leaves the original alone, and the other way round.
    ok(&home, &["records", "update", "backup/shopify", "--stage", "interviewing"]);
    ok(&home, &["records", "remove", "jobs/amazon"]);
    assert_eq!(json(&home, &["records", "get", "jobs/shopify"])["stage"], Value::Null);
    assert_eq!(json(&home, &["records", "get", "backup/amazon"])["status"], "done");

    // Logged: the copy, then one created per record, in the copy.
    let events = logged(&home);
    let at = events
        .iter()
        .position(|e| e["topic"] == "records.collection.copied" && e["payload"]["collection"] == "backup")
        .unwrap();
    assert_eq!(events[at]["payload"], serde_json::json!({"collection": "backup", "from": "jobs", "records": 2}));
    let created: Vec<&str> = events[at + 1..at + 3].iter().map(|e| e["payload"]["id"].as_str().unwrap()).collect();
    assert!(events[at + 1..at + 3]
        .iter()
        .all(|e| e["topic"] == "records.item.created" && e["payload"]["collection"] == "backup"));
    assert_eq!(created.len(), 2);

    // Mistakes say so and change nothing.
    assert!(fails(&home, &["records", "copy-collection", "jobs", "backup"]).contains("already a collection 'backup'"));
    assert!(fails(&home, &["records", "copy-collection", "nope", "x"]).contains("no collection 'nope'"));
    assert!(fails(&home, &["records", "copy-collection", "jobs", "Bad Id"]).contains("not a valid collection id"));
    assert_eq!(logged(&home).len(), events.len());

    // Files are the truth: after a restart it's all still there.
    ok(&home, &["shutdown"]);
    common::wait_gone(&home.socket());
    assert_eq!(json(&home, &["records", "get", "backup/shopify"])["stage"], "interviewing");
    assert_eq!(list("jobs-2027"), serde_json::json!([]));
}

// ---------------------------------------------------------------- ADR 0030 §4: edit and origin

/// `shimmer ARGS` at a real terminal (a pty from `script`), with `$EDITOR` set to `editor`: the
/// only way to reach `records edit`'s editor, which never runs without a person there. Its output,
/// stdout and stderr together, as the terminal showed them.
fn at_a_terminal(home: &Home, editor: &str, args: &[&str]) -> String {
    let command = std::iter::once(env!("CARGO_BIN_EXE_shimmer"))
        .chain(args.iter().copied())
        .map(|w| format!("'{w}'"))
        .collect::<Vec<_>>()
        .join(" ");
    // util-linux (Linux) and BSD (macOS) `script` take different arguments.
    let util_linux = std::process::Command::new("script").arg("--version").output().is_ok_and(|o| o.status.success());
    let mut script = std::process::Command::new("script");
    match util_linux {
        true => script.args(["-qec", &command, "/dev/null"]),
        false => script.args(["-q", "/dev/null", "sh", "-c", &command]),
    };
    let o = script
        .env("SHIMMER_HOME", home.dir.path().join("home"))
        .env("SHIMMER_SOCKET", home.socket())
        .env("XDG_CONFIG_HOME", home.dir.path().join("config"))
        .env("EDITOR", editor)
        .env_remove("VISUAL")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).replace('\r', "")
}

#[test]
fn edit_opens_the_file_then_checks_what_was_saved() {
    let home = Home::new();
    ok(&home, &["records", "new", "jobs", "--from", "job-applications"]);
    ok(&home, &["records", "add", "jobs", "amazon", "--position", "SDE Intern", "--company", "Amazon"]);
    let file = home.dir.path().join("home/data/records/collections/jobs.toml");

    // --path: where it is, for any editor; no terminal needed.
    assert_eq!(ok(&home, &["records", "edit", "jobs", "--path"]).trim(), file.display().to_string());
    // Without a terminal it never waits on an editor: it says where the file is.
    let err = fails(&home, &["records", "edit", "jobs"]);
    assert!(
        err.contains(&format!("records edit needs a terminal; edit the file directly: {}", file.display())),
        "{err}"
    );
    // Only a collection that exists, and never a path outside the folder.
    for bad in ["nope", "../config", "Jobs"] {
        assert!(fails(&home, &["records", "edit", bad, "--path"]).contains(&format!("no collection '{bad}'")), "{bad}");
    }

    // An editor that adds a required field: saved, and the record that doesn't have it is named.
    let editors = home.dir.path().join("editors");
    std::fs::create_dir_all(&editors).unwrap();
    let write = |name: &str, body: &str| {
        let path = editors.join(name);
        std::fs::write(&path, body).unwrap();
        format!("sh {}", path.display())
    };
    let add_field = write(
        "add-field.sh",
        "printf '\\n[[field]]\\nname = \"region\"\\ntype = \"string\"\\nrequired = true\\n' >> \"$1\"\n",
    );
    let out = at_a_terminal(&home, &add_field, &["records", "edit", "jobs"]);
    assert!(out.contains("✓ jobs saved; checked 1 record(s) in jobs; 1 with problems"), "{out}");
    assert!(out.contains("amazon  required field 'region' is missing"), "{out}");
    assert!(std::fs::read_to_string(&file).unwrap().contains("name = \"region\""));

    // An editor that breaks the file: what's wrong, and how to get back in.
    let before = std::fs::read_to_string(&file).unwrap();
    let breaks = write("break.sh", "printf 'not toml [' >> \"$1\"\n");
    let out = at_a_terminal(&home, &breaks, &["records", "edit", "jobs"]);
    assert!(
        out.contains("✗ jobs doesn't load now: ") && out.contains("fix it with: shimmer records edit jobs"),
        "{out}"
    );
    std::fs::write(&file, &before).unwrap();

    // An editor that fails: nothing checked.
    let fails_to_open = write("fail.sh", "exit 3\n");
    let out = at_a_terminal(&home, &fails_to_open, &["records", "edit", "jobs"]);
    assert!(out.contains("exited with") && out.contains("nothing checked"), "{out}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "untouched");
}

#[test]
fn collections_show_the_template_they_came_from() {
    let home = Home::with_leetcode();
    // Written by hand: no column at all.
    assert!(!ok(&home, &["records", "collections"]).contains("TEMPLATE"));

    ok(&home, &["records", "new", "jobs", "--from", "job-applications"]);
    ok(&home, &["records", "copy-collection", "jobs", "jobs-2027"]);
    let table = ok(&home, &["records", "collections"]);
    let row = |id: &str| {
        table.lines().find(|l| l.starts_with(&format!("{id} "))).unwrap_or_else(|| panic!("{table}")).to_owned()
    };
    assert!(table.lines().next().unwrap().contains("TEMPLATE"), "{table}");
    assert!(row("jobs").contains(" job-applications "), "{table}");
    assert!(row("jobs-2027").contains(" job-applications "), "a copy says what its original came from: {table}");
    assert!(row("leetcode").contains(" - "), "{table}");
    let all = json(&home, &["records", "collections"]);
    let jobs = all["collections"].as_array().unwrap().iter().find(|c| c["id"] == "jobs").unwrap();
    assert_eq!(jobs["template"], "job-applications");
    // Peek of that template is the way back to its notes and examples.
    assert!(ok(&home, &["records", "peek", "job-applications"]).contains("How it works"));
}

// ---------------------------------------------------------------- ADR 0030: pack aliases

#[test]
fn every_built_in_pack_names_the_new_records_commands() {
    let home = Home::new();
    ok(&home, &["records", "new", "jobs", "--from", "job-applications"]);
    let peek = ok(&home, &["records", "peek", "leetcode"]);
    let path = ok(&home, &["records", "edit", "jobs", "--path"]);
    for (pack, [peek_alias, copy_alias, edit_alias]) in [
        ("short", ["rpeek", "rcpcol", "redit"]),
        ("anime-tropes", ["hora", "futago", "kaizou"]),
        ("ship-it", ["preview", "copy-pasta", "tweak-schema"]),
        ("starship", ["probe", "twin-fleet", "drydock"]),
    ] {
        assert_eq!(ok(&home, &["--pack", pack, peek_alias, "leetcode"]), peek, "{pack}");
        assert_eq!(ok(&home, &["--pack", pack, edit_alias, "jobs", "--path"]), path, "{pack}");
        let copy = format!("jobs-{pack}");
        let out = ok(&home, &["--pack", pack, copy_alias, "jobs", &copy]);
        assert!(out.starts_with(&format!("✓ copied jobs to {copy}")), "{pack}: {out}");
        let shown = ok(&home, &["packs", "show", pack]);
        for op in ["shimmer records peek", "shimmer records copy-collection", "shimmer records edit"] {
            assert!(shown.contains(op), "{pack} doesn't show {op}:\n{shown}");
        }
    }
}
