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
  --cron-local \"EXPR\" the same, in your local timezone (config.toml's local_timezone):
                     9am stays 9am when the clocks change
  --once TIME        2026-10-10 09:00 (your local time) or 2026-10-10T09:00:00Z

--catch-up says what happens to runs missed while the machine was off or asleep. It has no
default, because guessing wrong sends duplicates:
  skip       drop them (polls, health checks)
  run-once   run once on waking, however many were missed
  backfill   run once for each missed time

e.g. shimmer scheduler add records.list '{\"collection\":\"leetcode\"}' --every 1d --catch-up skip

A trigger enqueues a task when it fires; 'shimmer queue' shows it running. TRIGGER is the id as
'scheduler list' shows it: its ending is enough (at least 4 characters). Add --json to any command for raw output.";

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
    let options: &[&str] =
        if sub == "add" { &["--every", "--cron", "--cron-local", "--once", "--catch-up", "--lane"] } else { &[] };
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
    let given = [flags.value("--every"), flags.value("--cron"), flags.value("--cron-local"), flags.value("--once")];
    let schedule = match given {
        [Some(d), None, None, None] => json!({ "every": when::parse_duration(d)? }),
        [None, Some(expr), None, None] => json!({ "cron": expr }),
        [None, None, Some(expr), None] => json!({ "cron_local": expr }),
        [None, None, None, Some(t)] => json!({ "once": when::parse_once(t)?.to_rfc3339() }),
        _ => {
            return Err(
                "'scheduler add' needs exactly one of --every DURATION, --cron \"EXPR\", --cron-local \"EXPR\" \
                        or --once TIME"
                    .into(),
            )
        }
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
            // fails the command still succeeds (a script must not add it again), and says why
            // the details are missing.
            return Ok(match client.call("scheduler.get", json!({"trigger_id": id})).await {
                Ok(t) => added(&id, Some(&t), now),
                Err(e) => format!(
                    "{}\n  (couldn't fetch its details: {}; see 'shimmer scheduler show {id}')",
                    added(&id, None, now),
                    e.code
                ),
            });
        }
        SchedulerCmd::Pause { id } => one(id, "scheduler.pause"),
        SchedulerCmd::Resume { id } => one(id, "scheduler.resume"),
        SchedulerCmd::Remove { id } => one(id, "scheduler.remove"),
    };
    // Find the trigger first, then act on its full id, and say which one it was and what it runs:
    // never just echo what was typed (#183's review). A removed trigger can't be looked up after.
    let trigger = client.call("scheduler.get", params).await?;
    let full = trigger["id"].as_str().unwrap_or("?").to_string();
    let data = client.call(op, json!({ "trigger_id": full })).await?;
    if json {
        return Ok(render::json(&data));
    }
    let shown = render::short_id(&full);
    let what = format!("{shown} ({}, {})", render::cell(&trigger["op"]), schedule(&trigger["schedule"]));
    Ok(match op {
        "scheduler.pause" => format!("✓ paused {what}: it won't fire until 'shimmer scheduler resume {shown}'"),
        "scheduler.resume" => format!("✓ resumed {what}: it fires again from now on"),
        _ => format!("✓ removed {what}"),
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
    let ids: Vec<String> = triggers.iter().map(|t| render::cell(&t["id"])).collect();
    let rows: Vec<Vec<String>> = triggers
        .iter()
        .zip(render::short_ids(&ids))
        .map(|(t, id)| {
            vec![
                id,
                command_of(&t["op"], &t["params"]),
                schedule(&t["schedule"]),
                catch_up(&t["catch_up"]),
                when::timestamp(&t["next_due"]).map_or("-".into(), |at| when::relative(at, now)),
                when::timestamp(&t["last_run"]).map_or("never".into(), |at| when::relative(at, now)),
                state(t),
                source(&t["source"]),
            ]
        })
        .collect();
    let header = ["ID", "COMMAND", "SCHEDULE", "CATCH-UP", "NEXT", "LAST", "STATE", "FROM"].map(String::from);
    table(&header, &rows)
}

/// One trigger as label / value lines.
fn trigger(t: &Value, now: DateTime<Utc>) -> String {
    let time = |v: &Value, none: &str| {
        when::timestamp(v).map_or(none.to_string(), |at| format!("{} ({})", when::local(at), when::relative(at, now)))
    };
    let mut out = format!("{}  ({})\n", render::cell(&t["id"]), state(t));
    let mut line = |label: &str, value: String| out.push_str(&format!("  {label:<10}{value}\n"));
    line("command", command_of(&t["op"], &t["params"]));
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

/// What a trigger runs, as the command you'd type (ADR 0031 §3): `workspaces activate site`,
/// `records complete habits/stretch`. Rebuilt from its op with the command words packs already
/// know (`TARGETS`) and its params: the collection and id as `COLLECTION/ID`, everything else
/// as `--name value`. An op with no command is shown as op + params, as before. Presentation
/// only: the daemon stores, and `--json` shows, op + params.
fn command_of(op: &Value, params: &Value) -> String {
    let op = render::cell(op);
    let Some(target) = crate::packs::pack::TARGETS.iter().find(|t| t.op == op) else {
        return format!("{op} {}", serde_json::to_string(params).unwrap_or_default());
    };
    let mut words: Vec<String> = target.words.iter().map(|w| w.to_string()).collect();
    let text = |v: &Value| {
        let s = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
        if s.contains(char::is_whitespace) || s.is_empty() {
            format!("{s:?}")
        } else {
            s
        }
    };
    match (params["collection"].as_str(), params["id"].as_str()) {
        (Some(c), Some(id)) => words.push(format!("{c}/{id}")),
        (Some(c), None) => words.push(c.into()),
        (None, Some(id)) => words.push(id.into()),
        (None, None) => {}
    }
    let mut flag = |name: &str, v: &Value| {
        if !v.is_null() {
            words.push(format!("--{}", name.replace('_', "-")));
            words.push(text(v));
        }
    };
    for (key, value) in params.as_object().into_iter().flatten() {
        match key.as_str() {
            "collection" | "id" | "scheduled_for" => {}
            // A record's fields and a list's filter read as the flags that set them.
            "fields" | "filter" => {
                for (name, v) in value.as_object().into_iter().flatten() {
                    flag(name, v);
                }
            }
            other => flag(other, value),
        }
    }
    words.join(" ")
}

/// `every 2d`, `weekdays 09:00 (local)`, `cron 0 9 * * 1 (UTC)` or `once 2026-10-10 09:00:00`.
fn schedule(v: &Value) -> String {
    if let Some(secs) = v["every"].as_u64() {
        format!("every {}", when::format_duration(secs))
    } else if let Some(expr) = v["cron"].as_str() {
        format!("cron {expr} (UTC)")
    } else if let Some(expr) = v["cron_local"].as_str() {
        local_words(expr).unwrap_or_else(|| format!("cron {expr} (local)"))
    } else if let Some(at) = when::timestamp(&v["once"]) {
        format!("once {}", when::local(at))
    } else {
        render::cell(v)
    }
}

/// A local cron as `shimmer schedule` writes it, when it is one of its forms: `0 9 * * 1-5` →
/// `weekdays 09:00 (local)`.
fn local_words(expr: &str) -> Option<String> {
    const NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    let [minute, hour, "*", "*", days] = expr.split_whitespace().collect::<Vec<_>>()[..] else { return None };
    let (minute, hour): (u32, u32) = (minute.parse().ok()?, hour.parse().ok()?);
    let days = match days {
        "*" => "daily".to_owned(),
        "1-5" => "weekdays".to_owned(),
        "0,6" => "weekends".to_owned(),
        list => list
            .split(',')
            .map(|d| d.parse::<usize>().ok().and_then(|i| NAMES.get(i)).copied())
            .collect::<Option<Vec<_>>>()?
            .join(","),
    };
    (hour < 24 && minute < 60).then(|| format!("{days} {hour:02}:{minute:02} (local)"))
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

    #[test]
    fn cron_local_is_added_and_shown_as_local_time() {
        let SchedulerCmd::Add(t) = p(&["add", "x.y", "--cron-local", "0 9 * * 1-5", "--catch-up", "skip"]).unwrap()
        else {
            panic!("not an add")
        };
        assert_eq!(t.schedule, json!({"cron_local": "0 9 * * 1-5"}));
        assert_eq!(schedule(&t.schedule), "weekdays 09:00 (local)");
        let both = p(&["add", "x.y", "--cron", "0 9 * * *", "--cron-local", "0 9 * * *", "--catch-up", "skip"]);
        assert!(both.unwrap_err().contains("exactly one of"));
    }

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
            ["ID", "COMMAND", "SCHEDULE", "CATCH-UP", "NEXT", "LAST", "STATE", "FROM"]
        );
        let row = |i: usize| lines[i].split("  ").map(str::trim).filter(|c| !c.is_empty()).collect::<Vec<_>>();
        assert_eq!(
            row(1),
            ["usr-1", "records list leetcode", "every 2d", "run-once", "in 2h", "never", "active", "you"]
        );
        assert_eq!(
            row(2),
            ["digest", "notify.digest {}", "cron 0 9 * * 1 (UTC)", "skip", "-", "3m ago", "paused", "module notify"]
        );
        assert!(list(&json!({"triggers": []}), now).starts_with("no triggers yet"));
    }

    #[test]
    fn a_trigger_reads_as_the_command_it_runs() {
        let c = |op: &str, params: Value| command_of(&json!(op), &params);
        assert_eq!(c("workspaces.activate", json!({"id": "site"})), "workspaces activate site");
        assert_eq!(
            c("records.complete", json!({"collection": "habits", "id": "stretch", "scheduled_for": "x"})),
            "records complete habits/stretch"
        );
        assert_eq!(
            c(
                "records.add",
                json!({"collection": "jobs", "fields": {"position": "SDE Intern", "apply_by": "2026-11-01"}})
            ),
            "records add jobs --apply-by 2026-11-01 --position \"SDE Intern\""
        );
        assert_eq!(
            c("records.list", json!({"collection": "jobs", "filter": {"stage": "offer"}})),
            "records list jobs --stage offer"
        );
        assert_eq!(c("demo.echo", json!({"x": 1})), "demo.echo {\"x\":1}", "no command: op + params");
    }

    #[test]
    fn local_schedules_read_as_they_were_written() {
        let s = |expr: &str| schedule(&json!({"cron_local": expr}));
        assert_eq!(s("0 9 * * 1-5"), "weekdays 09:00 (local)");
        assert_eq!(s("30 18 * * *"), "daily 18:30 (local)");
        assert_eq!(s("5 8 * * 0,6"), "weekends 08:05 (local)");
        assert_eq!(s("0 7 * * 1,3,5"), "mon,wed,fri 07:00 (local)");
        assert_eq!(s("*/15 9 * * 1"), "cron */15 9 * * 1 (local)", "anything else as cron");
    }

    #[test]
    fn show_and_added_read_naturally() {
        let now = Utc::now();
        let data = sample(now);
        let usr1 = &data["triggers"][0];
        assert_eq!(usr1["id"], "usr-1");
        let out = trigger(usr1, now);
        assert!(out.starts_with("usr-1  (active)\n  command   records list leetcode\n  op        records.list\n  params    {\"collection\":\"leetcode\"}\n  schedule  every 2d"), "{out}");
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

    /// A daemon that accepts `scheduler.add` and fails every other request, as one would if
    /// `scheduler.get` broke right after the trigger was created.
    async fn add_only_daemon(sock: &std::path::Path) {
        use shimmer_proto::{decode_client, encode, ClientFrame, ServerFrame};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::UnixListener::bind(sock).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let frame = match decode_client(&line).unwrap() {
                    ClientFrame::Hello { .. } => ServerFrame::welcome("test", "s1"),
                    ClientFrame::Request { id, op, .. } if op == "scheduler.add" => {
                        ServerFrame::ok(id, json!({"trigger_id": "usr-new"}))
                    }
                    ClientFrame::Request { id, op, .. } => {
                        ServerFrame::err(id, shimmer_core::Error::unavailable(format!("{op} broke")))
                    }
                    _ => continue,
                };
                wr.write_all(encode(&frame).unwrap().as_bytes()).await.unwrap();
            }
        });
    }

    #[tokio::test]
    async fn add_still_succeeds_and_says_why_when_its_details_cant_be_fetched() {
        // Wasay's review of #169: the trigger exists either way, so the command succeeds (a
        // script must not retry and add it twice), and says why it shows only the id.
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("s.sock");
        add_only_daemon(&sock).await;
        let mut client = Client::connect(&sock).await.unwrap();
        let cmd = p(&["add", "records.list", "--every", "1d", "--catch-up", "skip"]).unwrap();
        let out = run(&mut client, &cmd, false).await.unwrap();
        assert_eq!(
            out,
            "✓ added trigger usr-new\n  (couldn't fetch its details: unavailable; see 'shimmer scheduler show usr-new')"
        );
    }
}
