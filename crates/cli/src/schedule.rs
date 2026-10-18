//! `shimmer scheduler add WHEN [options] COMMAND…` (ADR 0031 §1, §6): the schedule in words, then the
//! command exactly as you'd run it. The command becomes the one op + params that running it
//! sends, using the same parsers, and is stored with `scheduler.add` as before. Nothing is
//! reserved on any command: the scheduling words all come before it.

use serde_json::{json, Value};
use shimmer_core::{Error, Result};

use crate::client::Client;
use crate::packs::pack::Pack;
use crate::workspaces::{Prompt, WorkspacesCmd};
use crate::{records, render, when, workspaces};

pub const USAGE: &str = "usage: shimmer scheduler add WHEN [--catch-up WHAT] [--lane LANE] COMMAND…

Runs COMMAND on a schedule instead of now: the command exactly as you'd type it.
  shimmer scheduler add weekdays 09:00 --catch-up skip workspaces activate site
  shimmer scheduler add daily 18:00 --catch-up skip workspaces stop site
  shimmer scheduler add every 1d --catch-up run-once records complete habits/stretch
  shimmer scheduler add once 2026-10-20 18:00 --catch-up skip workspaces stop site

WHEN is one of:
  daily HH:MM, weekdays HH:MM, weekends HH:MM, mon,wed,fri HH:MM
                     your local time; 9am stays 9am when the clocks change
  every DURATION     e.g. 30m, 2h, 1d, 1w
  once DATE TIME     e.g. 2026-10-20 18:00 (local time), or 2026-10-20T18:00:00Z
  cron \"EXPR\"        five fields, minute hour day month weekday, in UTC
  cron-local \"EXPR\"  the same, in your local time

--catch-up says what happens to runs missed while the computer was off or asleep: skip,
run-once (once on waking) or backfill (once for each missed time). Leave it out at a terminal
to be asked.

