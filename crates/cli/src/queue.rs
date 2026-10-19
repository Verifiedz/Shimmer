//! `shimmer queue …` (ADR 0015): what the machine is doing right now (CLAUDE.md §1.2). Every
//! command is one or more of the daemon's `queue.*` ops; nothing here decides anything.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use shimmer_core::{Error, Result};

use crate::client::Client;
use crate::flags::Flags;
use crate::render::{self, table};
use crate::style::{paint, Role};
use crate::when;

pub const USAGE: &str = "usage: shimmer queue <command>

commands:
  list [--lane LANE]             running and queued tasks, per lane
  show TASK                      one task in full, including finished ones the daemon remembers
  cancel TASK                    cancel a queued task, or ask a running one to stop
  move TASK --before OTHER       move a queued task ahead of another in the same lane
  move TASK --last               move it to the back of its lane

Every unit of work runs through the queue: workspace launches, scheduled tasks, anything a module
starts. Each lane runs a set number of tasks at a time; reordering is within a lane only, and a
running task can be cancelled but not moved. TASK is the id as 'shimmer queue' shows it: its
ending is enough (at least 4 characters). Add --json to any command for raw output.";

#[derive(Clone, Debug, PartialEq)]
pub enum QueueCmd {
    Help,
    List {
        lane: Option<String>,
    },
    Show {
        id: String,
    },
    Cancel {
        id: String,
    },
    /// `before: None` moves to the back of the lane.
    Move {
        id: String,
        before: Option<String>,
    },
}

/// `words` are what follows `shimmer queue`.
pub fn parse(words: Vec<String>) -> std::result::Result<QueueCmd, String> {
    let mut words = words.into_iter();
    let Some(sub) = words.next() else { return Ok(QueueCmd::Help) };
    let (options, switches): (&[&str], &[&str]) = match sub.as_str() {
        "list" => (&["--lane"], &[]),
        "move" => (&["--before"], &["--last"]),
        _ => (&[], &[]),
    };
    let flags = Flags::split(words.collect(), options, switches).map_err(|e| format!("queue {sub}: {e}"))?;
    let one = |what: &str| match flags.positional.as_slice() {
        [id] => Ok(id.clone()),
        _ => Err(format!("usage: shimmer queue {sub} {what}")),
    };
    match sub.as_str() {
        "help" => Ok(QueueCmd::Help),
        "list" if flags.positional.is_empty() => Ok(QueueCmd::List { lane: flags.value("--lane").map(String::from) }),
        "list" => Err("'queue list' takes no arguments; use --lane LANE to pick one lane".into()),
        "show" => Ok(QueueCmd::Show { id: one("TASK")? }),
        "cancel" => Ok(QueueCmd::Cancel { id: one("TASK")? }),
        "move" => {
            let id = one("TASK --before OTHER | --last")?;
            match (flags.value("--before"), flags.has("--last")) {
                (Some(other), false) => Ok(QueueCmd::Move { id, before: Some(other.into()) }),
                (None, true) => Ok(QueueCmd::Move { id, before: None }),
                _ => Err("'queue move' needs exactly one of --before OTHER or --last".into()),
            }
        }
        other => Err(format!("unknown queue command '{other}'; see 'shimmer queue --help'")),
    }
}

pub async fn run(client: &mut Client, cmd: &QueueCmd, json: bool) -> Result<String> {
    let now = Utc::now();
    match cmd {
        QueueCmd::Help => Ok(USAGE.into()),
        QueueCmd::List { lane } => {
            let params = lane.as_ref().map_or_else(|| json!({}), |l| json!({ "lane": l }));
            let data = client.call("queue.list", params).await?;
            Ok(if json { render::json(&data) } else { list(&data, now) })
        }
        QueueCmd::Show { id } => {
            let data = client.call("queue.show", json!({ "task_id": id })).await?;
            Ok(if json { render::json(&data) } else { task(&data, now) })
        }
        QueueCmd::Cancel { id } => {
            // Find it first, then act on its full id, and say which one it was (#183's review).
            let t = client.call("queue.show", json!({ "task_id": id })).await?;
            let full = render::cell(&t["task_id"]);
            let data = client.call("queue.cancel", json!({ "task_id": full })).await?;
            Ok(if json { render::json(&data) } else { cancelled(&named(&t), &data) })
        }
        QueueCmd::Move { id, before } => {
            // The lane comes from the task itself; `queue.move` needs it, and the lane's current
            // `queue_version`, so a move based on a stale view is refused rather than misplaced.
            let t = client.call("queue.show", json!({ "task_id": id })).await?;
            let lane = t["lane"].as_str().ok_or_else(|| Error::internal("task has no lane"))?.to_string();
            let full = render::cell(&t["task_id"]);
            let other = match before {
                Some(b) => Some(client.call("queue.show", json!({ "task_id": b })).await?),
                None => None,
            };
            let lanes = client.call("queue.list", json!({ "lane": lane })).await?;
            let version = lanes["lanes"][0]["queue_version"].clone();
            let before_id = other.as_ref().map(|o| render::cell(&o["task_id"]));
            let data = client
                .call(
                    "queue.move",
                    json!({"lane": lane, "task_id": full, "before": before_id, "queue_version": version}),
                )
                .await?;
            Ok(if json {
                render::json(&data)
            } else {
                match &other {
                    Some(o) => format!("✓ moved {} ahead of {} in lane {lane}", named(&t), named(o)),
                    None => format!("✓ moved {} to the back of lane {lane}", named(&t)),
                }
            })
        }
    }
}

