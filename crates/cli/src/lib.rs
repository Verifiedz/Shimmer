//! `shimmer-cli`: the command-line client. A thin client (CLAUDE.md §2): it parses a command,
//! sends requests over the socket, and renders the answers. No business logic lives here.
//!
//! Depends on `core` and `proto` only (§3 rule 2). Command packs (§15.1, ADR 0013) live in
//! [`packs`]: aliases are rewritten to canonical command words here and never reach the wire.

mod args;
mod autostart;
mod client;
mod complete;
mod csv;
mod doctor;
mod editor;
mod first;
mod flags;
mod follow;
pub mod packs;
mod queue;
mod records;
mod render;
mod schedule;
mod scheduler;
mod schedules;
mod startup;
mod style;
mod when;
mod workspaces;

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{json, Value};
use shimmer_core::{Error, Result};
use shimmer_proto::ops;

pub use args::{Args, Command, USAGE};
pub use client::{Client, ConnectError};
pub use queue::QueueCmd;
pub use records::RecordsCmd;
pub use scheduler::SchedulerCmd;
pub use workspaces::WorkspacesCmd;

/// Run `shimmer <args>`: `args` are what follows the binary name. Exit codes: 0 success,
/// 1 the daemon (or reaching it) failed, 2 bad usage.
pub async fn run(args: Vec<String>) -> ExitCode {
    run_line(None, args).await
}

/// The alias this binary was started as, when a link made by `shimmer packs use` ran it
/// (`ikuzo deep-work`), or `None` when it should run as plain `shimmer` (ADR 0013 §10). The app
/// asks this before anything else, so `ikuzo daemon` activates a workspace called `daemon`.
pub fn invoked_alias(argv0: &str) -> Option<String> {
    let name = std::path::Path::new(argv0).file_stem()?.to_str()?;
    let home = shimmer_proto::paths::shimmer_home();
    (name != "shimmer" && packs::active::is_known_alias(name, packs::settings::path().as_deref(), &home))
        .then(|| name.to_string())
}

/// Run `<alias> <args>`, as if it were `shimmer <alias> <args>`.
pub async fn run_as_alias(alias: String, args: Vec<String>) -> ExitCode {
    run_line(Some(alias), args).await
}

async fn run_line(alias: Option<String>, args: Vec<String>) -> ExitCode {
    // Tab, from a completion script (#188): answered before anything else, and never by starting
    // a daemon.
    if alias.is_none() && args.first().is_some_and(|a| a == "__complete") {
        return complete_line(&args[1..]).await;
    }
    // Command packs (ADR 0013): find the active pack, then turn an alias into the command it
    // stands for before anything else sees the line. A pack problem is only ever a warning.
    let front = match packs::active::take_front_flags(args) {
        Ok(f) => f,
        Err(e) => {
            style::init(false, false, &Default::default());
            eprintln!("{} {e}\n\n{}", style::paint_err(style::Role::Error, "shimmer:"), style::decorate_err(USAGE));
            return ExitCode::from(2);
        }
    };
    let active = packs::active::load(front.pack.as_deref());
    // Colour is decided once, as soon as the pack is known (#189): its warnings and a usage
    // error are coloured too.
    let colors = active.pack.as_ref().map(|p| p.colors.clone()).unwrap_or_default();
    style::init(front.no_color, front.args.iter().any(|a| a == "--json"), &colors);
    // `shimmer packs …` reports pack problems itself, and needs no aliases: no warning there.
    // `shimmer doctor` lists them in its report instead.
    let packs_command =
        packs::active::command_word(&front.args).is_some_and(|i| front.args[i] == "packs" || front.args[i] == "doctor");
    if !packs_command {
        for warning in &active.warnings {
            eprintln!("{} {warning}", style::paint_err(style::Role::Warning, "shimmer:"));
        }
    }
    if front.args.first().is_some_and(|a| a == "completions") {
        let colors = active.pack.as_ref().map(|p| p.colors.clone()).unwrap_or_default();
        let script = match front.args.get(1..) {
            Some([shell]) => daemon_exe()
                .map_err(|e| e.message)
                .and_then(|exe| complete::script(shell, &exe, active.pack.as_ref(), &style::for_script(&colors))),
            _ => Err("usage: shimmer completions bash|zsh|fish".to_owned()),
        };
        return match script {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{} {e}", style::paint_err(style::Role::Error, "shimmer:"));
                ExitCode::from(2)
            }
        };
    }
    let help = match (&active.pack, front.canonical) {
        (Some(pack), false) => format!("{USAGE}\n\n{}", packs::active::help_section(pack)),
        _ => USAGE.to_string(),
    };
    let mut line = front.args;
    if let Some(alias) = alias {
        // Started through a link: the alias must still be in the active pack.
        match &active.pack {
            Some(pack) if pack.aliases.contains_key(&alias) => line.insert(0, alias),
            Some(pack) => {
                eprintln!("{alias}: isn't an alias in your active pack ({}). Run 'shimmer packs link'.", pack.id);
                return ExitCode::from(2);
            }
            None => {
                eprintln!("{alias}: no command pack is active. Run 'shimmer packs use NAME', or 'shimmer packs unlink' to remove this command.");
                return ExitCode::from(2);
            }
        }
    }
    let args = packs::active::rewrite(line, active.pack.as_ref());
    // `shimmer schedule WHEN … ALIAS …`: the alias where the scheduled command starts, too.
    let args = schedule::rewrite_inner(args, active.pack.as_ref());

    let usage = usage_for(&args);
    let args = match Args::parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{} {e}\n\n{}", style::paint_err(style::Role::Error, "shimmer:"), style::decorate_err(usage));
            return ExitCode::from(2);
        }
    };
    // Doctor reports even when the daemon can't be reached, and sets its own exit code.
    if args.command == Command::Doctor {
        return doctor::run(&args, &active.warnings).await;
    }
    match execute(&args, help).await {
        Ok(Some(text)) => {
            // Output showing people's own data is printed as drawn: its tables are coloured as
            // they're built, and the finishing pass must never read a record's title as Shimmer's
            // own text (#192's review).
            let text = if prints_data(&args.command) { text } else { style::decorate(&text) };
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {}", style::paint_err(style::Role::Error, "shimmer:"), render::error(&e));
            ExitCode::FAILURE
        }
    }
}