What can be scheduled: a records or workspaces command that makes one change, e.g.
records add/update/complete/reopen/remove/restore, workspaces activate/stop/cleanup, and
notify send (a reminder: shimmer scheduler add weekdays 08:45 --catch-up skip notify send \"Standup in 15\"). An
alias from your command pack works too. See schedules with 'shimmer scheduler list'. Any other op:
shimmer call scheduler.add '{...}' (see docs/protocol.md).";

#[derive(Clone, Debug, PartialEq)]
pub enum ScheduleCmd {
    Help,
    Add(Planned),
}

/// A schedule as typed, before the command is turned into an op.
#[derive(Clone, Debug, PartialEq)]
pub struct Planned {
    pub when: When,
    /// `skip`, `run_once` or `backfill`, as the protocol spells them; `None` to ask.
    pub catch_up: Option<&'static str>,
    pub lane: Option<String>,
    /// The command, from its first word: `["workspaces", "activate", "site"]`.
    pub command: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct When {
    /// As `scheduler.add` takes it: `{"cron_local": "0 9 * * 1-5"}`.
    pub schedule: Value,
    /// As people read it: `weekdays 09:00 (local)`.
    pub words: String,
}

/// The words that start a command: the entries of `shimmer --help`'s "commands:" section, read
/// from the help text itself so a new command is known here with no second list to keep (#185's
/// review). An alias of the active pack starts one too, rewritten before this is asked.
fn is_command_word(word: &str) -> bool {
    let commands = crate::args::USAGE.split("commands:\n").nth(1).unwrap_or_default();
    word == "help"
        || commands
            .split("\n\n")
            .next()
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
            .filter_map(|l| l.split_whitespace().next())
            .any(|w| w == word)
}

/// What can be scheduled: the commands that make exactly one change (ADR 0031 §1). Running one
/// checks it in [`request`]; completion offers only these after `shimmer scheduler add WHEN`.
pub const SCHEDULABLE: [(&str, &[&str]); 3] = [
    ("records", &["add", "update", "complete", "reopen", "remove", "restore"]),
    ("workspaces", &["activate", "stop", "cleanup"]),
    ("notify", &["send"]),
];

/// `shimmer`'s own options that go before the command (`--pack`, `--canonical`, `--no-color`):
/// put after `scheduler add`, they're not the schedule's.
const FRONT_FLAGS: &[&str] = &["--pack", "--canonical", "--no-color"];

const EXAMPLES: &str = "e.g. 'weekdays 09:00', 'daily 18:00', 'every 2h' or 'once 2026-10-20 18:00'";

pub fn parse(words: Vec<String>) -> std::result::Result<ScheduleCmd, String> {
    if words.is_empty() || matches!(words[0].as_str(), "help" | "--help" | "-h") {
        return Ok(ScheduleCmd::Help);
    }
    // The raw `OP [PARAMS] --every …` form went (ADR 0031 §6): an op name where the time goes
    // says what to type instead.
    let raw_form =
        words[0].contains('.') || matches!(words[0].as_str(), "--every" | "--cron" | "--cron-local" | "--once");
    if raw_form && parse_when(&words).is_err() {
        return Err(format!(
            "'{}' is the old way of scheduling: say when first, then the command as you'd type it, e.g. scheduler add \
             weekdays 09:00 --catch-up skip workspaces activate site. For any other op: shimmer call \
             scheduler.add '{{\"schedule\":…,\"op\":…,\"params\":…,\"catch_up\":…}}'",
            words[0]
        ));
    }
    let (when, used) = parse_when(&words)?;
    let (mut catch_up, mut lane) = (None, None);
    let mut i = used;
    while let Some(word) = words.get(i) {
        let (flag, inline) = match word.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (word.as_str(), None),
        };
        if !flag.starts_with('-') {
            break;
        }
        let mut value = || -> std::result::Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            i += 1;
            words.get(i).cloned().ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--catch-up" => catch_up = Some(catch_up_word(&value()?)?),
            "--lane" => lane = Some(value()?),
            other if FRONT_FLAGS.contains(&other) => {
                return Err(format!(
                    "'{other}' is one of shimmer's own options: put it before 'scheduler add', e.g. shimmer {other} … scheduler add …"
                ))
            }
            other => {
                return Err(format!("unknown option '{other}' before the command; options for the command go after it"))
            }
        }
        i += 1;
    }
    let command: Vec<String> = words[i..].to_vec();
    match command.first() {
        None => {
            Err(format!("nothing to schedule after '{}': add the command, e.g. workspaces activate site", when.words))
        }
        Some(w) if !is_command_word(w) => Err(format!(
            "'{w}' isn't a command: after when ({}) comes the command, e.g. workspaces activate site",
            when.words
        )),
        Some(_) => Ok(ScheduleCmd::Add(Planned { when, catch_up, lane, command })),
    }
}

/// The one op + params a schedulable workspaces command sends: activate, stop or cleanup, without
/// `--wait` or `--first`. Shared with `scheduler list`, which reads a schedule back through it (ADR 0031 §3).
pub fn workspaces_op(cmd: &WorkspacesCmd) -> Option<(&'static str, Value)> {
    match cmd {
        WorkspacesCmd::Activate { id, wait: false, first: None } => Some(("workspaces.activate", json!({"id": id}))),
        WorkspacesCmd::Stop { id, wait: false, first: None } => Some(("workspaces.stop", json!({"id": id}))),
        WorkspacesCmd::Cleanup { id, wait: false, first: None } => Some(("workspaces.cleanup", json!({"id": id}))),
        _ => None,
    }
}

/// `op` for workspace `id`, once the daemon has it: a typo is caught when it's scheduled, not each
/// time it fires (#185's review). `workspaces.show` is the read-only lookup `status` makes.
async fn known_workspace(client: &mut Client, op: &'static str, id: &str) -> Result<()> {
    client.call("workspaces.show", json!({"id": id})).await.map(|_| ()).map_err(|e| match e.code {
        shimmer_core::ErrorCode::NotFound => Error::not_found(format!("no workspace '{id}' to schedule for ({op})")),
        _ => e,
    })
}

/// `skip`, `run-once` or `backfill` → the protocol's spelling.
fn catch_up_word(word: &str) -> std::result::Result<&'static str, String> {
    match word {
        "skip" => Ok("skip"),
        "run-once" | "run_once" => Ok("run_once"),
        "backfill" => Ok("backfill"),
        other => Err(format!("--catch-up is skip, run-once or backfill, not '{other}'")),
    }
}

