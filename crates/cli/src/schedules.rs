//! Schedules that name something by its old name (ADRs 0026 §2a, 0030 §2). The scheduler doesn't
//! know what its triggers' params mean (§1.2) and modules don't reach into it (§1.3), so when a
//! workspace, collection or record is renamed or removed, the client finds the triggers that
//! name it, says so, and moves them only when the person says yes, with ops that already exist.

use std::fmt::Write as _;

use serde_json::{json, Map, Value};
use shimmer_core::Result;

use crate::client::Client;
use crate::render::cell;
use crate::workspaces::Prompt;

/// The schedules you added that haven't finished and whose op starts with `prefix` (e.g.
/// `"records."`) and whose params hold every key in `names` with that value.
pub fn naming(all: &Value, prefix: &str, names: &Map<String, Value>) -> Vec<Value> {
    all["triggers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| {
            t["source"]["type"] == "user"
                && t["done"] != true
                && t["op"].as_str().is_some_and(|op| op.starts_with(prefix))
                && names.iter().all(|(k, v)| t["params"][k] == *v)
        })
        .cloned()
        .collect()
}

/// Every schedule naming it, from `scheduler.list`. None when the scheduler can't be asked: the
/// rename itself already happened, and this is advice about it.
pub async fn find(client: &mut Client, prefix: &str, names: &Map<String, Value>) -> Vec<Value> {
    let all = client.call("scheduler.list", json!({})).await.unwrap_or(Value::Null);
    naming(&all, prefix, names)
}

/// "usr-01… workspaces.activate, cron 0 9 * * 1-5".
pub fn line(t: &Value) -> String {
    let s = &t["schedule"];
    let when = if let Some(c) = s["cron"].as_str() {
        format!("cron {c}")
    } else if let Some(n) = s["every"].as_u64() {
        format!("every {n}s")
    } else if let Some(at) = s["once"].as_str() {
        format!("once at {at}")
    } else {
        s.to_string()
    };
    let paused = if t["paused"] == true { " (paused)" } else { "" };
    format!("{} {}, {when}{paused}", cell(&t["id"]), cell(&t["op"]))
}

/// Move one schedule: the same trigger added again with `changes` merged into its params (paused
/// if it was), then the old one removed. The old one stays if adding fails. Its new id.
pub async fn move_one(client: &mut Client, t: &Value, changes: &Map<String, Value>) -> Result<String> {
    let mut params = t["params"].clone();
    for (k, v) in changes {
        params[k] = v.clone();
    }
    let mut add = json!({"schedule": t["schedule"], "op": t["op"], "params": params, "catch_up": t["catch_up"]});
    for key in ["lane", "fallback"] {
        if !t[key].is_null() {
            add[key] = t[key].clone();
        }
    }
    let new = client.call("scheduler.add", add).await?;
    let new_id = new["trigger_id"].as_str().unwrap_or_default().to_owned();
    if t["paused"] == true {
        client.call("scheduler.pause", json!({"trigger_id": new_id})).await?;
    }
    client.call("scheduler.remove", json!({"trigger_id": t["id"]})).await?;
    Ok(new_id)
}

pub fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// What a rename did to the schedules that named the old name.
pub struct Rename<'a> {
    /// The old and new names, as the person wrote them: `site`, `jobs/two-sum`.
    pub from: &'a str,
    pub to: &'a str,
    /// The params to change on each, e.g. `{"collection": "apps"}`.
    pub changes: Map<String, Value>,
    /// The command that undoes the rename.
    pub undo: String,
    /// `--move-schedules`: move them without asking.
    pub yes: bool,
}

/// After a rename: list the schedules `old` that still use the old name, ask (at a terminal)
/// or go by `--move-schedules`, and move them. The text to add to the rename's output.
pub async fn follow_rename(client: &mut Client, prompt: &mut dyn Prompt, old: &[Value], r: &Rename<'_>) -> String {
    let mut text = String::new();
    if old.is_empty() {
        return text;
    }
    let (from, to) = (r.from, r.to);
    let verb = if old.len() == 1 { "uses" } else { "use" };
    let mut listing = format!("\n  {} still {verb} the old name {from}:", plural(old.len(), "schedule"));
    for t in old {
        let _ = write!(listing, "\n    {}", line(t));
    }
    let asked = !r.yes && prompt.interactive();
    let go = r.yes || {
        if asked {
            eprintln!("{}", listing.trim_start_matches('\n'));
            prompt.confirm(&format!("Move {} to {to}?", if old.len() == 1 { "it" } else { "them" }))
        } else {
            false
        }
    };
    if !asked {
        text.push_str(&listing);
    }
    if !go {
        let _ = write!(
            text,
            "\n  they'll fail until moved: run '{}' to undo, or remove each (shimmer scheduler remove ID) \
             and add it again for {to}",
            r.undo
        );
        return text;
    }
    for t in old {
        let line = match move_one(client, t, &r.changes).await {
            Ok(new) => format!("moved {} → {new}", cell(&t["id"])),
            Err(e) => format!("couldn't move {}: {} (it still uses {from})", cell(&t["id"]), e.message),
        };
        let _ = write!(text, "\n  {line}");
    }
    text
}

