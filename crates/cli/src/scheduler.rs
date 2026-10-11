//! `shimmer scheduler …` (ADR 0015): the triggers that decide *when* work happens (CLAUDE.md
//! §1.2, §6.3). A trigger only enqueues a task; `shimmer queue` shows what then runs.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use shimmer_core::Result;

use crate::client::Client;
use crate::flags::Flags;
use crate::render::{self, table};
use crate::when;

pub const USAGE: &str = "usage: shimmer scheduler <command>

commands:
  list                                  every trigger: when it runs next, last ran, and its state
  show TRIGGER                          one trigger in full
  add OP [PARAMS] WHEN --catch-up WHAT  run OP on a schedule; PARAMS is a JSON object (default {})
      [--lane LANE]
  pause TRIGGER                         stop it firing until resumed
  resume TRIGGER                        fire again, from now on (time spent paused isn't 'missed')
  remove TRIGGER                        delete one you added (a module's own can only be paused)

WHEN is exactly one of:
  --every DURATION   e.g. 30s, 15m, 2h, 3d, 1w, 1h30m
  --cron \"EXPR\"      five fields, minute hour day month weekday; times are UTC
  --once TIME        2026-10-10 09:00 (your local time) or 2026-10-10T09:00:00Z

--catch-up says what happens to runs missed while the machine was off or asleep. It has no
default, because guessing wrong sends duplicates:
  skip       drop them (polls, health checks)
  run-once   run once on waking, however many were missed
  backfill   run once for each missed time

e.g. shimmer scheduler add records.list '{\"collection\":\"leetcode\"}' --every 1d --catch-up skip

A trigger enqueues a task when it fires; 'shimmer queue' shows it running. Add --json to any
command for raw output.";

#[derive(Clone, Debug, PartialEq)]
pub enum SchedulerCmd {
    Help,
    List,
    Show { id: String },
    Add(NewTrigger),
    Pause { id: String },
    Resume { id: String },
    Remove { id: String },
}

/// `scheduler.add`'s params, already in the protocol's shapes.
#[derive(Clone, Debug, PartialEq)]
pub struct NewTrigger {
    pub op: String,
    pub params: Value,
    /// `{"every": secs}`, `{"cron": "…"}` or `{"once": "<UTC RFC 3339>"}`.
    pub schedule: Value,
    /// `skip`, `run_once` or `backfill`, as the protocol spells them.
    pub catch_up: &'static str,
    pub lane: Option<String>,
}

const CATCH_UP_HELP: &str = "skip (drop missed runs), run-once (one run on waking, however many were missed) \
                             or backfill (one run for each missed time)";

/// `words` are what follows `shimmer scheduler`.
pub fn parse(words: Vec<String>) -> std::result::Result<SchedulerCmd, String> {
    let mut words = words.into_iter();
    let Some(sub) = words.next() else { return Ok(SchedulerCmd::Help) };
    let options: &[&str] = if sub == "add" { &["--every", "--cron", "--once", "--catch-up", "--lane"] } else { &[] };
    let flags = Flags::split(words.collect(), options, &[]).map_err(|e| format!("scheduler {sub}: {e}"))?;
    let id = || match flags.positional.as_slice() {
        [id] => Ok(id.clone()),
        _ => Err(format!("usage: shimmer scheduler {sub} TRIGGER")),
    };
    match sub.as_str() {
        "help" => Ok(SchedulerCmd::Help),
        "list" if flags.positional.is_empty() => Ok(SchedulerCmd::List),
        "list" => Err("'scheduler list' takes no arguments".into()),
        "show" => Ok(SchedulerCmd::Show { id: id()? }),
        "pause" => Ok(SchedulerCmd::Pause { id: id()? }),
        "resume" => Ok(SchedulerCmd::Resume { id: id()? }),
        "remove" => Ok(SchedulerCmd::Remove { id: id()? }),
        "add" => add(&flags).map(SchedulerCmd::Add),
        other => Err(format!("unknown scheduler command '{other}'; see 'shimmer scheduler --help'")),
    }
}

fn add(flags: &Flags) -> std::result::Result<NewTrigger, String> {
    let (op, params) = match flags.positional.as_slice() {
        [op] => (op.clone(), json!({})),
        [op, params] => {
            let v: Value = serde_json::from_str(params).map_err(|e| format!("PARAMS is not valid JSON: {e}"))?;
            if !v.is_object() {
                return Err("PARAMS must be a JSON object, e.g. '{\"collection\":\"leetcode\"}'".into());
            }
            (op.clone(), v)
        }
        _ => {
            return Err(
                "usage: shimmer scheduler add OP [PARAMS] WHEN --catch-up WHAT; see 'shimmer scheduler --help'".into()
            )
        }
    };
    let schedule = match (flags.value("--every"), flags.value("--cron"), flags.value("--once")) {
        (Some(d), None, None) => json!({ "every": when::parse_duration(d)? }),
        (None, Some(expr), None) => json!({ "cron": expr }),
        (None, None, Some(t)) => json!({ "once": when::parse_once(t)?.to_rfc3339() }),
        _ => return Err("'scheduler add' needs exactly one of --every DURATION, --cron \"EXPR\" or --once TIME".into()),
    };
    let catch_up = match flags.value("--catch-up") {
        Some("skip") => "skip",
        Some("run-once" | "run_once") => "run_once",
        Some("backfill") => "backfill",
        Some(other) => return Err(format!("--catch-up '{other}' isn't one of {CATCH_UP_HELP}")),
        None => return Err(format!("'scheduler add' needs --catch-up: {CATCH_UP_HELP}. There is no default.")),
    };
    Ok(NewTrigger { op, params, schedule, catch_up, lane: flags.value("--lane").map(String::from) })
}

pub async fn run(client: &mut Client, cmd: &SchedulerCmd, json: bool) -> Result<String> {
    let now = Utc::now();
    let one = |id: &str, op: &'static str| (op, json!({ "trigger_id": id }));
    let (op, params) = match cmd {
        SchedulerCmd::Help => return Ok(USAGE.into()),
        SchedulerCmd::List => {
            let data = client.call("scheduler.list", json!({})).await?;
            return Ok(if json { render::json(&data) } else { list(&data, now) });
        }
        SchedulerCmd::Show { id } => {
            let data = client.call("scheduler.get", json!({"trigger_id": id})).await?;
            return Ok(if json { render::json(&data) } else { trigger(&data, now) });
        }
        SchedulerCmd::Add(t) => {
            let mut params = json!({"schedule": t.schedule, "op": t.op, "params": t.params, "catch_up": t.catch_up});
            if let Some(lane) = &t.lane {
                params["lane"] = json!(lane);
            }
            let data = client.call("scheduler.add", params).await?;
            if json {
                return Ok(render::json(&data));
            }
            let id = data["trigger_id"].as_str().unwrap_or("?").to_string();
            // Just the new trigger, as `show` gets it (#104). It's already added, so if this
            // fails, say so plainly rather than failing the command.
            let new = client.call("scheduler.get", json!({"trigger_id": id})).await.ok();
            return Ok(added(&id, new.as_ref(), now));
        }
        SchedulerCmd::Pause { id } => one(id, "scheduler.pause"),
        SchedulerCmd::Resume { id } => one(id, "scheduler.resume"),
        SchedulerCmd::Remove { id } => one(id, "scheduler.remove"),
    };
    let data = client.call(op, params).await?;
    if json {
        return Ok(render::json(&data));
    }
    let id = match cmd {
        SchedulerCmd::Pause { id } | SchedulerCmd::Resume { id } | SchedulerCmd::Remove { id } => id,
        _ => unreachable!("only these reach here"),
    };
    Ok(match op {
        "scheduler.pause" => format!("✓ paused {id}: it won't fire until 'shimmer scheduler resume {id}'"),
        "scheduler.resume" => format!("✓ resumed {id}: it fires again from now on"),
        _ => format!("✓ removed {id}"),
    })
}

fn added(id: &str, t: Option<&Value>, now: DateTime<Utc>) -> String {
    let Some(t) = t else { return format!("✓ added trigger {id}") };
    let next = when::timestamp(&t["next_due"]).map_or("never (already in the past)".into(), |at| {
        format!("{} ({})", when::relative(at, now), when::local(at))
    });
    format!(
        "✓ added trigger {id}: {} {}, catch-up {}\n  first run: {next}",
        render::cell(&t["op"]),
        schedule(&t["schedule"]),
        catch_up(&t["catch_up"])
    )
}

/// Every trigger in one table, or a line saying there are none.
fn list(data: &Value, now: DateTime<Utc>) -> String {
    let triggers = data["triggers"].as_array().cloned().unwrap_or_default();
    if triggers.is_empty() {
        return "no triggers yet; add one with: shimmer scheduler add OP WHEN --catch-up WHAT".into();
    }
    let rows: Vec<Vec<String>> = triggers
        .iter()
        .map(|t| {
            vec![
                render::cell(&t["id"]),
                render::cell(&t["op"]),
                schedule(&t["schedule"]),
                catch_up(&t["catch_up"]),
                when::timestamp(&t["next_due"]).map_or("-".into(), |at| when::relative(at, now)),
                when::timestamp(&t["last_run"]).map_or("never".into(), |at| when::relative(at, now)),
                state(t),
                source(&t["source"]),
            ]
        })
        .collect();
    let header = ["ID", "OP", "SCHEDULE", "CATCH-UP", "NEXT", "LAST", "STATE", "FROM"].map(String::from);
    table(&header, &rows)
}

/// One trigger as label / value lines.
fn trigger(t: &Value, now: DateTime<Utc>) -> String {
    let time = |v: &Value, none: &str| {
        when::timestamp(v).map_or(none.to_string(), |at| format!("{} ({})", when::local(at), when::relative(at, now)))
    };
    let mut out = format!("{}  ({})\n", render::cell(&t["id"]), state(t));
    let mut line = |label: &str, value: String| out.push_str(&format!("  {label:<10}{value}\n"));
    line("op", render::cell(&t["op"]));
    line("params", serde_json::to_string(&t["params"]).unwrap_or_default());
    line("schedule", schedule(&t["schedule"]));
    line("catch-up", catch_up(&t["catch_up"]));
    line("lane", if t["lane"].is_null() { "the op's own".into() } else { render::cell(&t["lane"]) });
    line("next run", time(&t["next_due"], "-"));
    line("last run", time(&t["last_run"], "never"));
    line("from", source(&t["source"]));
    out.trim_end().to_string()
}

/// `every 2d`, `cron 0 9 * * 1 (UTC)` or `once 2026-10-10 09:00:00`.
fn schedule(v: &Value) -> String {
    if let Some(secs) = v["every"].as_u64() {
        format!("every {}", when::format_duration(secs))
    } else if let Some(expr) = v["cron"].as_str() {
        format!("cron {expr} (UTC)")
    } else if let Some(at) = when::timestamp(&v["once"]) {
        format!("once {}", when::local(at))
    } else {
        render::cell(v)
    }
}

/// The protocol's `run_once` as the CLI spells it.
fn catch_up(v: &Value) -> String {
    render::cell(v).replace('_', "-")
}

fn state(t: &Value) -> String {
    if t["done"].as_bool() == Some(true) {
        "done".into()
    } else if t["paused"].as_bool() == Some(true) {
        "paused".into()
    } else {
        "active".into()
    }
}

/// `you`, or `module <id>` for one a module declared (it can be paused, not removed).
fn source(v: &Value) -> String {
    match v["type"].as_str() {
        Some("user") => "you".into(),
        Some("module") => format!("module {}", v["id"].as_str().unwrap_or("?")),
        _ => render::cell(v),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn p(s: &[&str]) -> std::result::Result<SchedulerCmd, String> {
        parse(s.iter().map(|w| w.to_string()).collect())
    }

    #[test]
    fn simple_commands_parse() {
        assert_eq!(p(&[]), Ok(SchedulerCmd::Help));
        assert_eq!(p(&["list"]), Ok(SchedulerCmd::List));
        for (word, want) in [
            ("show", SchedulerCmd::Show { id: "usr-1".into() }),
            ("pause", SchedulerCmd::Pause { id: "usr-1".into() }),
            ("resume", SchedulerCmd::Resume { id: "usr-1".into() }),
            ("remove", SchedulerCmd::Remove { id: "usr-1".into() }),
        ] {
            assert_eq!(p(&[word, "usr-1"]), Ok(want));
            assert!(p(&[word]).unwrap_err().contains("TRIGGER"), "{word}");
        }
        assert!(p(&["frob"]).unwrap_err().contains("unknown scheduler command"));
    }

    #[test]
    fn add_builds_the_protocols_shapes() {
        let SchedulerCmd::Add(t) = p(&[
            "add",
            "records.list",
            r#"{"collection":"leetcode"}"#,
            "--every",
            "1d",
            "--catch-up",
            "run-once",
            "--lane=default",
        ])
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            t,
            NewTrigger {
                op: "records.list".into(),
                params: json!({"collection": "leetcode"}),
                schedule: json!({"every": 86_400}),
                catch_up: "run_once",
                lane: Some("default".into()),
            }
        );
        let SchedulerCmd::Add(t) = p(&["add", "core.x", "--cron", "0 9 * * 1", "--catch-up", "skip"]).unwrap() else {
            panic!()
        };
        assert_eq!((t.schedule, t.params), (json!({"cron": "0 9 * * 1"}), json!({})));
        let SchedulerCmd::Add(t) =
            p(&["add", "core.x", "--once", "2026-10-10T09:00:00+01:00", "--catch-up", "backfill"]).unwrap()
        else {
            panic!()
        };
        assert_eq!((t.schedule, t.catch_up), (json!({"once": "2026-10-10T08:00:00+00:00"}), "backfill"));
    }

    #[test]
    fn add_mistakes_are_explained() {
        let e = |w: &[&str]| p(w).unwrap_err();
        assert!(e(&["add", "x.y", "--every", "1d"]).contains("needs --catch-up: skip (drop missed runs), run-once"));
        assert!(e(&["add", "x.y", "--every", "1d", "--catch-up", "sometimes"])
            .contains("--catch-up 'sometimes' isn't one of"));
        assert!(e(&["add", "x.y", "--catch-up", "skip"]).contains("exactly one of --every"));
        assert!(
            e(&["add", "x.y", "--every", "1d", "--cron", "* * * * *", "--catch-up", "skip"]).contains("exactly one of")
        );
        assert!(e(&["add", "x.y", "--every", "2", "--catch-up", "skip"]).contains("'2' isn't a duration"));
        assert!(e(&["add", "x.y", "[1]", "--every", "1d", "--catch-up", "skip"]).contains("must be a JSON object"));
        assert!(e(&["add", "--every", "1d", "--catch-up", "skip"]).contains("usage: shimmer scheduler add OP"));
        assert!(e(&["pause", "x", "--every", "1d"]).contains("unknown option '--every'"));
    }

    fn sample(now: DateTime<Utc>) -> Value {
        json!({"triggers": [
            {"id": "usr-1", "source": {"type": "user"}, "schedule": {"every": 172_800}, "op": "records.list",
             "params": {"collection": "leetcode"}, "catch_up": "run_once", "lane": null, "fallback": null,
             "next_due": (now + Duration::seconds(7_201)).to_rfc3339(), "last_run": null, "paused": false, "done": false},
            {"id": "digest", "source": {"type": "module", "id": "notify"}, "schedule": {"cron": "0 9 * * 1"},
             "op": "notify.digest", "params": {}, "catch_up": "skip", "lane": "notify", "fallback": null,
             "next_due": null, "last_run": (now - Duration::seconds(180)).to_rfc3339(), "paused": true, "done": false}
        ]})
    }

    #[test]
    fn list_shows_each_trigger_readably() {
        let now = Utc::now();
        let out = list(&sample(now), now);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["ID", "OP", "SCHEDULE", "CATCH-UP", "NEXT", "LAST", "STATE", "FROM"]
        );
        let row = |i: usize| lines[i].split("  ").map(str::trim).filter(|c| !c.is_empty()).collect::<Vec<_>>();
        assert_eq!(row(1), ["usr-1", "records.list", "every 2d", "run-once", "in 2h", "never", "active", "you"]);
        assert_eq!(
            row(2),
            ["digest", "notify.digest", "cron 0 9 * * 1 (UTC)", "skip", "-", "3m ago", "paused", "module notify"]
        );
        assert!(list(&json!({"triggers": []}), now).starts_with("no triggers yet"));
    }

    #[test]
    fn show_and_added_read_naturally() {
        let now = Utc::now();
        let data = sample(now);
        let usr1 = &data["triggers"][0];
        assert_eq!(usr1["id"], "usr-1");
        let out = trigger(usr1, now);
        assert!(out.starts_with("usr-1  (active)\n  op        records.list\n  params    {\"collection\":\"leetcode\"}\n  schedule  every 2d"), "{out}");
        assert!(
            out.contains("  lane      the op's own") && out.contains("(in 2h)") && out.contains("  last run  never"),
            "{out}"
        );
        let msg = added("usr-1", Some(usr1), now);
        assert!(
            msg.starts_with("✓ added trigger usr-1: records.list every 2d, catch-up run-once\n  first run: in 2h ("),
            "{msg}"
        );
        assert_eq!(added("usr-9", None, now), "✓ added trigger usr-9", "still says it was added");
    }
}