/// One table of every running and queued task, or a line saying the queue is idle.
fn list(data: &Value, now: DateTime<Utc>) -> String {
    let lanes = data["lanes"].as_array().cloned().unwrap_or_default();
    let mut rows = Vec::new();
    let all: Vec<String> = lanes
        .iter()
        .flat_map(|l| l["running"].as_array().into_iter().flatten().chain(l["queued"].as_array().into_iter().flatten()))
        .map(|t| render::cell(&t["task_id"]))
        .collect();
    let mut short = render::short_ids(&all).into_iter();
    for lane in &lanes {
        let id = lane["id"].as_str().unwrap_or("?");
        for t in lane["running"].as_array().into_iter().flatten().chain(lane["queued"].as_array().into_iter().flatten())
        {
            let since = when::timestamp(&t["started_at"]).or_else(|| when::timestamp(&t["enqueued_at"]));
            rows.push(vec![
                id.to_string(),
                short.next().unwrap_or_default(),
                render::cell(&t["status"]),
                render::cell(&t["op"]),
                origin(&t["origin"]),
                progress(&t["progress"]),
                since.map_or("-".into(), |s| when::age(s, now)),
            ]);
        }
    }
    let summary: Vec<String> = lanes
        .iter()
        .map(|l| format!("{} ({} at a time)", l["id"].as_str().unwrap_or("?"), l["max_concurrent"]))
        .collect();
    if rows.is_empty() {
        return format!("nothing queued or running\nlanes: {}", summary.join(", "));
    }
    let header = ["LANE", "TASK", "STATUS", "OP", "FROM", "PROGRESS", "AGE"].map(String::from);
    let n = rows.len();
    format!("{}\n{n} task{}; lanes: {}", table(&header, &rows), if n == 1 { "" } else { "s" }, summary.join(", "))
}

/// One task as label / value lines.
fn task(t: &Value, now: DateTime<Utc>) -> String {
    let time = |v: &Value| {
        when::timestamp(v).map_or("-".into(), |at| format!("{} ({})", when::local(at), when::relative(at, now)))
    };
    let id = paint(Role::Name, &render::cell(&t["task_id"]));
    let mut out = format!("{id}  ({})\n", crate::style::state(&render::cell(&t["status"])));
    let mut line = |label: &str, value: String| out.push_str(&format!("  {}{value}\n", crate::style::label(label, 10)));
    line("op", render::cell(&t["op"]));
    line("lane", render::cell(&t["lane"]));
    line("from", origin(&t["origin"]));
    line("priority", priority(&t["priority"]));
    line("progress", progress(&t["progress"]));
    line("enqueued", time(&t["enqueued_at"]));
    line("started", time(&t["started_at"]));
    line("finished", time(&t["finished_at"]));
    if !t["error"].is_null() {
        line("error", render::cell(&t["error"]));
    }
    out.trim_end().to_string()
}

/// A task as a reply names it: the ending lists show, and what it runs, e.g. `kfxbpp (workspaces.activate)`.
fn named(t: &Value) -> String {
    format!("{} ({})", render::short_id(&render::cell(&t["task_id"])), render::cell(&t["op"]))
}

fn cancelled(id: &str, data: &Value) -> String {
    match data["was"].as_str() {
        Some("running") => format!("✓ asked {id} to stop: it was running, and ends at its next cancellation check"),
        _ => format!("✓ cancelled {id}: it was still queued, so it never ran"),
    }
}

/// `you`, `trigger <id>` or `module <id>` (CLAUDE.md §5 `Origin`).
fn origin(v: &Value) -> String {
    match v["type"].as_str() {
        Some("user") => "you".into(),
        Some("scheduler") => format!("trigger {}", v["trigger_id"].as_str().unwrap_or("?")),
        Some("module") => format!("module {}", v["id"].as_str().unwrap_or("?")),
        _ => render::cell(v),
    }
}

/// `scheduled`, `normal`, or `overridden by X` (CLAUDE.md §5 `Priority`).
fn priority(v: &Value) -> String {
    match &v["overridden"] {
        Value::Object(o) => format!("overridden by {}", o.get("by").and_then(Value::as_str).unwrap_or("?")),
        _ => render::cell(v),
    }
}