/// After a removal: the schedules that still name what was removed, which will fail until it is
/// restored or they are removed. Never removes them: restoring makes them work again.
pub fn after_removal(old: &[Value], what: &str, restore: &str) -> String {
    if old.is_empty() {
        return String::new();
    }
    let verb = if old.len() == 1 { "uses" } else { "use" };
    let mut text = format!("\n  {} still {verb} {what}:", plural(old.len(), "schedule"));
    for t in old {
        let _ = write!(text, "\n    {}", line(t));
    }
    let _ =
        write!(text, "\n  they'll fail until you restore it ({restore}) or remove them (shimmer scheduler remove ID)");
    text
}

/// `{"id": "site"}`, `{"collection": "jobs", "id": "two-sum"}`: the params that name something.
pub fn names(pairs: &[(&str, &str)]) -> Map<String, Value> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), json!(v))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Value {
        json!({"triggers": [
            {"id": "usr-1", "source": {"type": "user"}, "op": "workspaces.activate", "params": {"id": "site"},
             "schedule": {"cron": "0 9 * * 1-5"}, "paused": false, "done": false},
            {"id": "usr-2", "source": {"type": "user"}, "op": "workspaces.stop", "params": {"id": "site"},
             "schedule": {"every": 3600}, "paused": true, "done": false},
            {"id": "usr-3", "source": {"type": "user"}, "op": "workspaces.activate", "params": {"id": "other"},
             "schedule": {"every": 60}, "done": false},
            {"id": "usr-4", "source": {"type": "user"}, "op": "workspaces.activate", "params": {"id": "site"},
             "schedule": {"once": "2026-10-01T09:00:00Z"}, "done": true},
            {"id": "records-x", "source": {"type": "module", "id": "records"}, "op": "workspaces.activate",
             "params": {"id": "site"}, "schedule": {"every": 60}, "done": false},
            {"id": "usr-5", "source": {"type": "user"}, "op": "records.list", "params": {"id": "site"},
             "schedule": {"every": 60}, "done": false},
            {"id": "usr-6", "source": {"type": "user"}, "op": "records.list",
             "params": {"collection": "jobs", "filter": {"status": "todo"}}, "schedule": {"every": 60}, "done": false},
            {"id": "usr-7", "source": {"type": "user"}, "op": "records.complete",
             "params": {"collection": "jobs", "id": "two-sum"}, "schedule": {"every": 60}, "done": false},
            {"id": "usr-8", "source": {"type": "user"}, "op": "records.complete",
             "params": {"collection": "leetcode", "id": "two-sum"}, "schedule": {"every": 60}, "done": false}]})
    }

    fn ids(found: &[Value]) -> Vec<String> {
        found.iter().map(|t| cell(&t["id"])).collect()
    }

    #[test]
    fn naming_finds_your_unfinished_schedules_for_one_thing() {
        let site = naming(&all(), "workspaces.", &names(&[("id", "site")]));
        assert_eq!(ids(&site), ["usr-1", "usr-2"], "yours, unfinished, a workspaces op, this workspace");
        assert_eq!(
            site.iter().map(line).collect::<Vec<_>>(),
            ["usr-1 workspaces.activate, cron 0 9 * * 1-5", "usr-2 workspaces.stop, every 3600s (paused)"]
        );
        // A collection: every records op that names it; a record: its collection and id both.
        assert_eq!(ids(&naming(&all(), "records.", &names(&[("collection", "jobs")]))), ["usr-6", "usr-7"]);
        let record = naming(&all(), "records.", &names(&[("collection", "jobs"), ("id", "two-sum")]));
        assert_eq!(ids(&record), ["usr-7"]);
        assert!(naming(&Value::Null, "records.", &names(&[("collection", "jobs")])).is_empty());
    }

    #[test]
    fn a_removal_lists_what_will_fail() {
        assert_eq!(after_removal(&[], "jobs", "x"), "");
        let jobs = naming(&all(), "records.", &names(&[("collection", "jobs")]));
        assert_eq!(
            after_removal(&jobs, "jobs", "shimmer records restore-collection jobs"),
            "\n  2 schedules still use jobs:\n    usr-6 records.list, every 60s\n    usr-7 records.complete, every 60s\n  \
             they'll fail until you restore it (shimmer records restore-collection jobs) or remove them \
             (shimmer scheduler remove ID)"
        );
    }
}