/// `WHEN` from the front of `words`, and how many words it took.
pub(crate) fn parse_when(words: &[String]) -> std::result::Result<(When, usize), String> {
    let at = |i: usize| words.get(i).map(String::as_str);
    let first = at(0).unwrap_or_default();
    match first {
        "every" => {
            let d = at(1).ok_or_else(|| format!("'every' needs a duration, {EXAMPLES}"))?;
            let secs = when::parse_duration(d)?;
            Ok((When { schedule: json!({"every": secs}), words: format!("every {}", when::format_duration(secs)) }, 2))
        }
        "once" => {
            let date = at(1).ok_or_else(|| "'once' needs a date and time, e.g. once 2026-10-20 18:00".to_owned())?;
            let (text, used) = match at(2) {
                Some(time) if !date.contains('T') && time.contains(':') => (format!("{date} {time}"), 3),
                _ => (date.to_owned(), 2),
            };
            let t = when::parse_once(&text)?;
            Ok((When { schedule: json!({"once": t.to_rfc3339()}), words: format!("once {}", when::local(t)) }, used))
        }
        "cron" | "cron-local" => {
            let expr = at(1)
                .ok_or_else(|| format!("'{first}' needs an expression in quotes, e.g. {first} \"0 9 * * 1-5\""))?;
            if expr.split_whitespace().count() != 5 {
                return Err(format!(
                    "'{expr}' isn't five cron fields (minute hour day month weekday): put it in quotes"
                ));
            }
            let (key, zone) = if first == "cron" { ("cron", "UTC") } else { ("cron_local", "local") };
            Ok((When { schedule: json!({ key: expr }), words: format!("cron {expr} ({zone})") }, 2))
        }
        days => {
            let weekday = days_field(days)
                .ok_or_else(|| format!("'{days}' isn't a time to schedule at: say when first, {EXAMPLES}"))?;
            let time = at(1).ok_or_else(|| format!("'{days}' needs a time, e.g. {days} 09:00"))?;
            let (hour, minute) = clock(time)?;
            let schedule = json!({"cron_local": format!("{minute} {hour} * * {weekday}")});
            Ok((When { schedule, words: format!("{days} {hour:02}:{minute:02} (local)") }, 2))
        }
    }
}

/// `daily`, `weekdays`, `weekends`, or a list like `mon,wed,fri` → cron's weekday field.
fn days_field(days: &str) -> Option<String> {
    const NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    match days {
        "daily" => Some("*".into()),
        "weekdays" => Some("1-5".into()),
        "weekends" => Some("0,6".into()),
        list => list
            .split(',')
            .map(|d| NAMES.iter().position(|n| *n == d).map(|i| i.to_string()))
            .collect::<Option<Vec<_>>>()
            .map(|v| v.join(",")),
    }
}

