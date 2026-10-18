//! `shimmer automations …` on the real binary and daemon (ADR 0035 §8): what a person types.

mod common;

use common::{fails, ok, Home};

const RULE: &str = r#"
[rule]
label = "Tell me when it's solved"
on = "records.item.completed"
when = { collection = "leetcode" }

[[do]]
op = "records.update"
params = { collection = "leetcode", id = "{id}", fields = { url = "https://x.test/{id}" } }
after = "60d"
"#;

#[test]
fn rules_are_made_shown_tested_switched_and_brought_back() {
    let home = Home::with_leetcode();
    let rule = home.dir.path().join("rule.toml");
    std::fs::write(&rule, RULE).unwrap();
    let file = rule.to_str().unwrap();

    let out = ok(&home, &["automations", "new", "solved", "--file", file]);
    assert!(out.contains("✓ made rule solved (Tell me when it's solved): on records.item.completed"), "{out}");
    let out = ok(&home, &["automations"]);
    assert!(out.contains("solved") && out.contains("records.update (after 8w 4d)") && out.contains("never"), "{out}");
    assert!(ok(&home, &["automations", "show", "solved"]).contains("[[do]]"));

    // A broken edit is refused, and the old rule kept.
    std::fs::write(&rule, RULE.replace("records.update", "records.nope")).unwrap();
    let err = fails(&home, &["automations", "edit", "solved", "--file", file]);
    assert!(err.contains("no op 'records.nope'"), "{err}");
    assert!(ok(&home, &["automations", "show", "solved"]).contains("op = \"records.update\""));

    let event = home.dir.path().join("event.json");
    std::fs::write(
        &event,
        r#"{"topic": "records.item.completed", "payload": {"collection": "leetcode", "id": "two-sum"}}"#,
    )
    .unwrap();
    let out = ok(&home, &["automations", "test", "solved", "--event", event.to_str().unwrap()]);
    assert!(out.contains("solved reacts, and would:\n  schedule records.update"), "{out}");
    assert!(out.contains(r#""url":"https://x.test/two-sum""#), "{out}");

    assert!(ok(&home, &["automations", "disable", "solved"]).contains("✓ solved is off"));
    assert!(ok(&home, &["automations", "list"]).contains(" off "));
    ok(&home, &["automations", "enable", "solved"]);

    assert!(ok(&home, &["automations", "remove", "solved"]).contains("shimmer automations restore solved"));
    assert!(ok(&home, &["automations"]).contains("removed: solved"));
    assert!(ok(&home, &["automations", "restore", "solved"]).contains("✓ restored rule solved"));
    // Purge deletes a removed rule for good, and asks first (ADR 0038).
    ok(&home, &["automations", "remove", "solved"]);
    fails(&home, &["automations", "purge", "solved"]);
    assert_eq!(ok(&home, &["automations", "purge", "--yes"]).trim(), "✓ 1 removed rule(s) deleted for good");
    fails(&home, &["automations", "restore", "solved"]);
    std::fs::write(&rule, RULE).unwrap();
    ok(&home, &["automations", "new", "solved", "--file", file]);

    assert!(ok(&home, &["automations", "events"]).contains("records.item.completed"));
    assert!(ok(&home, &["automations", "depth", "4"]).contains("✓ depth is 4"));
    assert!(ok(&home, &["automations", "status"]).contains("depth: 4"));
    assert!(ok(&home, &["automations", "reload"]).contains("✓ read 5 rules"), "yours and the four built in");
    assert!(ok(&home, &["automations", "history"]).contains("nothing yet"));

    // Without a terminal, edit says how to do it instead of waiting on an editor.
    let err = fails(&home, &["automations", "edit", "solved"]);
    assert!(err.contains("--file FILE"), "{err}");
}

#[test]
fn a_rule_firing_shows_in_history_and_the_short_pack_names_it() {
    let home = Home::with_leetcode();
    let rule = home.dir.path().join("rule.toml");
    std::fs::write(&rule, RULE).unwrap();
    ok(&home, &["--pack", "short", "anew", "solved", "--file", rule.to_str().unwrap()]);
    ok(&home, &["records", "add", "leetcode", "--title", "Two Sum"]);
    ok(&home, &["records", "complete", "leetcode/two-sum"]);
    let mut out = String::new();
    for _ in 0..100 {
        out = ok(&home, &["--pack", "short", "alog", "solved"]);
        if !out.contains("nothing yet") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(out.contains("solved on records.item.completed: scheduled records.update in"), "{out}");
}
