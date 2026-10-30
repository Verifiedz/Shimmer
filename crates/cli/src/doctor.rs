//! `shimmer doctor` (ADR 0033): one command that says what's wrong and how to fix it.
//!
//! The CLI checks only what is the client's own business: can it reach the daemon, does the
//! daemon's version match, is the active command pack fine. Everything else comes from the
//! modules: each one that has a `<module>.health` op is asked, found through `core.manifest`, so
//! a module added later is checked without changing this file (CLAUDE.md §9). Nothing here
//! decides whether a record or a workspace is broken; it only shows what the daemon said.

use std::fmt::Write as _;
use std::process::ExitCode;

use serde_json::{json, Value};
use shimmer_proto::ops;

use crate::args::Args;
use crate::client::Client;
use crate::packs::pack;
use crate::style::{paint, Role};
use crate::{autostart, render};

pub const USAGE: &str = "usage: shimmer doctor [--json]

Checks that the daemon is running and matches this shimmer, that your command pack loads, and
asks every module that has a health check (records, workspaces, …) for problems. Each problem
comes with how to fix it. Changes nothing.

Exits 1 when something is broken (FAIL), 0 when there are only warnings or nothing at all.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

/// One line of the report. `area` is `daemon`, `packs`, or the module that reported it.
#[derive(Debug, PartialEq)]
pub struct Check {
    pub area: String,
    pub status: Status,
    pub message: String,
    /// How to fix it, ready to show: a summary, and the command when there is one.
    pub fix: Option<String>,
}

impl Check {
    fn new(area: &str, status: Status, message: impl Into<String>) -> Self {
        Self { area: area.into(), status, message: message.into(), fix: None }
    }

    fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

/// Run the checks and print the report. `pack_warnings` are what loading the active pack said.
pub async fn run(args: &Args, pack_warnings: &[String]) -> ExitCode {
    let mut checks = Vec::new();
    if let Some(mut client) = connect(args, &mut checks).await {
        checks.extend(packs(pack_warnings));
        checks.extend(schedules(&mut client).await);
        checks.extend(modules(&mut client).await);
        // Read from this folder's own `.staging/`: with --socket, the daemon asked may serve
        // another folder, so the check would describe the wrong one (#309's review). Until the
        // daemon answers it itself, it's skipped there and says so.
        match &args.socket {
            None => checks.extend(saves(&shimmer_proto::paths::shimmer_home())),
            Some(_) => checks.push(Check::new(
                "store",
                Status::Warn,
                "saves not checked: with --socket, doctor may be asking another folder's daemon",
            )),
        }
    }
    let failed = checks.iter().any(|c| c.status == Status::Fail);
    println!("{}", if args.json { render::json(&to_json(&checks)) } else { report(&checks) });
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Reach the daemon, starting it as any command would. Says what happened either way.
async fn connect(args: &Args, checks: &mut Vec<Check>) -> Option<Client> {
    let socket = args.socket.clone().unwrap_or_else(shimmer_proto::paths::socket_path);
    let (client, started) = match Client::connect(&socket).await {
        Ok(c) => (Ok(c), false),
        Err(e) if e.nobody_listening() && args.socket.is_none() => {
            let started = match crate::daemon_exe() {
                Ok(exe) => autostart::start_and_connect(&exe, &socket).await,
                Err(e) => Err(e),
            };
            (started, true)
        }
        Err(e) => (Err(e.into_error(&socket)), false),
    };
    let mut client = match client {
        Ok(c) => c,
        Err(e) => {
            // With --socket, doctor never starts a daemon, so "see why it won't start" doesn't fit (#230).
            let fix = match &args.socket {
                Some(path) => format!("is a daemon listening at {}?", path.display()),
                None => "run 'shimmer daemon' to see why it won't start".to_owned(),
            };
            checks.push(Check::new("daemon", Status::Fail, format!("can't reach it: {}", e.message)).fix(fix));
            return None;
        }
    };
    let up = match client.call(ops::CORE_PING, json!({})).await {
        Ok(data) => data["uptime_s"].as_u64().map(render::uptime),
        Err(e) => {
            checks.push(Check::new("daemon", Status::Fail, format!("it doesn't answer: {}", e.message)));
            return None;
        }
    };
    let mut check = daemon(started, up, client.daemon_version(), env!("CARGO_PKG_VERSION"));
    // In another vault, say which (ADR 0041 §5): the report is about that folder.
    if let Some(vault) = crate::vaults::current_name() {
        check.message.push_str(&format!(", in vault {vault}"));
    }
    checks.push(check);
    Some(client)
}

fn daemon(started: bool, up: Option<String>, theirs: &str, ours: &str) -> Check {
    if theirs != ours {
        return Check::new("daemon", Status::Warn, format!("it is version {theirs}, this shimmer is {ours}"))
            .fix("restart it so they match: shimmer shutdown (the next command starts the new one)");
    }
    let message = match (started, up) {
        (true, _) => "it wasn't running, so doctor started it".to_owned(),
        (false, Some(up)) => format!("running, up {up}"),
        (false, None) => "running".to_owned(),
    };
    Check::new("daemon", Status::Ok, message)
}

fn packs(warnings: &[String]) -> Vec<Check> {
    if warnings.is_empty() {
        return vec![Check::new("packs", Status::Ok, "your command pack loads (or none is active)")];
    }
    // Each warning already says which command shows the details.
    warnings.iter().map(|w| Check::new("packs", Status::Warn, w.clone())).collect()
}

/// Schedule files the daemon couldn't read (#263 finding 5): each one is a schedule that has
/// stopped running, so it's a warning with the way out. The scheduler is a core service, not a
/// module, so it has no `<module>.health` for `modules` to find.
async fn schedules(client: &mut Client) -> Vec<Check> {
    let data = match client.call("scheduler.list", json!({})).await {
        Ok(d) => d,
        Err(e) => {
            return vec![Check::new("scheduler", Status::Fail, format!("scheduler.list failed: {}", render::error(&e)))]
        }
    };
    let broken = data["broken"].as_array().cloned().unwrap_or_default();
    if broken.is_empty() {
        let n = data["triggers"].as_array().map_or(0, Vec::len);
        return vec![Check::new("scheduler", Status::Ok, format!("{n} schedule(s), every file readable"))];
    }
    broken
        .iter()
        .map(|b| {
            let id = render::cell(&b["id"]);
            Check::new(
                "scheduler",
                Status::Warn,
                format!(
                    "{id} can't be read, so it doesn't run: {} ({})",
                    render::cell(&b["problem"]),
                    render::cell(&b["file"])
                ),
            )
            .fix(format!("fix the file, or remove it: shimmer scheduler remove {id}"))
        })
        .collect()
}

/// Ask every module with a `<module>.health` op, in the manifest's order.
async fn modules(client: &mut Client) -> Vec<Check> {
    let manifest = match client.call(ops::CORE_MANIFEST, json!({})).await {
        Ok(m) => m,
        Err(e) => return vec![Check::new("daemon", Status::Fail, format!("core.manifest failed: {}", e.message))],
    };
    let mut out = Vec::new();
    for module in manifest["modules"].as_array().into_iter().flatten() {
        let Some(id) = module["id"].as_str() else { continue };
        let op = format!("{id}.health");
        let has = module["commands"].as_array().into_iter().flatten().any(|c| c["op"] == op.as_str());
        if !has {
            continue;
        }
        match client.call(&op, json!({})).await {
            Ok(data) => out.extend(module_checks(id, &data)),
            Err(e) => out.push(Check::new(id, Status::Fail, format!("{op} failed: {}", render::error(&e)))),
        }
    }
    out
}

/// A `<module>.health` answer as report lines (ADR 0033 has the shape). Anything that doesn't
/// fit the shape is shown as it came, never dropped.
pub fn module_checks(area: &str, data: &Value) -> Vec<Check> {
    let Some(list) = data["checks"].as_array() else {
        return vec![Check::new(area, Status::Warn, format!("unexpected answer: {}", render::json(data)))];
    };
    list.iter()
        .map(|c| {
            let status = match c["status"].as_str() {
                Some("ok") => Some(Status::Ok),
                Some("warn") => Some(Status::Warn),
                Some("fail") => Some(Status::Fail),
                _ => None,
            };
            match (status, c["message"].as_str()) {
                (Some(status), Some(message)) => {
                    Check { area: area.into(), status, message: message.into(), fix: fix(&c["fix"]) }
                }
                _ => Check::new(area, Status::Warn, format!("unexpected check: {}", render::json(c))),
            }
        })
        .collect()
}

/// Saves the daemon couldn't finish (#260): a committed one still waiting in `.staging/` after a
/// minute (it's retried before the next save and at the next start), and damaged ones recovery
/// moved to `.staging/broken/`, kept because they may hold data. Read from the folder: the store
/// has no op for this, and the CLI never links it (CLAUDE.md §3).
pub fn saves(home: &std::path::Path) -> Vec<Check> {
    let staging = home.join(".staging");
    let minute_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let dirs = |dir: std::path::PathBuf| -> Vec<std::fs::DirEntry> {
        std::fs::read_dir(dir).map(|d| d.flatten().filter(|e| e.path().is_dir()).collect()).unwrap_or_default()
    };
    let waiting = dirs(staging.clone())
        .into_iter()
        .filter(|e| e.file_name() != "broken" && e.path().join("COMMITTED").exists())
        .filter(|e| e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < minute_ago))
        .count();
    let broken = dirs(staging.join("broken")).len();
    let mut out = Vec::new();
    if waiting > 0 {
        out.push(
            Check::new(
                "store",
                Status::Warn,
                format!("{waiting} save(s) made but not finished: their event isn't logged yet"),
            )
            .fix("it's retried before the next save and at the next start; the daemon log says why it failed"),
        );
    }
    if broken > 0 {
        out.push(
            Check::new(
                "store",
                Status::Warn,
                format!(
                    "{broken} damaged save(s) couldn't be applied and were kept in {}",
                    staging.join("broken").display()
                ),
            )
            .fix(
                "each folder's manifest.json says where its files/ go; the daemon log says why; delete it once handled",
            ),
        );
    }
    if out.is_empty() {
        out.push(Check::new("store", Status::Ok, "every save finished"));
    }
    out
}

/// `{"summary", "op"?, "params"?}` as one line. The command is the one a person would type when
/// the op has a command of its own and one plain argument; otherwise `shimmer call`, which always
/// works.
fn fix(fix: &Value) -> Option<String> {
    if !fix.is_object() {
        return None;
    }
    let summary = fix["summary"].as_str().unwrap_or_default();
    let Some(op) = fix["op"].as_str() else { return (!summary.is_empty()).then(|| summary.to_owned()) };
    let params = fix.get("params").cloned().unwrap_or(json!({}));
    let one_word = params.as_object().filter(|p| p.len() == 1).and_then(|p| p.values().next()?.as_str());
    let command = match (pack::target(op), one_word) {
        (Some(t), Some(arg)) if is_plain(arg) => format!("shimmer {} {arg}", t.words.join(" ")),
        _ => format!("shimmer call {op} {}", single_quoted(&params.to_string())),
    };
    Some(if summary.is_empty() { command } else { format!("{summary}: {command}") })
}

/// `text` in single quotes for a shell, a `'` inside written as `'\''`, so a fix stays
/// copy-pasteable whatever its params hold (#230).
fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Safe to show unquoted in a shell command.
fn is_plain(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
}

/// The colour a check's label is shown in: ok green, warn yellow, FAIL red, as states are (#189).
fn status_role(status: Status) -> Role {
    match status {
        Status::Ok => Role::Success,
        Status::Warn => Role::Warning,
        Status::Fail => Role::Error,
    }
}

pub fn report(checks: &[Check]) -> String {
    let width = checks.iter().map(|c| c.area.len()).max().unwrap_or(0);
    let mut out = String::new();
    for c in checks {
        let label = match c.status {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        };
        // Padded first, so the colour codes never count towards the column width.
        let label = paint(status_role(c.status), &format!("{label:4}"));
        // A message can run over several lines (a TOML parse error does): keep them in its column.
        let indent = " ".repeat(4 + 2 + width + 2);
        let message = c.message.trim_end().replace('\n', &format!("\n{indent}"));
        let _ = writeln!(out, "{label}  {:width$}  {message}", c.area);
        if let Some(fix) = &c.fix {
            let _ = writeln!(out, "{indent}{}", paint(Role::Hint, &format!("→ {fix}")));
        }
    }
    let count = |s: Status| checks.iter().filter(|c| c.status == s).count();
    let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let _ = write!(
        out,
        "\n{}",
        match (count(Status::Fail), count(Status::Warn)) {
            (0, 0) => paint(Role::Success, "Everything looks fine."),
            (f, w) => paint(
                if f > 0 { Role::Error } else { Role::Warning },
                &format!("{}, {}.", plural(f, "problem", "problems"), plural(w, "warning", "warnings")),
            ),
        }
    );
    out
}

fn to_json(checks: &[Check]) -> Value {
    let list: Vec<Value> = checks
        .iter()
        .map(|c| {
            let status = match c.status {
                Status::Ok => "ok",
                Status::Warn => "warn",
                Status::Fail => "fail",
            };
            json!({"area": c.area, "status": status, "message": c.message, "fix": c.fix})
        })
        .collect();
    json!({"checks": list})
}

/// `shimmer doctor` takes no arguments beyond the global flags.
pub fn parse(rest: Vec<String>) -> std::result::Result<(), String> {
    match rest.first() {
        None => Ok(()),
        Some(w) if w.starts_with('-') => Err(format!("unknown option '{}'", w.split('=').next().unwrap_or(w))),
        Some(_) => Err("'doctor' takes no arguments".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_label_is_coloured_by_what_it_means() {
        assert_eq!(status_role(Status::Ok), Role::Success);
        assert_eq!(status_role(Status::Warn), Role::Warning);
        assert_eq!(status_role(Status::Fail), Role::Error);
    }

    #[test]
    fn module_answers_become_lines_with_their_fix_as_a_command() {
        let data = json!({"checks": [
            {"status": "fail", "subject": "deep-work", "message": "deep-work is dirty",
             "fix": {"summary": "run its cleanup script", "op": "workspaces.cleanup", "params": {"id": "deep-work"}}},
            {"status": "warn", "message": "no cleanup", "fix": {"summary": "add cleanup.sh"}},
            {"status": "ok", "message": "fine"},
        ]});
        let checks = module_checks("workspaces", &data);
        assert_eq!(checks[0].fix.as_deref(), Some("run its cleanup script: shimmer workspaces cleanup deep-work"));
        assert_eq!(checks[1].fix.as_deref(), Some("add cleanup.sh"));
        assert_eq!((checks[2].status, checks[2].fix.as_deref()), (Status::Ok, None));
    }

    #[test]
    fn a_fix_with_no_command_of_its_own_uses_call() {
        // An op no pack target names, or params that aren't one plain word: `call` always works.
        let f = |v: Value| fix(&v).unwrap();
        assert_eq!(
            f(json!({"summary": "s", "op": "x.y", "params": {"id": "a"}})),
            r#"s: shimmer call x.y '{"id":"a"}'"#
        );
        assert_eq!(
            f(json!({"op": "workspaces.reset", "params": {"id": "a b"}})),
            r#"shimmer call workspaces.reset '{"id":"a b"}'"#
        );
        assert_eq!(
            f(json!({"op": "records.check", "params": {"collection": "leetcode"}})),
            "shimmer records check leetcode"
        );
    }

    #[test]
    fn a_fix_with_a_quote_in_its_params_stays_pasteable() {
        // #230 (Wasay's #215 review): the JSON goes in single quotes, so a `'` inside is escaped.
        let f = fix(&json!({"op": "x.y", "params": {"id": "it's"}})).unwrap();
        assert_eq!(f, r#"shimmer call x.y '{"id":"it'\''s"}'"#);
    }

    #[test]
    fn an_answer_that_doesnt_fit_is_shown_not_dropped() {
        let checks = module_checks("x", &json!({"surprise": 1}));
        assert_eq!(checks[0].status, Status::Warn);
        assert!(checks[0].message.contains("surprise"), "{}", checks[0].message);
        let checks = module_checks("x", &json!({"checks": [{"status": "bogus", "message": "m"}]}));
        assert!(checks[0].message.contains("unexpected check"), "{}", checks[0].message);
    }

    #[test]
    fn the_daemon_line_says_what_happened() {
        assert_eq!(daemon(false, Some("4s".into()), "0.1.0", "0.1.0").message, "running, up 4s");
        assert_eq!(daemon(true, None, "0.1.0", "0.1.0").message, "it wasn't running, so doctor started it");
        let old = daemon(false, None, "0.0.9", "0.1.0");
        assert_eq!(old.status, Status::Warn);
        assert!(old.fix.unwrap().contains("shimmer shutdown"));
    }

    #[test]
    fn the_report_lines_up_and_counts() {
        let checks = vec![
            Check::new("daemon", Status::Ok, "running, up 4s"),
            Check::new("workspaces", Status::Fail, "deep-work is dirty").fix("run its cleanup script"),
        ];
        let want = "\
ok    daemon      running, up 4s
FAIL  workspaces  deep-work is dirty
                  → run its cleanup script

1 problem, 0 warnings.";
        assert_eq!(report(&checks), want);
        assert_eq!(report(&checks[..1]), "ok    daemon  running, up 4s\n\nEverything looks fine.");

        let multi = [Check::new("records", Status::Fail, "parse error\n  |\n1 | [x\n")];
        assert_eq!(
            report(&multi),
            "FAIL  records  parse error\n                 |\n               1 | [x\n\n1 problem, 0 warnings."
        );
    }

    #[test]
    fn it_takes_no_arguments() {
        assert!(parse(vec![]).is_ok());
        assert!(parse(vec!["x".into()]).unwrap_err().contains("takes no arguments"));
        assert!(parse(vec!["--all".into()]).unwrap_err().contains("unknown option '--all'"));
    }
}