/// `HH:MM` (24-hour) → (hour, minute).
fn clock(text: &str) -> std::result::Result<(u32, u32), String> {
    let bad = || format!("'{text}' isn't a time of day: use HH:MM, e.g. 09:00 or 18:30");
    let (h, m) = text.split_once(':').ok_or_else(bad)?;
    let (h, m): (u32, u32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 || text.len() > 5 {
        return Err(bad());
    }
    Ok((h, m))
}

/// Where the command starts in `words` (what follows `shimmer scheduler add`), by position (ADR 0031
/// §1): after `WHEN`, which has a fixed shape, and the schedule's `--option value` pairs. So an
/// option's value is never taken for the command (`--lane workspaces workspaces activate site`),
/// and no word needs reserving: `daily` may be a pack's alias too. For rewriting an alias there,
/// as the CLI does for the first word of a line.
fn command_start(words: &[String]) -> Option<usize> {
    let (_, mut i) = parse_when(words).ok()?;
    while let Some(word) = words.get(i) {
        if !word.starts_with("--") {
            return Some(i);
        }
        // `--catch-up X` / `--lane X` take the next word; `--flag=X` doesn't.
        i += if word.contains('=') { 1 } else { 2 };
    }
    None
}

/// `shimmer scheduler add WHEN … ALIAS rest…` → the alias replaced by the words it stands for, so
/// what's stored is canonical (CLAUDE.md §12 rule 16). `args` is the whole line (a pack's alias for
/// `scheduler add` already written out); anything that isn't a `scheduler add` line comes back
/// unchanged.
pub fn rewrite_inner(mut args: Vec<String>, pack: Option<&Pack>) -> Vec<String> {
    let (Some(pack), Some(i)) = (pack, crate::packs::active::command_word(&args)) else { return args };
    if args[i] != "scheduler" || args.get(i + 1).map(String::as_str) != Some("add") {
        return args;
    }
    let rest = &args[i + 2..];
    if let Some(start) = command_start(rest) {
        if let Some(target) = pack.aliases.get(&rest[start]) {
            let at = i + 2 + start;
            args.splice(at..=at, target.words.iter().map(|w| w.to_string()));
        }
    }
    args
}

pub async fn run(client: &mut Client, cmd: &ScheduleCmd, json: bool, prompt: &mut dyn Prompt) -> Result<String> {
    let ScheduleCmd::Add(plan) = cmd else { return Ok(USAGE.into()) };
    let (op, params) = request(client, &plan.command).await?;
    let catch_up = match plan.catch_up {
        Some(c) => c,
        None => ask_catch_up(prompt, &plan.when)?,
    };
    let mut add = json!({"schedule": plan.when.schedule, "op": op, "params": params, "catch_up": catch_up});
    if let Some(lane) = &plan.lane {
        add["lane"] = json!(lane);
    }
    let data = client.call("scheduler.add", add).await?;
    if json {
        return Ok(render::json(&data));
    }
    let id = data["trigger_id"].as_str().unwrap_or("?").to_owned();
    let short = render::short_ids(std::slice::from_ref(&id)).remove(0);
    let first = match client.call("scheduler.show", json!({"trigger_id": id})).await {
        Ok(t) => when::timestamp(&t["next_due"]).map_or("never (already past)".into(), |at| {
            format!("{} ({})", when::relative(at, chrono::Utc::now()), when::local(at))
        }),
        Err(_) => "see 'shimmer scheduler show'".into(),
    };
    Ok(format!(
        "✓ scheduled {} · {} · catch-up {}\n  first run: {first}\n  id {short} (shimmer scheduler show {short})",
        typed(&plan.command),
        plan.when.words,
        catch_up.replace('_', "-"),
    ))
}

/// `scheduler edit TRIGGER …` (ADR 0038 §6): the same checks as `add`, sent as `scheduler.edit`
/// for the trigger's full id, so it keeps its id, whether it's on, and when it last ran.
pub async fn edit(
    client: &mut Client,
    id: &str,
    plan: &Planned,
    json: bool,
    prompt: &mut dyn Prompt,
) -> Result<String> {
    let current = client.call("scheduler.show", json!({"trigger_id": id})).await?;
    let full = current["id"].as_str().unwrap_or(id).to_owned();
    let (op, params) = request(client, &plan.command).await?;
    let catch_up = match plan.catch_up {
        Some(c) => c,
        None => ask_catch_up(prompt, &plan.when)?,
    };
    let mut edit = json!({"trigger_id": full, "schedule": plan.when.schedule, "op": op, "params": params,
                          "catch_up": catch_up});
    if let Some(lane) = &plan.lane {
        edit["lane"] = json!(lane);
    }
    let data = client.call("scheduler.edit", edit).await?;
    if json {
        return Ok(render::json(&data));
    }
    let short = render::short_ids(std::slice::from_ref(&full)).remove(0);
    let next = when::timestamp(&data["next_due"]).map_or("never (already past)".into(), |at| {
        format!("{} ({})", when::relative(at, chrono::Utc::now()), when::local(at))
    });
    let off = if data["enabled"] == false { " (off: shimmer scheduler enable it to run)" } else { "" };
    Ok(format!(
        "✓ {short} now runs {} · {} · catch-up {}{off}\n  next run: {next}",
        typed(&plan.command),
        plan.when.words,
        catch_up.replace('_', "-"),
    ))
}

/// The one op + params running `command` would send. Only commands that make exactly one
/// change can be scheduled (ADR 0031 §1).
async fn request(client: &mut Client, command: &[String]) -> Result<(&'static str, Value)> {
    let (word, rest) = command.split_first().ok_or_else(|| Error::invalid_params("nothing to schedule"))?;
    let sub = rest.first().map(String::as_str).unwrap_or_default();
    let not = |why: &str| Error::invalid_params(format!("'{word} {sub}' can't be scheduled: {why}"));
    match word.as_str() {
        "records" => {
            if !SCHEDULABLE[0].1.contains(&sub) {
                return Err(not(
                    "only records add, update, complete, reopen, remove and restore can (one change each, nothing to read)",
                ));
            }
            let cmd = records::parse(rest.to_vec()).map_err(Error::invalid_params)?;
            let schema = match cmd.collection() {
                Some(id) => records::schema(client, id).await?,
                None => Value::Null,
            };
            // A typo is caught now, not every day after (#185's review): the same lookup running
            // the command does.
            if let (Some(id), true) = (cmd.collection(), schema.is_null()) {
                return Err(Error::not_found(format!(
                    "no collection '{id}' to schedule for (see 'shimmer records collections')"
                )));
            }
            records::request(&cmd, &schema)
        }
        "workspaces" => match workspaces::parse(rest.to_vec()).map_err(Error::invalid_params)? {
            WorkspacesCmd::Activate { wait: true, .. }
            | WorkspacesCmd::Stop { wait: true, .. }
            | WorkspacesCmd::Cleanup { wait: true, .. } => {
                Err(not("--wait follows a launch as it happens, and a scheduled one runs with no one waiting"))
            }
            // A trigger can't carry a request's `queue` block (#222): a scheduled run is the
            // scheduler's own task and takes its turn, so `--first` would be silently lost.
            WorkspacesCmd::Activate { first: Some(_), .. }
            | WorkspacesCmd::Stop { first: Some(_), .. }
            | WorkspacesCmd::Cleanup { first: Some(_), .. } => Err(not(
                "--first jumps the queue as you run it; a scheduled run is the scheduler's own and takes its turn",
            )),
            cmd => match workspaces_op(&cmd) {
                Some((op, params)) => {
                    known_workspace(client, op, params["id"].as_str().unwrap_or_default()).await?;
                    Ok((op, params))
                }
                None => Err(not("only workspaces activate, stop and cleanup can")),
            },
        },
        "notify" => crate::notify::parse(rest.to_vec())
            .ok()
            .as_ref()
            .and_then(crate::notify::scheduled_send)
            .ok_or_else(|| not("only notify send can (one message each time)")),
        other => Err(Error::invalid_params(format!(
            "'{other}' can't be scheduled: only records and workspaces commands that make one change, and notify send, can"
        ))),
    }
}

/// `--catch-up` left out: ask, in plain words, when someone is there; never guess (§6.3).
fn ask_catch_up(prompt: &mut dyn Prompt, when: &When) -> Result<&'static str> {
    let missing = || {
        Error::invalid_params(
            "say what happens to runs missed while the computer is off or asleep: \
             --catch-up skip, run-once or backfill",
        )
    };
    if !prompt.interactive() {
        return Err(missing());
    }
    let question = format!(
        "If your computer is off or asleep when it's due ({}):\n  1) skip that time\n  2) run it once when it wakes\n  \
         3) run every time it missed\n>",
        when.words
    );
    match prompt.ask(&question).as_deref().map(str::trim) {
        Some("1" | "skip") => Ok("skip"),
        Some("2" | "run-once") => Ok("run_once"),
        Some("3" | "backfill") => Ok("backfill"),
        _ => Err(missing()),
    }
}