/// `shimmer __complete WORDS…`: the suggestions for the last word (#188). `--pack` and
/// `--socket` on the line count, as they would when it runs.
async fn complete_line(words: &[String]) -> ExitCode {
    let after = |flag: &str| flag_value(words, flag);
    let active = packs::active::load(after("--pack").as_deref());
    let socket = after("--socket").map(PathBuf::from).unwrap_or_else(shimmer_proto::paths::socket_path);
    let cx = complete::Context {
        pack: active.pack.as_ref(),
        pack_folders: complete::pack_folders(&shimmer_proto::paths::shimmer_home()),
    };
    let groups = complete::complete(words, &cx, &mut complete::Daemon::new(socket)).await;
    print!("{}", complete::print(&groups));
    ExitCode::SUCCESS
}

/// `flag`'s value on a command line: `--socket PATH` or `--socket=PATH`, as the CLI itself reads it.
fn flag_value(words: &[String], flag: &str) -> Option<String> {
    let inline = format!("{flag}=");
    words.iter().enumerate().find_map(|(i, w)| match w.strip_prefix(&inline) {
        Some(value) => Some(value.to_owned()),
        None if w == flag => words.get(i + 1).cloned(),
        None => None,
    })
}

/// The help to show after a usage error: chosen by the command word the user typed, not by
/// searching the error message, so `shimmer workspaces activate x --bogus` gets the workspaces
/// help even though its error ("unknown option '--bogus'") never says "workspaces".
fn usage_for(args: &[String]) -> &'static str {
    let mut words = args.iter().map(String::as_str);
    while let Some(word) = words.next() {
        match word {
            // `--socket PATH` takes the next word; skip it too.
            "--socket" => {
                words.next();
            }
            w if w.starts_with('-') => {}
            "records" => return records::USAGE,
            "workspaces" => return workspaces::USAGE,
            "queue" => return queue::USAGE,
            "scheduler" => return scheduler::USAGE,
            "schedule" => return schedule::USAGE,
            "packs" => return packs::cmd::USAGE,
            "doctor" => return doctor::USAGE,
            "startup" => return startup::USAGE,
            _ => return USAGE,
        }
    }
    USAGE
}

/// Commands whose output shows what people typed into Shimmer (record fields, collection labels,
/// workspace labels, a trigger's params, raw JSON). The finishing pass skips them, so a title like
/// "Run shimmer records list (see notes)" or a CSV value stays exactly as typed (#192's review).
fn prints_data(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Call { .. }
            | Command::Records(
                RecordsCmd::List { .. }
                    | RecordsCmd::Get { .. }
                    | RecordsCmd::Export { .. }
                    | RecordsCmd::Trash { .. }
                    | RecordsCmd::Collections
                    | RecordsCmd::Check { .. }
                    | RecordsCmd::Peek { file: true, .. }
            )
            | Command::Workspaces(WorkspacesCmd::List | WorkspacesCmd::Status { .. })
            | Command::Queue(QueueCmd::List { .. } | QueueCmd::Show { .. })
            | Command::Scheduler(SchedulerCmd::List | SchedulerCmd::Show { .. })
            | Command::Schedule(_)
    )
}

