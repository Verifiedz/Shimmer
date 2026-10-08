//! `shimmer fetchers …`: sources that reach outward for new items (ADR 0028). Every command is
//! one of the `fetchers` module's own ops; nothing here decides anything (CLAUDE.md §2).

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use shimmer_core::{Error, ErrorCode, Result};

use crate::client::Client;
use crate::flags::Flags;
use crate::render::{self, table};
use crate::when;

pub const USAGE: &str = "usage: shimmer fetchers <command>

commands:
  list                 every known source and its configured state
  status               per-source last-run time and whether it is running
  test SOURCE          fetch one page and show its parsed items; never saves or marks anything seen
  run SOURCE [--wait]  fetch one source now
  items [SOURCE]       items a real run has actually found and kept, most recent first

A source is either one of the built-in ones (hn-whoishiring, weworkremotely) or a configured
kind (ADR 0028 §2a) under [modules.fetchers.sources.<id>] in config.toml. Add --json to any
command for raw output.";

/// `core.events.list`'s own query limit for `items` -- generous enough to cover several runs
/// across every source before filtering down to one, well under the daemon's own cap
/// (`MAX_EVENTS_LIMIT`, `crates/daemon/src/core.rs`).
const ITEMS_QUERY_LIMIT: usize = 200;

#[derive(Clone, Debug, PartialEq)]
pub enum FetchersCmd {
    Help,
    List,
    Status,
    Test { source: String },
    Run { source: String, wait: bool },
    Items { source: Option<String> },
}

/// `words` are what follows `shimmer fetchers`.
pub fn parse(words: Vec<String>) -> std::result::Result<FetchersCmd, String> {
    let mut words = words.into_iter();
    let Some(sub) = words.next() else { return Ok(FetchersCmd::Help) };
    let rest: Vec<String> = words.collect();
    let one = |what: &str| match rest.as_slice() {
        [source] => Ok(source.clone()),
        _ => Err(format!("usage: shimmer fetchers {sub} {what}")),
    };
    match sub.as_str() {
        "help" => Ok(FetchersCmd::Help),
        "list" if rest.is_empty() => Ok(FetchersCmd::List),
        "list" => Err("'fetchers list' takes no arguments".into()),
        "status" if rest.is_empty() => Ok(FetchersCmd::Status),
        "status" => Err("'fetchers status' takes no arguments".into()),
        "test" => Ok(FetchersCmd::Test { source: one("SOURCE")? }),
        "run" => {
            let flags = Flags::split(rest, &[], &["--wait"]).map_err(|e| format!("fetchers run: {e}"))?;
            let source = match flags.positional.as_slice() {
                [s] => s.clone(),
                _ => return Err("usage: shimmer fetchers run SOURCE [--wait]".into()),
            };
            Ok(FetchersCmd::Run { source, wait: flags.has("--wait") })
        }
        "items" => match rest.as_slice() {
            [] => Ok(FetchersCmd::Items { source: None }),
            [source] => Ok(FetchersCmd::Items { source: Some(source.clone()) }),
            _ => Err("usage: shimmer fetchers items [SOURCE]".into()),
        },
        other => Err(format!("unknown fetchers command '{other}'; see 'shimmer fetchers --help'")),
    }
}

pub async fn run(client: &mut Client, cmd: &FetchersCmd, json_out: bool) -> Result<String> {
    match cmd {
        FetchersCmd::Help => Ok(USAGE.into()),
        FetchersCmd::List => {
            let data = client.call("fetchers.list", json!({})).await?;
            Ok(if json_out { render::json(&data) } else { list(&data) })
        }
        FetchersCmd::Status => {
            let data = client.call("fetchers.status", json!({})).await?;
            Ok(if json_out { render::json(&data) } else { status(&data, Utc::now()) })
        }
        FetchersCmd::Test { source } => {
            // Always follows: a dry run that returned immediately with just a task id would
            // defeat its own point, unlike `run`, which has something useful to say either way.
            client.subscribe(&["queue.task.*"]).await?;
            let handle = client.call("fetchers.test", json!({"source": source})).await?;
            let task = task_id(&handle)?;
            let result = follow(client, task).await?;
            Ok(if json_out { render::json(&result) } else { test_items(source, &result) })
        }
        FetchersCmd::Run { source, wait } => {
            if *wait {
                // Before the request, so not even the first event can be missed.
                client.subscribe(&["queue.task.*"]).await?;
            }
            let handle = client.call("fetchers.fetch", json!({"source": source})).await?;
            let task = task_id(&handle)?;
            if !*wait {
                return Ok(if json_out {
                    render::json(&handle)
                } else {
                    format!("queued a fetch of {source} (task {task})\ncheck on it with: shimmer fetchers status")
                });
            }
            let result = follow(client, task).await?;
            Ok(if json_out { render::json(&result) } else { fetched(source, &result) })
        }
        FetchersCmd::Items { source } => {
            // fetchers.item.found is a plain string literal, not an import from shimmer-fetchers
            // -- a client crate must not depend on a module crate (CLAUDE.md §3's dependency
            // rules), same reason every other op name in this file is also a literal.
            let data = client
                .call("core.events.list", json!({"topic": "fetchers.item.found", "limit": ITEMS_QUERY_LIMIT}))
                .await?;
            let events = data["events"].as_array().cloned().unwrap_or_default();
            let matching: Vec<Value> = events
                .into_iter()
                .filter(|e| match source {
                    None => true,
                    Some(s) => e["payload"]["source_name"].as_str() == Some(s.as_str()),
                })
                .collect();
            Ok(if json_out { render::json(&json!({"items": matching})) } else { items(source.as_deref(), &matching) })
        }
    }
}

