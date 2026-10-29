//! `shimmer <command>` arguments. Hand-rolled like `shimmer mockd`'s: a handful of commands does not
//! need a parser crate. Command packs (§15.1) have already rewritten any alias by the time this
//! runs (`crate::packs::active`).

use std::path::PathBuf;

use serde_json::Value;

use crate::automations::{self, AutomationsCmd};
use crate::fetchers::{self, FetchersCmd};
use crate::notify::{self, NotifyCmd};
use crate::packs::cmd::{self as packs, PacksCmd};
use crate::queue::{self, QueueCmd};
use crate::records::{self, RecordsCmd};
use crate::scheduler::{self, SchedulerCmd};
use crate::startup::{self, StartupCmd};
use crate::workspaces::{self, WorkspacesCmd};

pub const USAGE: &str = "usage: shimmer [--socket PATH] [--json] <command>

new here? start from a template: shimmer records templates, or shimmer workspaces templates

commands:
  records …             add, list and complete records (shimmer records --help)
  workspaces …          list, inspect and reset workspaces (shimmer workspaces --help)
  scheduler …           run commands on a schedule, and list, disable or remove them
                        (shimmer scheduler --help)
  automations …         rules: when something happens, run a command (shimmer automations --help)
  notify …              your messages: the inbox, quiet hours, pause (shimmer notify --help)
  fetchers …            sources that reach outward for new items (shimmer fetchers --help)
  queue …               what's running and waiting, and cancel or reorder it (shimmer queue --help)
  doctor                check everything and say how to fix what's wrong
  packs …               command packs: themed aliases for these commands (shimmer packs --help)
  startup …             start the daemon every time you log in (shimmer startup --help)
  completions SHELL     print Tab completion for bash, zsh or fish, to load in your shell:
                        source <(shimmer completions zsh), e.g. in ~/.zshrc
  shutdown              stop the daemon

under the hood:
  ping                  check the daemon is up
  manifest              list registered modules, their ops and lanes
  call OP [PARAMS]      send any op; PARAMS is a JSON object (default {});
       [--first [--yes]]  --first: a queued op ahead of its lane (asks first)
  daemon [--quiet]      run the daemon in the foreground (Ctrl-C to stop); --quiet: nothing
                        scheduled runs and modules can't queue tasks, for this run only
  mockd                 run the mock daemon for building clients (shimmer mockd --help)

options:
  --socket PATH         talk to this socket instead of the default; never auto-starts
  --json                print the daemon's raw JSON instead of formatted output
  --pack NAME           use this command pack for one command ('none' for plain names);
                        goes before the command
  --no-color            no colour (also when NO_COLOR is set, or output isn't a terminal);
                        goes before the command
  -h, --help            show this help; with --canonical, without the active pack's aliases

The daemon is started automatically when it is not running.";

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Help,
    Ping,
    Manifest,
    Shutdown,
    /// `first`: `--first` (#222), `Some(yes)`.
    Call {
        op: String,
        params: Value,
        first: Option<bool>,
    },
    Doctor,
    DoctorHelp,
    Records(RecordsCmd),
    Workspaces(WorkspacesCmd),
    Queue(QueueCmd),
    Scheduler(SchedulerCmd),
    Automations(AutomationsCmd),
    Fetchers(FetchersCmd),
    Packs(PacksCmd),
    Notify(NotifyCmd),
    Startup(StartupCmd),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Args {
    /// `None` = the default socket, which is also the only case where the daemon auto-starts.
    pub socket: Option<PathBuf>,
    pub json: bool,
    pub command: Command,
}

impl Args {
    /// `args` are what follows `shimmer`. The global flags below may appear anywhere; any other
    /// option belongs to the command, which decides whether it takes one.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let (mut socket, mut json, mut help) = (None, false, false);
        let mut words = Vec::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f.to_owned(), Some(v.to_owned())),
                _ => (arg, None),
            };
            match flag.as_str() {
                "-h" | "--help" => help = true,
                "--json" => json = true,
                "--socket" => {
                    let v = inline.or_else(|| args.next()).filter(|v| !v.is_empty());
                    socket = Some(PathBuf::from(v.ok_or("--socket needs a value")?));
                }
                // Kept whole, `=value` included, for the command to read.
                _ => words.push(match inline {
                    Some(v) => format!("{flag}={v}"),
                    None => flag,
                }),
            }
        }
        let command = match (help, words.first().map(String::as_str)) {
            (true, Some("records")) => Command::Records(RecordsCmd::Help),
            (true, Some("workspaces")) => Command::Workspaces(WorkspacesCmd::Help),
            (true, Some("queue")) => Command::Queue(QueueCmd::Help),
            (true, Some("scheduler")) => Command::Scheduler(SchedulerCmd::Help),
            (true, Some("automations")) => Command::Automations(AutomationsCmd::Help),
            (true, Some("fetchers")) => Command::Fetchers(FetchersCmd::Help),
            // `schedule --help` is what muscle memory types: the same pointer as `schedule …`.
            (true, Some("schedule")) => command(words.clone())?,
            (true, Some("packs")) => Command::Packs(PacksCmd::Help),
            (true, Some("notify")) => Command::Notify(NotifyCmd::Help),
            (true, Some("doctor")) => Command::DoctorHelp,
            (true, Some("startup")) => Command::Startup(StartupCmd::Help),
            (true, _) => Command::Help,
            (false, _) => command(words)?,
        };
        Ok(Self { socket, json, command })
    }
}