/// Once, the first time something that only runs while the daemon runs is made (ADR 0034 §7). Only
/// to a person at a terminal: never with `--json`, and never into a script's output.
fn nudge_startup(json: bool, what: &str) {
    use std::io::IsTerminal as _;
    if json || !std::io::stderr().is_terminal() {
        return;
    }
    let os = startup::Env::from_process().os;
    if let Some(note) = startup::nudge(packs::settings::path().as_deref(), os, what) {
        eprintln!("{}", style::paint_err(style::Role::Hint, &note));
    }
}

/// `help` is the top-level help: [`USAGE`], plus the active pack's aliases unless `--canonical`.
async fn execute(args: &Args, help: String) -> Result<Option<String>> {
    match &args.command {
        Command::Help => return Ok(Some(help)),
        Command::Records(RecordsCmd::Help) => return Ok(Some(records::USAGE.into())),
        Command::Workspaces(WorkspacesCmd::Help) => return Ok(Some(workspaces::USAGE.into())),
        Command::Queue(QueueCmd::Help) => return Ok(Some(queue::USAGE.into())),
        Command::Scheduler(SchedulerCmd::Help) => return Ok(Some(scheduler::USAGE.into())),
        Command::Schedule(schedule::ScheduleCmd::Help) => return Ok(Some(schedule::USAGE.into())),
        Command::DoctorHelp => return Ok(Some(doctor::USAGE.into())),
        // Packs live entirely in the client: no daemon needed.
        Command::Packs(cmd) => return packs::cmd::run(cmd, &packs::cmd::Env::from_process()).map(Some),
        // The login service is the operating system's, not the daemon's (ADR 0034).
        Command::Startup(cmd) => return startup::run(cmd, &startup::Env::from_process(), &mut startup::Real).map(Some),
        _ => {}
    }
    let Some(mut client) = connect(args).await? else { return Ok(None) };

    if let Command::Records(cmd) = &args.command {
        return records::run(&mut client, cmd, args.json, &mut workspaces::Terminal).await.map(Some);
    }
    if let Command::Queue(cmd) = &args.command {
        return queue::run(&mut client, cmd, args.json).await.map(Some);
    }
    if let Command::Scheduler(cmd) = &args.command {
        let out = scheduler::run(&mut client, cmd, args.json).await?;
        if matches!(cmd, SchedulerCmd::Add(_)) {
            nudge_startup(args.json, "schedules");
        }
        return Ok(Some(out));
    }
    if let Command::Workspaces(cmd) = &args.command {
        return workspaces::run(&mut client, cmd, args.json, &mut workspaces::Terminal).await.map(Some);
    }
    if let Command::Schedule(cmd) = &args.command {
        let out = schedule::run(&mut client, cmd, args.json, &mut workspaces::Terminal).await?;
        if matches!(cmd, schedule::ScheduleCmd::Add(_)) {
            nudge_startup(args.json, "schedules");
        }
        return Ok(Some(out));
    }
    if let Command::Call { op, params, first: Some(yes) } = &args.command {
        return call_first(&mut client, op, params.clone(), *yes).await.map(|data| Some(output(args, &data)));
    }
    let (op, params) = match &args.command {
        Command::Ping => (ops::CORE_PING, json!({})),
        Command::Manifest => (ops::CORE_MANIFEST, json!({})),
        Command::Shutdown => (ops::CORE_SHUTDOWN, json!({})),
        Command::Call { op, params, .. } => (op.as_str(), params.clone()),
        Command::Help
        | Command::Doctor
        | Command::DoctorHelp
        | Command::Records(_)
        | Command::Workspaces(_)
        | Command::Queue(_)
        | Command::Scheduler(_)
        | Command::Schedule(_)
        | Command::Packs(_)
        | Command::Startup(_) => {
            unreachable!("handled above")
        }
    };
    let data = client.call(op, params).await?;
    Ok(Some(output(args, &data)))
}