/// The command as the person typed it, quoted where a shell needs it (`records add notes --title
/// 'Two Sum'`), as `scheduler list` shows it.
fn typed(command: &[String]) -> String {
    command.iter().map(|w| crate::scheduler::shell_word(w)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    fn plan(words: &[&str]) -> Planned {
        match parse(w(words)).unwrap() {
            ScheduleCmd::Add(p) => p,
            ScheduleCmd::Help => panic!("help"),
        }
    }

    #[test]
    fn the_reply_quotes_the_command_as_typed() {
        assert_eq!(
            typed(&w(&["records", "add", "notes", "--title", "Two Sum"])),
            "records add notes --title 'Two Sum'"
        );
        assert_eq!(typed(&w(&["workspaces", "stop", "site"])), "workspaces stop site");
    }

    #[test]
    fn days_and_a_time_become_local_cron() {
        let p = plan(&["weekdays", "09:00", "--catch-up", "skip", "workspaces", "activate", "site"]);
        assert_eq!(p.when.schedule, json!({"cron_local": "0 9 * * 1-5"}));
        assert_eq!(p.when.words, "weekdays 09:00 (local)");
        assert_eq!((p.catch_up, p.command.clone()), (Some("skip"), w(&["workspaces", "activate", "site"])));
        assert_eq!(
            plan(&["daily", "18:30", "workspaces", "stop", "site"]).when.schedule,
            json!({"cron_local": "30 18 * * *"})
        );
        assert_eq!(
            plan(&["weekends", "8:05", "records", "add", "x"]).when.schedule,
            json!({"cron_local": "5 8 * * 0,6"})
        );
        assert_eq!(
            plan(&["mon,wed,fri", "07:00", "records", "add", "x"]).when.schedule,
            json!({"cron_local": "0 7 * * 1,3,5"})
        );
    }

    #[test]
    fn every_once_and_cron() {
        assert_eq!(plan(&["every", "2h", "records", "add", "x"]).when.schedule, json!({"every": 7200}));
        let once = plan(&["once", "2026-10-20T18:00:00Z", "--catch-up=run-once", "workspaces", "stop", "s"]);
        assert_eq!(
            (once.when.schedule["once"].as_str().map(|s| &s[..16]), once.catch_up),
            (Some("2026-10-20T18:00"), Some("run_once"))
        );
        assert!(plan(&["once", "2026-10-20", "18:00", "workspaces", "stop", "s"]).when.schedule["once"].is_string());
        assert_eq!(plan(&["cron", "0 9 * * 1", "records", "add", "x"]).when.schedule, json!({"cron": "0 9 * * 1"}));
        assert_eq!(plan(&["cron-local", "0 9 * * 1", "records", "add", "x"]).when.words, "cron 0 9 * * 1 (local)");
    }

    #[test]
    fn the_command_keeps_its_own_options_and_nothing_is_reserved() {
        // `--at` here is the command's field, not a scheduling word: it comes after the command starts.
        let p = plan(&["daily", "09:00", "--lane", "workspaces", "records", "add", "fairs", "--at", "Hall B"]);
        assert_eq!(p.lane.as_deref(), Some("workspaces"));
        assert_eq!(p.command, w(&["records", "add", "fairs", "--at", "Hall B"]));
        assert_eq!(p.catch_up, None, "asked later");
    }

    #[test]
    fn command_words_come_from_the_help() {
        // Every entry in `shimmer --help`'s commands, with no second list to keep (#185's review).
        for word in ["records", "workspaces", "queue", "scheduler", "packs", "ping", "call", "shutdown"] {
            assert!(is_command_word(word), "{word}");
        }
        assert!(is_command_word("help"));
        assert!(!is_command_word("activate") && !is_command_word("--socket"));
    }

    #[test]
    fn shimmers_own_options_are_sent_before_schedule() {
        let e = parse(w(&["weekdays", "09:00", "--catch-up", "skip", "--pack", "short", "wgo", "site"])).unwrap_err();
        assert!(e.contains("'--pack' is one of shimmer's own options: put it before 'scheduler add'"), "{e}");
    }

    #[test]
    fn mistakes_say_what_was_expected() {
        let e = |words: &[&str]| parse(w(words)).unwrap_err();
        assert!(e(&["tomorrow", "workspaces"]).contains("say when first"));
        assert!(e(&["weekdays", "9am", "x"]).contains("HH:MM"));
        assert!(e(&["weekdays", "25:00"]).contains("HH:MM"));
        assert!(e(&["weekdays", "09:00"]).contains("nothing to schedule"));
        assert!(e(&["weekdays", "09:00", "activate", "site"]).contains("isn't a command"));
        assert!(e(&["daily", "09:00", "--catch-up", "sometimes", "records", "add", "x"])
            .contains("skip, run-once or backfill"));
        assert!(e(&["daily", "09:00", "--wait", "records", "add", "x"]).contains("options for the command go after it"));
        assert!(e(&["cron", "0 9 * *", "records", "add"]).contains("five cron fields"));
        assert_eq!(parse(w(&[])).unwrap(), ScheduleCmd::Help);
    }

    #[test]
    fn an_alias_where_the_command_starts_is_turned_into_the_command() {
        let pack = crate::packs::builtin::builtin("short").unwrap();
        let (alias, target) = pack.aliases.iter().find(|(_, t)| t.op == "workspaces.activate").unwrap();
        let line = w(&["scheduler", "add", "weekdays", "09:00", "--catch-up", "skip", alias, "site"]);
        let out = rewrite_inner(line, Some(&pack));
        let mut want = w(&["scheduler", "add", "weekdays", "09:00", "--catch-up", "skip"]);
        want.extend(target.words.iter().map(|s| s.to_string()));
        want.push("site".into());
        assert_eq!(out, want);
        // Not a schedule line, or no pack: unchanged.
        assert_eq!(rewrite_inner(w(&["records", "list", "x"]), Some(&pack)), w(&["records", "list", "x"]));
        assert_eq!(
            rewrite_inner(w(&["scheduler", "add", "daily", "09:00", alias]), None),
            w(&["scheduler", "add", "daily", "09:00", alias])
        );
    }

    fn pack(aliases: &str) -> Pack {
        crate::packs::pack::parse("p", &format!("[pack]\nid = \"p\"\nlabel = \"P\"\n[alias]\n{aliases}"))
            .unwrap_or_else(|problems| panic!("{problems:?}"))
    }

    #[test]
    fn the_command_starts_by_position_so_a_pack_may_alias_when_words() {
        // ADR 0031 §1 (#175's review): nothing is reserved (§6 freed `schedule` itself too).
        let pack = pack("\"daily\" = \"records.list\"\n\"mon\" = \"workspaces.activate\"\n");
        // `daily` as WHEN, then `daily` the alias as the command: only the second is rewritten.
        let line = w(&["scheduler", "add", "daily", "09:00", "--catch-up", "skip", "daily", "lc"]);
        assert_eq!(
            rewrite_inner(line, Some(&pack)),
            w(&["scheduler", "add", "daily", "09:00", "--catch-up", "skip", "records", "list", "lc"])
        );
        // An option's value is never the command, even when it's an alias's or a command's name.
        let line = w(&["scheduler", "add", "mon", "09:00", "--lane", "mon", "mon", "site"]);
        assert_eq!(
            rewrite_inner(line, Some(&pack)),
            w(&["scheduler", "add", "mon", "09:00", "--lane", "mon", "workspaces", "activate", "site"])
        );
        let p = plan(&["weekdays", "09:00", "--lane", "workspaces", "workspaces", "activate", "site"]);
        assert_eq!((p.lane.as_deref(), p.command), (Some("workspaces"), w(&["workspaces", "activate", "site"])));
    }

    #[test]
    fn schedule_is_free_for_a_pack_again() {
        // ADR 0031 §6: `shimmer schedule` went, so `schedule` is no longer a command word.
        let pack = crate::packs::pack::parse(
            "p",
            "[pack]\nid = \"p\"\nlabel = \"P\"\n[alias]\n\"schedule\" = \"scheduler.add\"\n",
        )
        .unwrap();
        assert_eq!(pack.aliases["schedule"].words, ["scheduler", "add"]);
    }

    struct Answer(Option<&'static str>, bool);
    impl Prompt for Answer {
        fn interactive(&self) -> bool {
            self.1
        }
        fn confirm(&mut self, _: &str) -> bool {
            false
        }
        fn ask(&mut self, _: &str) -> Option<String> {
            self.0.map(String::from)
        }
    }

    #[test]
    fn catch_up_is_asked_at_a_terminal_and_required_without_one() {
        let when = When { schedule: json!({}), words: "daily 09:00 (local)".into() };
        assert_eq!(ask_catch_up(&mut Answer(Some("2"), true), &when).unwrap(), "run_once");
        assert_eq!(ask_catch_up(&mut Answer(Some("skip"), true), &when).unwrap(), "skip");
        assert!(ask_catch_up(&mut Answer(Some("maybe"), true), &when).is_err());
        let e = ask_catch_up(&mut Answer(Some("1"), false), &when).unwrap_err();
        assert!(e.message.contains("--catch-up skip, run-once or backfill"), "never guessed: {}", e.message);
    }
}