fn command(words: Vec<String>) -> Result<Command, String> {
    let mut words = words.into_iter();
    let Some(word) = words.next() else { return Ok(Command::Help) };
    let rest: Vec<String> = words.collect();
    if word == "records" {
        return records::parse(rest).map(Command::Records);
    }
    if word == "workspaces" {
        return workspaces::parse(rest).map(Command::Workspaces);
    }
    if word == "queue" {
        return queue::parse(rest).map(Command::Queue);
    }
    if word == "scheduler" {
        return scheduler::parse(rest).map(Command::Scheduler);
    }
    if word == "automations" {
        return automations::parse(rest).map(Command::Automations);
    }
    if word == "fetchers" {
        return fetchers::parse(rest).map(Command::Fetchers);
    }
    if word == "schedule" {
        // ADR 0031 §6: one group for schedules. Said once, never a second working spelling.
        return Err("'shimmer schedule' is now 'shimmer scheduler add', e.g. shimmer scheduler add weekdays 09:00 \
                    --catch-up skip workspaces activate site"
            .into());
    }
    if word == "notify" {
        return notify::parse(rest).map(Command::Notify);
    }
    if word == "packs" {
        return packs::parse(rest).map(Command::Packs);
    }
    if word == "startup" {
        return startup::parse(rest).map(Command::Startup);
    }
    if word == "doctor" {
        return crate::doctor::parse(rest).map(|()| Command::Doctor);
    }
    if word == "call" {
        return call(rest);
    }
    if let Some(opt) = rest.iter().find(|w| w.starts_with('-') && w.len() > 1) {
        return Err(format!("unknown option '{}'", opt.split('=').next().unwrap_or(opt)));
    }
    let no_args = |c: Command| if rest.is_empty() { Ok(c) } else { Err(format!("'{word}' takes no arguments")) };
    match word.as_str() {
        "help" => no_args(Command::Help),
        "ping" => no_args(Command::Ping),
        "manifest" => no_args(Command::Manifest),
        "shutdown" => no_args(Command::Shutdown),
        "call" => call(rest),
        other if other.starts_with('-') && other.len() > 1 => {
            Err(format!("unknown option '{}'", other.split('=').next().unwrap_or(other)))
        }
        other => Err(crate::suggest::unknown_command(None, other, USAGE)),
    }
}