/// `50% step 2/3 editor`, just the note, or `-` before anything is reported.
fn progress(v: &Value) -> String {
    let fraction = v["fraction"].as_f64().filter(|f| *f > 0.0);
    let note = v["note"].as_str().filter(|n| !n.is_empty());
    match (fraction, note) {
        (Some(f), Some(n)) => format!("{:.0}% {n}", f * 100.0),
        (Some(f), None) => format!("{:.0}%", f * 100.0),
        (None, Some(n)) => n.to_string(),
        (None, None) => "-".into(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn p(s: &[&str]) -> std::result::Result<QueueCmd, String> {
        parse(s.iter().map(|w| w.to_string()).collect())
    }

    #[test]
    fn commands_parse() {
        assert_eq!(p(&[]), Ok(QueueCmd::Help));
        assert_eq!(p(&["list"]), Ok(QueueCmd::List { lane: None }));
        assert_eq!(p(&["list", "--lane=workspaces"]), Ok(QueueCmd::List { lane: Some("workspaces".into()) }));
        assert_eq!(p(&["show", "01ABC"]), Ok(QueueCmd::Show { id: "01ABC".into() }));
        assert_eq!(p(&["cancel", "01ABC"]), Ok(QueueCmd::Cancel { id: "01ABC".into() }));
        assert_eq!(p(&["move", "A", "--before", "B"]), Ok(QueueCmd::Move { id: "A".into(), before: Some("B".into()) }));
        assert_eq!(p(&["move", "A", "--last"]), Ok(QueueCmd::Move { id: "A".into(), before: None }));
    }

    #[test]
    fn mistakes_are_explained() {
        assert!(p(&["show"]).unwrap_err().contains("usage: shimmer queue show TASK"));
        assert!(p(&["move", "A"]).unwrap_err().contains("exactly one of --before OTHER or --last"));
        assert!(p(&["move", "A", "--last", "--before", "B"]).unwrap_err().contains("exactly one"));
        assert!(p(&["list", "x"]).unwrap_err().contains("takes no arguments"));
        assert!(p(&["cancel", "A", "--last"]).unwrap_err().contains("unknown option '--last'"));
        assert!(p(&["frob"]).unwrap_err().contains("unknown queue command 'frob'"));
    }

    fn sample(now: DateTime<Utc>) -> Value {
        let t = |s: i64| (now - Duration::seconds(s)).to_rfc3339();
        json!({"lanes": [
            {"id": "default", "max_concurrent": 4, "queue_version": 0, "running": [], "queued": []},
            {"id": "workspaces", "max_concurrent": 1, "queue_version": 3,
             "running": [{"task_id": "01RUN", "lane": "workspaces", "op": "workspaces.activate", "status": "running",
                          "priority": "normal", "origin": {"type": "user"}, "progress": {"fraction": 0.5, "note": "step 2/3 editor"},
                          "enqueued_at": t(30), "started_at": t(12), "finished_at": null, "error": null}],
             "queued": [{"task_id": "01WAIT", "lane": "workspaces", "op": "workspaces.activate", "status": "queued",
                         "priority": "scheduled", "origin": {"type": "scheduler", "trigger_id": "usr-1"},
                         "progress": {"fraction": 0.0, "note": ""}, "enqueued_at": t(5), "started_at": null,
                         "finished_at": null, "error": null}]}
        ]})
    }

    #[test]
    fn list_is_one_table_with_lanes_summarised() {
        let now = Utc::now();
        let out = list(&sample(now), now);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["LANE", "TASK", "STATUS", "OP", "FROM", "PROGRESS", "AGE"]
        );
        assert!(
            lines[1].starts_with("workspaces  01RUN   running  workspaces.activate  you")
                && lines[1].ends_with("50% step 2/3 editor  12s"),
            "{out}"
        );
        assert!(
            lines[2].contains("01WAIT  queued   workspaces.activate  trigger usr-1") && lines[2].ends_with("5s"),
            "{out}"
        );
        assert_eq!(lines[3], "2 tasks; lanes: default (4 at a time), workspaces (1 at a time)");
    }

    #[test]
    fn an_idle_queue_says_so() {
        let now = Utc::now();
        let idle = json!({"lanes": [{"id": "default", "max_concurrent": 4, "running": [], "queued": []}]});
        assert_eq!(list(&idle, now), "nothing queued or running\nlanes: default (4 at a time)");
    }

    #[test]
    fn a_task_shows_every_field() {
        let now = Utc::now();
        let mut t = sample(now)["lanes"][1]["running"][0].clone();
        t["status"] = json!("failed");
        t["error"] = json!("exit code 1");
        t["priority"] = json!({"overridden": {"by": "gaurav", "at": now.to_rfc3339()}});
        let out = task(&t, now);
        assert!(out.starts_with("01RUN  (failed)\n  op        workspaces.activate\n  lane      workspaces\n  from      you\n  priority  overridden by gaurav"), "{out}");
        assert!(out.contains("  started   ") && out.contains("(12s ago)") && out.contains("  finished  -"), "{out}");
        assert!(out.ends_with("  error     exit code 1"), "{out}");
    }

    #[test]
    fn cancel_says_which_case_happened() {
        assert!(cancelled("01A", &json!({"cancelled": true, "was": "queued"})).contains("never ran"));
        assert!(cancelled("01A", &json!({"cancelled": true, "was": "running"})).contains("next cancellation check"));
    }
}