fn task_id(handle: &Value) -> Result<&str> {
    handle["task_id"].as_str().ok_or_else(|| Error::internal("no task_id in the queued response"))
}

fn fetched(source: &str, result: &Value) -> String {
    let n = result["new_items"].as_u64().unwrap_or(0);
    format!("✓ fetched {source}: {n} new item{}", if n == 1 { "" } else { "s" })
}

/// Follow a queued task over the subscription stream until it ends, the same pattern
/// `workspaces::queued`'s `follow` uses for a launch -- `queue.task.finished`'s own event
/// payload carries the handler's full return value, so this never needs the task's
/// persisted record, which keeps no result at all for a succeeded task (only its error, for
/// a failed one; see `finished_task`'s own note on the one path where that matters).
async fn follow(client: &mut Client, task: &str) -> Result<Value> {
    loop {
        let ev = client.next_event().await?;
        if ev.topic == "core.stream.lagged" {
            // Events were dropped, maybe the one we wait for: ask directly instead.
            if let Some(outcome) = finished_task(client, task).await? {
                return outcome;
            }
            continue;
        }
        if ev.payload["task_id"] != task {
            continue;
        }
        match ev.topic.as_str() {
            "queue.task.finished" => return Ok(ev.payload["result"].clone()),
            "queue.task.failed" => return Err(task_error(&ev.payload)),
            "queue.task.cancelled" => return Err(Error::new(ErrorCode::ModuleError, "the task was cancelled")),
            _ => {}
        }
    }
}

/// After `core.stream.lagged`: has the task ended? `None` while it is still queued or
/// running. `queue.task`'s response has no `result` field for a succeeded task -- only
/// `error` for a failed one (`crates/daemon/src/queue/mod.rs`'s `view`) -- so a succeeded
/// outcome here is reported as a (rare) miss, honestly, rather than as a silent empty result.
async fn finished_task(client: &mut Client, task: &str) -> Result<Option<Result<Value>>> {
    let t = client.call("queue.task", json!({ "task_id": task })).await?;
    Ok(match t["status"].as_str() {
        Some("succeeded") => Some(Err(Error::new(
            ErrorCode::Unavailable,
            "the fetch finished, but its result was missed (events were dropped); see 'shimmer fetchers status'",
        ))),
        Some("failed") => {
            Some(Err(Error::new(ErrorCode::ModuleError, t["error"].as_str().unwrap_or("the task failed").to_owned())))
        }
        Some("cancelled") => Some(Err(Error::new(ErrorCode::ModuleError, "the task was cancelled"))),
        _ => None,
    })
}

fn task_error(payload: &Value) -> Error {
    let code: ErrorCode = serde_json::from_value(payload["code"].clone()).unwrap_or(ErrorCode::ModuleError);
    Error::new(code, payload["error"].as_str().unwrap_or("the task failed").to_owned())
}