/// `call OP --first` (#222): only for an op the manifest says is queued, since the daemon ignores
/// the queue block on an inline one and a silent no-op would mislead.
async fn call_first(client: &mut Client, op: &str, params: Value, yes: bool) -> Result<Value> {
    let manifest = client.call(ops::CORE_MANIFEST, json!({})).await?;
    let spec = manifest["modules"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["commands"].as_array().into_iter().flatten())
        .find(|c| c["op"] == op);
    // The core, queue and scheduler ops aren't in a module's list, and all answer at once.
    match spec {
        Some(c) if c["execution"].get("queued").is_some() => {}
        Some(_) => return Err(Error::invalid_params(format!("--first is for queued ops; '{op}' answers at once"))),
        None => {
            return Err(Error::invalid_params(format!(
                "--first is for queued ops; '{op}' isn't one in 'shimmer manifest'"
            )))
        }
    }
    match first::send(client, op, params, yes, &mut workspaces::Terminal).await? {
        Some(data) => Ok(data),
        None => Ok(json!({"queued": false})),
    }
}

/// `None` when there is nothing to do: `shimmer shutdown` with no daemon running.
async fn connect(args: &Args) -> Result<Option<Client>> {
    let socket = args.socket.clone().unwrap_or_else(shimmer_proto::paths::socket_path);
    let shutdown = args.command == Command::Shutdown;
    match Client::connect(&socket).await {
        Ok(c) => Ok(Some(c)),
        // Starting a daemon only to stop it again helps nobody.
        Err(e) if e.nobody_listening() && shutdown => {
            eprintln!("shimmer: the daemon is not running");
            Ok(None)
        }
        Err(e) if e.nobody_listening() && args.socket.is_none() => {
            autostart::start_and_connect(&daemon_exe()?, &socket).await.map(Some)
        }
        Err(e) => Err(e.into_error(&socket)),
    }
}

fn output(args: &Args, data: &Value) -> String {
    if args.json {
        return render::json(data);
    }
    match args.command {
        Command::Ping => render::ping(data),
        Command::Manifest => render::manifest(data),
        Command::Shutdown => render::shutdown(),
        Command::Call { .. }
        | Command::Help
        | Command::Doctor
        | Command::DoctorHelp
        | Command::Records(_)
        | Command::Workspaces(_)
        | Command::Queue(_)
        | Command::Scheduler(_)
        | Command::Schedule(_)
        | Command::Packs(_)
        | Command::Startup(_) => render::json(data),
    }
}

/// The daemon is this same binary, run as `shimmer daemon`. Symlinks are resolved: started as
/// `ikuzo` through a pack link, the binary must not start the daemon as `ikuzo daemon`, which
/// would run the alias (ADR 0013 §10).
pub(crate) fn daemon_exe() -> Result<PathBuf> {
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| Error::unavailable(format!("cannot find the shimmer binary to start the daemon: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(args: &[&str]) -> &'static str {
        usage_for(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn completion_reads_socket_and_pack_in_either_form() {
        let w = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(flag_value(&w(&["shimmer", "--socket", "/tmp/s", "ping"]), "--socket").as_deref(), Some("/tmp/s"));
        assert_eq!(flag_value(&w(&["shimmer", "--socket=/tmp/s", "ping"]), "--socket").as_deref(), Some("/tmp/s"));
        assert_eq!(flag_value(&w(&["shimmer", "--pack=short", "up"]), "--pack").as_deref(), Some("short"));
        assert_eq!(flag_value(&w(&["shimmer", "ping"]), "--socket"), None);
    }

    #[test]
    fn output_with_peoples_data_isnt_read_as_shimmers_own_text() {
        let cmd = |words: &[&str]| Args::parse(words.iter().map(|w| w.to_string())).unwrap().command;
        for words in [
            &["records", "list", "lc"][..],
            &["records", "get", "lc/x"],
            &["records", "export", "lc"],
            &["records", "collections"],
            &["workspaces", "status", "site"],
            &["scheduler", "list"],
            &["call", "core.ping"],
        ] {
            assert!(prints_data(&cmd(words)), "{words:?}");
        }
        for words in
            [&["records", "complete", "lc/x"][..], &["workspaces", "templates"], &["records", "peek", "leetcode"]]
        {
            assert!(!prints_data(&cmd(words)), "{words:?}: Shimmer's own messages and pages");
        }
    }

    #[test]
    fn usage_follows_the_command_word() {
        assert_eq!(usage(&["workspaces", "activate", "x", "--bogus"]), workspaces::USAGE);
        assert_eq!(usage(&["--json", "--socket", "/tmp/s", "workspaces", "status"]), workspaces::USAGE);
        assert_eq!(usage(&["records", "list", "--limit", "x"]), records::USAGE);
        assert_eq!(usage(&["call"]), USAGE);
        assert_eq!(usage(&["--bogus"]), USAGE);
        assert_eq!(usage(&[]), USAGE);
    }
}