fn call(rest: Vec<String>) -> Result<Command, String> {
    let (mut first, mut yes) = (false, false);
    let mut words = Vec::new();
    for word in rest {
        match word.as_str() {
            "--first" => first = true,
            "--yes" | "-y" => yes = true,
            w if w.starts_with('-') && w.len() > 1 => {
                return Err(format!("unknown option '{}'", w.split('=').next().unwrap_or(w)))
            }
            _ => words.push(word),
        }
    }
    if yes && !first {
        return Err("'call' takes --yes only with --first".into());
    }
    let mut rest = words.into_iter();
    let op = rest.next().ok_or("call needs an op, e.g. 'shimmer call core.ping'")?;
    let params = match rest.next() {
        None => Value::Object(Default::default()),
        Some(text) => {
            let v: Value = serde_json::from_str(&text).map_err(|e| format!("PARAMS is not valid JSON: {e}"))?;
            if !v.is_object() {
                return Err("PARAMS must be a JSON object, e.g. '{\"id\":\"two-sum\"}'".into());
            }
            v
        }
    };
    if rest.next().is_some() {
        return Err("call takes an op and at most one PARAMS argument; quote the JSON".into());
    }
    Ok(Command::Call { op, params, first: first.then_some(yes) })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        Args::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn commands_and_flags_anywhere() {
        let a = parse(&["ping"]).unwrap();
        assert_eq!(a, Args { socket: None, json: false, command: Command::Ping });

        let a = parse(&["--json", "manifest", "--socket", "/tmp/s.sock"]).unwrap();
        assert_eq!(a, Args { socket: Some("/tmp/s.sock".into()), json: true, command: Command::Manifest });
        assert_eq!(parse(&["manifest", "--socket=/tmp/s.sock"]).unwrap().socket, Some("/tmp/s.sock".into()));
    }

    #[test]
    fn help_wins_and_is_the_default() {
        assert_eq!(parse(&[]).unwrap().command, Command::Help);
        assert_eq!(parse(&["ping", "--help"]).unwrap().command, Command::Help);
        assert_eq!(parse(&["-h"]).unwrap().command, Command::Help);
        assert_eq!(parse(&["help"]).unwrap().command, Command::Help);
    }

    #[test]
    fn call_takes_an_op_and_optional_object() {
        assert_eq!(
            parse(&["call", "core.ping"]).unwrap().command,
            Command::Call { op: "core.ping".into(), params: json!({}), first: None }
        );
        assert_eq!(
            parse(&["call", "records.list", r#"{"collection":"leetcode"}"#]).unwrap().command,
            Command::Call { op: "records.list".into(), params: json!({"collection": "leetcode"}), first: None }
        );
        assert!(parse(&["call"]).unwrap_err().contains("needs an op"));
        assert!(parse(&["call", "x.y", "{nope"]).unwrap_err().contains("not valid JSON"));
        assert!(parse(&["call", "x.y", "[1]"]).unwrap_err().contains("JSON object"));
        assert!(parse(&["call", "x.y", "{}", "extra"]).unwrap_err().contains("at most one"));
        // #222.
        assert_eq!(
            parse(&["call", "workspaces.activate", r#"{"id":"x"}"#, "--first", "--yes"]).unwrap().command,
            Command::Call { op: "workspaces.activate".into(), params: json!({"id": "x"}), first: Some(true) }
        );
        assert_eq!(
            parse(&["call", "x.y", "--first"]).unwrap().command,
            Command::Call { op: "x.y".into(), params: json!({}), first: Some(false) }
        );
        assert!(parse(&["call", "x.y", "--yes"]).unwrap_err().contains("--yes only with --first"));
        assert!(parse(&["call", "x.y", "--priority", "override"]).unwrap_err().contains("unknown option '--priority'"));
    }

    #[test]
    fn bad_input() {
        assert!(parse(&["nope"]).unwrap_err().contains("unknown command"));
        assert!(parse(&["ping", "extra"]).unwrap_err().contains("takes no arguments"));
        assert!(parse(&["--frobnicate"]).unwrap_err().contains("unknown option"));
        assert!(parse(&["ping", "--socket"]).unwrap_err().contains("needs a value"));
        assert!(parse(&["ping", "--socket="]).unwrap_err().contains("needs a value"));
        assert!(parse(&["ping", "--title", "x"]).unwrap_err().contains("unknown option '--title'"));
        assert!(parse(&["ping", "--title=x"]).unwrap_err().contains("unknown option '--title'"));
    }

    #[test]
    fn records_get_their_own_options_and_the_globals_still_work() {
        let a = parse(&["records", "list", "leetcode", "--status", "todo", "--json", "--difficulty=easy"]).unwrap();
        assert!(a.json);
        let RecordsCmd::List { filter, .. } = a.command.records().unwrap() else { panic!("not a list") };
        assert_eq!(filter, &[("status".into(), "todo".into()), ("difficulty".into(), "easy".into())]);

        assert_eq!(parse(&["records", "--help"]).unwrap().command, Command::Records(RecordsCmd::Help));
        assert_eq!(parse(&["records"]).unwrap().command, Command::Records(RecordsCmd::Help));
    }

    impl Command {
        fn records(&self) -> Option<&RecordsCmd> {
            match self {
                Command::Records(r) => Some(r),
                _ => None,
            }
        }
    }
}