/// One table of every known source and its configured state, or a line saying there are none.
fn list(data: &Value) -> String {
    let sources = data["sources"].as_array().cloned().unwrap_or_default();
    if sources.is_empty() {
        return "no sources known".into();
    }
    let rows: Vec<Vec<String>> = sources
        .iter()
        .map(|s| {
            vec![
                render::cell(&s["id"]),
                render::cell(&s["method"]),
                s["kind"].as_str().unwrap_or("-").to_string(),
                if s["enabled"].as_bool().unwrap_or(false) { "yes".into() } else { "no".into() },
                s["interval_s"].as_u64().map(when::format_duration).unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    let header = ["ID", "METHOD", "KIND", "ENABLED", "INTERVAL"].map(String::from);
    let n = sources.len();
    format!("{}\n{n} source{}", table(&header, &rows), if n == 1 { "" } else { "s" })
}

/// One table of every known source's last-run time and whether it is currently running.
fn status(data: &Value, now: DateTime<Utc>) -> String {
    let sources = data["sources"].as_array().cloned().unwrap_or_default();
    if sources.is_empty() {
        return "no sources known".into();
    }
    let rows: Vec<Vec<String>> = sources
        .iter()
        .map(|s| {
            let last_run = when::timestamp(&s["last_run_at"]);
            vec![
                render::cell(&s["id"]),
                last_run.map_or("never".into(), |t| when::relative(t, now)),
                if s["running"].as_bool().unwrap_or(false) { "yes".into() } else { "no".into() },
            ]
        })
        .collect();
    table(&["ID", "LAST RUN", "RUNNING"].map(String::from), &rows)
}

/// Every parsed item `fetchers.test` found, title and url, or a line saying it found none.
fn test_items(source: &str, result: &Value) -> String {
    let items = result["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return format!("{source}: no items parsed");
    }
    let mut out = format!("{source}: {} item{}\n", items.len(), if items.len() == 1 { "" } else { "s" });
    for item in &items {
        // The url must survive copy/paste intact -- render::cell's truncation is fine for a
        // title (a preview), but a truncated url is a different, invalid url, not a shorter
        // version of the real one, and silently hands back a 404 instead of the job posting.
        let url = item["url"].as_str().unwrap_or_default();
        let _ = writeln!(out, "  {}  {url}", render::cell(&item["title"]));
    }
    out.trim_end().to_string()
}

/// Every `fetchers.item.found` event matched, most recent first -- `core.events.list` itself
/// returns oldest-first (the event-log's own convention), reversed here only for display,
/// same reasoning as a feed or a log tail: what just happened belongs on top.
fn items(source: Option<&str>, events: &[Value]) -> String {
    if events.is_empty() {
        return match source {
            Some(s) => format!("{s}: no items found yet"),
            None => "no items found yet".into(),
        };
    }
    let n = events.len();
    let mut out = match source {
        Some(s) => format!("{s}: {n} item{}\n", if n == 1 { "" } else { "s" }),
        None => format!("{n} item{}\n", if n == 1 { "" } else { "s" }),
    };
    for ev in events.iter().rev() {
        let p = &ev["payload"];
        // Never truncated, same reasoning as test_items's own url handling.
        let url = p["url"].as_str().unwrap_or_default();
        let _ = match source {
            Some(_) => writeln!(out, "  {}  {url}", render::cell(&p["title"])),
            None => {
                writeln!(out, "  [{}] {}  {url}", p["source_name"].as_str().unwrap_or("?"), render::cell(&p["title"]))
            }
        };
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &[&str]) -> std::result::Result<FetchersCmd, String> {
        parse(s.iter().map(|w| w.to_string()).collect())
    }

    #[test]
    fn commands_parse() {
        assert_eq!(p(&[]), Ok(FetchersCmd::Help));
        assert_eq!(p(&["list"]), Ok(FetchersCmd::List));
        assert_eq!(p(&["status"]), Ok(FetchersCmd::Status));
        assert_eq!(p(&["test", "hn-whoishiring"]), Ok(FetchersCmd::Test { source: "hn-whoishiring".into() }));
        assert_eq!(
            p(&["run", "hn-whoishiring"]),
            Ok(FetchersCmd::Run { source: "hn-whoishiring".into(), wait: false })
        );
        assert_eq!(
            p(&["run", "hn-whoishiring", "--wait"]),
            Ok(FetchersCmd::Run { source: "hn-whoishiring".into(), wait: true })
        );
        assert_eq!(p(&["items"]), Ok(FetchersCmd::Items { source: None }));
        assert_eq!(p(&["items", "jobbank-canada"]), Ok(FetchersCmd::Items { source: Some("jobbank-canada".into()) }));
    }

    #[test]
    fn mistakes_are_explained() {
        assert!(p(&["list", "x"]).unwrap_err().contains("takes no arguments"));
        assert!(p(&["status", "x"]).unwrap_err().contains("takes no arguments"));
        assert!(p(&["test"]).unwrap_err().contains("usage: shimmer fetchers test SOURCE"));
        assert!(p(&["run"]).unwrap_err().contains("usage: shimmer fetchers run SOURCE"));
        assert!(p(&["run", "a", "--bogus"]).unwrap_err().contains("unknown option '--bogus'"));
        assert!(p(&["items", "a", "b"]).unwrap_err().contains("usage: shimmer fetchers items"));
        assert!(p(&["frob"]).unwrap_err().contains("unknown fetchers command 'frob'"));
    }

    #[test]
    fn list_renders_a_table_with_a_count() {
        let data = json!({"sources": [
            {"id": "hn-whoishiring", "method": "api", "kind": null, "enabled": true, "interval_s": 86_400},
            {"id": "my-team-blog", "method": "api", "kind": "rss", "enabled": false, "interval_s": null},
        ]});
        let out = list(&data);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0].split_whitespace().collect::<Vec<_>>(), ["ID", "METHOD", "KIND", "ENABLED", "INTERVAL"]);
        assert!(lines[1].contains("hn-whoishiring") && lines[1].contains("yes") && lines[1].contains("1d"), "{out}");
        assert!(lines[2].contains("my-team-blog") && lines[2].contains("rss") && lines[2].contains("no"), "{out}");
        assert_eq!(lines[3], "2 sources");
    }

    #[test]
    fn list_with_nothing_known_says_so() {
        assert_eq!(list(&json!({"sources": []})), "no sources known");
    }

    #[test]
    fn status_renders_last_run_and_running() {
        let now = Utc::now();
        let data = json!({"sources": [
            {"id": "hn-whoishiring", "last_run_at": (now - chrono::Duration::seconds(90)).to_rfc3339(), "running": false},
            {"id": "weworkremotely", "last_run_at": null, "running": true},
        ]});
        let out = status(&data, now);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("hn-whoishiring") && lines[1].contains("ago") && lines[1].ends_with("no"), "{out}");
        assert!(
            lines[2].contains("weworkremotely") && lines[2].contains("never") && lines[2].ends_with("yes"),
            "{out}"
        );
    }

    #[test]
    fn test_items_lists_title_and_url_or_says_there_were_none() {
        let data = json!({"source": "hn-whoishiring", "items": [{"title": "Acme is hiring", "url": "https://x/1"}]});
        let out = test_items("hn-whoishiring", &data);
        assert_eq!(out, "hn-whoishiring: 1 item\n  Acme is hiring  https://x/1");
        assert_eq!(test_items("hn-whoishiring", &json!({"items": []})), "hn-whoishiring: no items parsed");
    }

    #[test]
    fn test_items_never_truncates_the_url_even_when_the_title_does() {
        // A truncated title is still a readable preview; a truncated url is a different,
        // invalid url that 404s if anyone follows it -- the two must not be treated the same.
        let long_title = "x".repeat(80);
        let long_url = format!("https://www.jobbank.gc.ca/jobsearch/jobposting/{}", "1".repeat(80));
        let data = json!({"items": [{"title": long_title, "url": long_url}]});
        let out = test_items("jobbank-canada", &data);
        assert!(out.ends_with(&long_url), "the full url must survive untouched:\n{out}");
        assert!(out.contains('…'), "the long title should still be shortened for display");
    }

    fn found(source: &str, title: &str, url: &str) -> Value {
        json!({"topic": "fetchers.item.found", "payload": {"source_name": source, "title": title, "url": url}})
    }

    #[test]
    fn items_groups_by_source_when_none_is_given_and_shows_newest_first() {
        let events = vec![
            found("hn-whoishiring", "Acme hiring", "https://x/1"),
            found("weworkremotely", "Rust role", "https://x/2"),
        ];
        let out = items(None, &events);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "2 items");
        // events arrive oldest-first; display must flip to newest-first.
        assert!(lines[1].contains("weworkremotely") && lines[1].contains("Rust role"), "{out}");
        assert!(lines[2].contains("hn-whoishiring") && lines[2].contains("Acme hiring"), "{out}");
    }

    #[test]
    fn items_for_one_source_omits_the_source_label() {
        let events = vec![found("jobbank-canada", "Software Engineer", "https://x/1")];
        let out = items(Some("jobbank-canada"), &events);
        assert_eq!(out, "jobbank-canada: 1 item\n  Software Engineer  https://x/1");
    }

    #[test]
    fn items_with_nothing_found_says_so_either_way() {
        assert_eq!(items(None, &[]), "no items found yet");
        assert_eq!(items(Some("jobbank-canada"), &[]), "jobbank-canada: no items found yet");
    }

    #[test]
    fn fetched_pluralises_new_items() {
        assert_eq!(fetched("hn-whoishiring", &json!({"new_items": 0})), "✓ fetched hn-whoishiring: 0 new items");
        assert_eq!(fetched("hn-whoishiring", &json!({"new_items": 1})), "✓ fetched hn-whoishiring: 1 new item");
    }
}
