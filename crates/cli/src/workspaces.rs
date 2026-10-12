//! `shimmer workspaces …`: typed commands over the `workspaces.*` ops (docs/protocol.md,
//! "Workspace ops"; issue #33).
//!
//! A thin client (CLAUDE.md §2): the daemon decides everything (whether a workspace may launch,
//! what dirty means). The CLI only sends ops, shows what comes back, and asks before the two
//! actions that skip safety: `reset` and `force-relaunch`.
//!
//! `activate`, `cleanup` and `force-relaunch` are queued ops: the request returns a task handle
//! at once. With `--wait` the CLI follows the task over the subscription stream (docs/protocol.md,
//! `subscribe`) and prints how it ended. Without it, it returns straight away; that is the
//! default because a workspace's own step calling `shimmer workspaces activate … --wait` would
//! wait on a launch that cannot start until its own launch ends (one lane slot, §6.1).

use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write as _};

use serde_json::{json, Value};
use shimmer_core::{Error, ErrorCode, Result};

use crate::client::Client;
use crate::editor;
use crate::render::{self, cell, table, wrap};
use crate::schedules;

pub const USAGE: &str = "usage: shimmer workspaces <command>

commands:
  list                                  list workspaces and their state
  status NAME                           show one workspace in full: steps, last session, why it is dirty
  activate NAME [--wait]                launch its steps in order
  cleanup NAME [--wait]                 run a dirty workspace's cleanup script
  force-relaunch NAME [--yes] [--wait]  launch a dirty workspace anyway (asks first)
  reset NAME [--yes]                    clear dirty without running cleanup (asks first)
  stop NAME [--wait]                    stop what an active workspace started (its dev server,
                                        services, the windows it can close) and say what it left
  remove NAME                           remove a workspace that isn't running
  edit NAME [FILE] [--path]             open its workspace.toml (or FILE in its folder, e.g.
                                        steps/01-check.sh) in $VISUAL/$EDITOR, then check it;
                                        --path only prints where the file is
  restore NAME                          bring a removed workspace back
  copy NAME NEW [--set QUESTION=ANSWER]… [--label TEXT]
                                        a new workspace like NAME, starting ready; asks whether
                                        to change answers (e.g. another project folder)
  rename NAME NEW [--move-schedules]    give a workspace that isn't running a new name; offers
                                        to move schedules that activate it (--move-schedules:
                                        without asking)

  templates [CATEGORY]                  list ready-made workspaces to start from, by category
  peek TEMPLATE                         everything about a template before you use it: what it
                                        does and changes, what it needs, its steps, what stop
                                        does, and its questions
      [--scripts]                       also every step's script, and the cleanup script stop runs
      [--file PATH]                     only that file, as it is (e.g. lib/shimmer-open.sh)
  reconfigure NAME                      ask its template's questions again, your answers as the
      [--set QUESTION=ANSWER]…          defaults (Enter keeps one, - clears it); only when it's
                                        ready, and only its [env] changes
  new NAME --from TEMPLATE              create a workspace from a template; asks its questions,
      [--set QUESTION=ANSWER]…          e.g. shimmer workspaces new site --from web-project
      [--label TEXT]                    --set answers one without asking (needed in scripts)

--wait  follow the launch until it ends and show how it went. Without it the command
        returns once the launch is queued; check on it with 'shimmer workspaces status NAME'.
        Don't use --wait from a workspace's own step scripts: launches run one at a time.
--yes   skip the question. Needed when no one is at the keyboard (scripts).

A workspace is a folder under data/workspaces/ in your Shimmer folder, holding a
workspace.toml and its steps/ scripts (ADR 0012). Add --json to any command for raw output.";

#[derive(Clone, Debug, PartialEq)]
pub enum WorkspacesCmd {
    Help,
    List,
    Status {
        id: String,
    },
    Activate {
        id: String,
        wait: bool,
    },
    Cleanup {
        id: String,
        wait: bool,
    },
    ForceRelaunch {
        id: String,
        yes: bool,
        wait: bool,
    },
    Reset {
        id: String,
        yes: bool,
    },
    Stop {
        id: String,
        wait: bool,
    },
    /// `file`: inside the workspace folder; `None` is `workspace.toml`.
    Edit {
        id: String,
        file: Option<String>,
        path_only: bool,
    },
    Remove {
        id: String,
    },
    Restore {
        id: String,
    },
    /// `which`: a category (only its templates) or a template (as `peek` shows it).
    Templates {
        which: Option<String>,
    },
    /// `scripts`: each step's script and cleanup's after the summary. `file`: only that file.
    Peek {
        template: String,
        scripts: bool,
        file: Option<String>,
    },
    /// `set`: answers the copy has differently; `label`: its own label (ADR 0026 §2b).
    Copy {
        id: String,
        to: String,
        set: Vec<(String, String)>,
        label: Option<String>,
    },
    /// `move_schedules`: move the schedules that name it without asking (ADR 0026 §2a).
    Rename {
        id: String,
        to: String,
        move_schedules: bool,
    },
    /// `set`: `--set NAME=VALUE` answers, in order (ADR 0025 §9).
    Reconfigure {
        id: String,
        set: Vec<(String, String)>,
    },
    /// `set`: `--set NAME=VALUE` answers, in order (ADR 0025 §8).
    New {
        id: String,
        template: String,
        label: Option<String>,
        set: Vec<(String, String)>,
    },
}

// ---------------------------------------------------------------- parsing

/// `words` are what follows `shimmer workspaces`, with global flags already removed.
pub fn parse(words: Vec<String>) -> std::result::Result<WorkspacesCmd, String> {
    let mut words = words.into_iter();
    let Some(sub) = words.next() else { return Ok(WorkspacesCmd::Help) };
    if sub == "new" {
        return parse_new(words.collect());
    }
    if sub == "peek" {
        return parse_peek(words.collect());
    }
    if sub == "reconfigure" {
        return parse_reconfigure(words.collect());
    }
    if sub == "copy" {
        let usage = "usage: shimmer workspaces copy NAME NEW [--set QUESTION=ANSWER]… [--label TEXT]";
        let (mut names, mut set, mut label) = (Vec::new(), Vec::new(), None);
        let mut words = words;
        while let Some(word) = words.next() {
            if word == "--set" {
                set.push(set_answer(&words.next().ok_or("--set needs QUESTION=ANSWER")?)?);
            } else if let Some(value) = word.strip_prefix("--set=") {
                set.push(set_answer(value)?);
            } else if word == "--label" {
                label = Some(words.next().ok_or("--label needs a value")?);
            } else if let Some(value) = word.strip_prefix("--label=") {
                label = Some(value.to_owned());
            } else if word.starts_with('-') && word.len() > 1 {
                return Err(format!("unknown option '{word}'; {usage}"));
            } else {
                names.push(word);
            }
        }
        let [id, to]: [String; 2] = names.try_into().map_err(|_| usage.to_owned())?;
        return Ok(WorkspacesCmd::Copy { id, to, set, label });
    }
    if sub == "rename" {
        let usage = "usage: shimmer workspaces rename NAME NEW [--move-schedules]";
        let (mut names, mut move_schedules) = (Vec::new(), false);
        for word in words {
            match word.as_str() {
                "--move-schedules" => move_schedules = true,
                w if w.starts_with('-') && w.len() > 1 => return Err(format!("unknown option '{w}'; {usage}")),
                _ => names.push(word),
            }
        }
        let [id, to]: [String; 2] = names.try_into().map_err(|_| usage.to_owned())?;
        return Ok(WorkspacesCmd::Rename { id, to, move_schedules });
    }
    let mut positional = Vec::new();
    let (mut yes, mut wait, mut path_only) = (false, false, false);
    for word in words {
        match word.as_str() {
            "--yes" | "-y" => yes = true,
            "--wait" => wait = true,
            "--path" if sub == "edit" => path_only = true,
            w if w.starts_with('-') && w.len() > 1 => {
                return Err(format!("unknown option '{}'", w.split('=').next().unwrap_or(w)));
            }
            _ => positional.push(word),
        }
    }
    // Which command takes which flag; anything else is a mistake worth naming.
    let (takes_yes, takes_wait) = match sub.as_str() {
        "force-relaunch" => (true, true),
        "activate" | "cleanup" | "stop" => (false, true),
        "reset" => (true, false),
        _ => (false, false),
    };
    for (given, allowed, flag) in [(yes, takes_yes, "--yes"), (wait, takes_wait, "--wait")] {
        if given && !allowed {
            return Err(format!("'workspaces {sub}' takes no {flag}"));
        }
    }
    let name = |positional: Vec<String>| -> std::result::Result<String, String> {
        match <[String; 1]>::try_from(positional) {
            Ok([id]) => Ok(id),
            Err(_) => Err(format!("usage: shimmer workspaces {sub} NAME")),
        }
    };
    match sub.as_str() {
        "help" => Ok(WorkspacesCmd::Help),
        "list" => match positional.is_empty() {
            true => Ok(WorkspacesCmd::List),
            false => Err("'workspaces list' takes no arguments".into()),
        },
        "status" => Ok(WorkspacesCmd::Status { id: name(positional)? }),
        "activate" => Ok(WorkspacesCmd::Activate { id: name(positional)?, wait }),
        "cleanup" => Ok(WorkspacesCmd::Cleanup { id: name(positional)?, wait }),
        "force-relaunch" => Ok(WorkspacesCmd::ForceRelaunch { id: name(positional)?, yes, wait }),
        "reset" => Ok(WorkspacesCmd::Reset { id: name(positional)?, yes }),
        "stop" => Ok(WorkspacesCmd::Stop { id: name(positional)?, wait }),
        "remove" => Ok(WorkspacesCmd::Remove { id: name(positional)? }),
        "edit" => {
            let mut words = positional.into_iter();
            match (words.next(), words.next(), words.next()) {
                (Some(id), file, None) => Ok(WorkspacesCmd::Edit { id, file, path_only }),
                _ => Err("usage: shimmer workspaces edit NAME [FILE] [--path]".into()),
            }
        }
        "restore" => Ok(WorkspacesCmd::Restore { id: name(positional)? }),
        "templates" => {
            let mut words = positional.into_iter();
            match (words.next(), words.next()) {
                (which, None) => Ok(WorkspacesCmd::Templates { which }),
                _ => Err("usage: shimmer workspaces templates [CATEGORY | TEMPLATE]".into()),
            }
        }
        other => Err(format!("unknown workspaces command '{other}'; see 'shimmer workspaces --help'")),
    }
}

/// `--set QUESTION=ANSWER`'s value, split.
fn set_answer(value: &str) -> std::result::Result<(String, String), String> {
    match value.split_once('=') {
        Some((q, a)) if !q.trim().is_empty() => Ok((q.trim().to_owned(), a.to_owned())),
        _ => Err(format!("--set takes QUESTION=ANSWER, e.g. --set PROJECT_DIR=~/code/site, got '{value}'")),
    }
}

/// `reconfigure NAME [--set QUESTION=ANSWER]…`.
fn parse_reconfigure(words: Vec<String>) -> std::result::Result<WorkspacesCmd, String> {
    let usage = "usage: shimmer workspaces reconfigure NAME [--set QUESTION=ANSWER]…";
    let (mut id, mut set) = (None, Vec::new());
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        if word == "--set" {
            set.push(set_answer(&words.next().ok_or("--set needs QUESTION=ANSWER")?)?);
        } else if let Some(value) = word.strip_prefix("--set=") {
            set.push(set_answer(value)?);
        } else if word.starts_with('-') && word.len() > 1 {
            return Err(format!("'workspaces reconfigure' takes --set, not '{word}'"));
        } else if id.replace(word).is_some() {
            return Err(usage.into());
        }
    }
    Ok(WorkspacesCmd::Reconfigure { id: id.ok_or(usage)?, set })
}

/// `peek TEMPLATE [--scripts | --file PATH]`.
fn parse_peek(words: Vec<String>) -> std::result::Result<WorkspacesCmd, String> {
    let usage =
        "usage: shimmer workspaces peek TEMPLATE [--scripts | --file PATH] (see 'shimmer workspaces templates')";
    let (mut template, mut scripts, mut file) = (None, false, None);
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--scripts" => scripts = true,
            "--file" => file = Some(words.next().ok_or("--file needs a path, e.g. --file lib/shimmer-open.sh")?),
            w if w.starts_with("--file=") => file = Some(w["--file=".len()..].to_owned()),
            w if w.starts_with('-') && w.len() > 1 => return Err(format!("unknown option '{w}'; {usage}")),
            _ if template.is_some() => return Err(usage.into()),
            _ => template = Some(word),
        }
    }
    if scripts && file.is_some() {
        return Err("use --scripts or --file, not both".into());
    }
    Ok(WorkspacesCmd::Peek { template: template.ok_or(usage)?, scripts, file })
}

/// `new NAME --from TEMPLATE [--label TEXT] [--set NAME=VALUE]…`: the one command here whose
/// flags take values.
fn parse_new(words: Vec<String>) -> std::result::Result<WorkspacesCmd, String> {
    let usage = "usage: shimmer workspaces new NAME --from TEMPLATE [--label TEXT] [--set QUESTION=ANSWER]…";
    let (mut positional, mut template, mut label, mut set) = (Vec::new(), None, None, Vec::new());
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let Some(flag) = word.strip_prefix("--") else {
            positional.push(word);
            continue;
        };
        let name = flag.split('=').next().unwrap_or(flag);
        if !matches!(name, "from" | "label" | "set") {
            return Err(format!("'workspaces new' takes --from, --label and --set, not '--{name}'"));
        }
        let (name, value) = match flag.split_once('=') {
            Some((name, value)) => (name.to_owned(), value.to_owned()),
            None => (flag.to_owned(), words.next().ok_or(format!("--{flag} needs a value"))?),
        };
        match name.as_str() {
            "from" => template = Some(value),
            "label" => label = Some(value),
            "set" => set.push(set_answer(&value)?),
            _ => unreachable!("checked above"),
        }
    }
    let [id]: [String; 1] = positional.try_into().map_err(|_| usage.to_owned())?;
    let template = template.ok_or("workspaces new needs --from TEMPLATE; see 'shimmer workspaces templates'")?;
    Ok(WorkspacesCmd::New { id, template, label, set })
}

// ---------------------------------------------------------------- asking first

/// How the CLI asks "are you sure?". A real terminal, or a scripted answer in tests.
pub trait Prompt {
    /// Is a person there to answer? False in scripts, pipes, and workspace steps calling
    /// `shimmer` through `SHIMMER_SOCKET`: the CLI must never wait on a question nobody sees.
    fn interactive(&self) -> bool;
    /// Ask a yes/no question; anything but y/yes is no.
    fn confirm(&mut self, question: &str) -> bool;
    /// Ask for one line of text; `None` if nothing could be read.
    fn ask(&mut self, _question: &str) -> Option<String> {
        None
    }
}

/// Asks nothing: for answers given with --set only.
struct NoQuestions;

impl Prompt for NoQuestions {
    fn interactive(&self) -> bool {
        false
    }
    fn confirm(&mut self, _question: &str) -> bool {
        false
    }
}

/// The real one: asks on stderr, so `--json` output on stdout stays clean.
pub struct Terminal;

impl Prompt for Terminal {
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn confirm(&mut self, question: &str) -> bool {
        eprint!("{question} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().lock().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }

    fn ask(&mut self, question: &str) -> Option<String> {
        eprint!("{question}");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        match std::io::stdin().lock().read_line(&mut answer) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(answer.trim().to_owned()),
        }
    }
}

/// Decide whether to go ahead with an action that skips safety. `status` is what
/// `workspaces.status` returned, shown so the person knows what they're overriding.
/// `Ok(false)` means they said no.
fn confirm(prompt: &mut dyn Prompt, yes: bool, action: &str, question: &str, id: &str, status: &Value) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !prompt.interactive() {
        return Err(Error::invalid_params(format!(
            "refusing to {action} '{id}' without confirmation; pass --yes to do it anyway"
        )));
    }
    eprintln!("{}", summary(status));
    Ok(prompt.confirm(question))
}

// ---------------------------------------------------------------- running

pub async fn run(client: &mut Client, cmd: &WorkspacesCmd, json: bool, prompt: &mut dyn Prompt) -> Result<String> {
    let id_params = |id: &str| json!({"id": id});
    let out = |data: &Value, text: String| if json { render::json(data) } else { text };
    match cmd {
        WorkspacesCmd::Help => Ok(USAGE.into()),
        WorkspacesCmd::List => {
            let data = client.call("workspaces.list", json!({})).await?;
            Ok(out(&data, list(&data)))
        }
        WorkspacesCmd::Status { id } => {
            let data = client.call("workspaces.status", id_params(id)).await?;
            Ok(out(&data, status(&data)))
        }
        WorkspacesCmd::Activate { id, wait } => queued(client, Queued::Activate, id, *wait, json).await,
        WorkspacesCmd::Cleanup { id, wait } => queued(client, Queued::Cleanup, id, *wait, json).await,
        WorkspacesCmd::Stop { id, wait } => queued(client, Queued::Stop, id, *wait, json).await,
        WorkspacesCmd::Edit { id, file, path_only } => {
            // Through the daemon first: an unknown workspace is its `not_found`, not a new file.
            client.call("workspaces.status", id_params(id)).await?;
            let path = edit_path(&shimmer_proto::paths::shimmer_home(), id, file.as_deref())?;
            if *path_only {
                return Ok(path.display().to_string());
            }
            if !prompt.interactive() {
                return Err(Error::invalid_params(format!(
                    "workspaces edit needs a terminal; edit the file directly: {}",
                    path.display()
                )));
            }
            editor::open(&path)?;
            // The daemon reads the file fresh on every request, so this checks what was saved.
            let data = client.call("workspaces.status", id_params(id)).await?;
            Ok(out(&data, edited(id, &data)))
        }
        WorkspacesCmd::Remove { id } => {
            let data = client.call("workspaces.remove", id_params(id)).await?;
            Ok(out(&data, format!("removed {id} (undo: shimmer workspaces restore {id})")))
        }
        WorkspacesCmd::Restore { id } => {
            let data = client.call("workspaces.restore", id_params(id)).await?;
            Ok(out(&data, format!("✓ {id} restored")))
        }
        WorkspacesCmd::ForceRelaunch { id, yes, wait } => {
            if !*yes {
                let current = client.call("workspaces.status", id_params(id)).await?;
                let question = "Launch it anyway, without cleaning up?";
                if !confirm(prompt, false, "force a relaunch of", question, id, &current)? {
                    return Ok(format!("{id} was not relaunched"));
                }
            }
            queued(client, Queued::ForceRelaunch, id, *wait, json).await
        }
        WorkspacesCmd::Templates { which } => {
            let data = client.call("workspaces.templates", json!({})).await?;
            let text = templates(&data, which.as_deref())?;
            Ok(out(&data, text))
        }
        WorkspacesCmd::Peek { template, scripts, file } => {
            let data = client.call("workspaces.templates", json!({})).await?;
            let text = peek(&data, template)?;
            if !*scripts && file.is_none() {
                let one = data["templates"].as_array().and_then(|ts| ts.iter().find(|t| t["id"] == template.as_str()));
                return Ok(out(one.unwrap_or(&Value::Null), text));
            }
            let full = client.call("workspaces.templates", json!({"id": template, "files": true})).await?;
            let one = &full["templates"][0];
            match file {
                Some(path) => {
                    let text = template_file(one, path)?;
                    Ok(out(one, text))
                }
                None => Ok(out(one, format!("{text}\n\n{}", scripts_of(one)))),
            }
        }
        WorkspacesCmd::Copy { id, to, set, label } => {
            // Answers to change: --set ones, or at a terminal, asked when the person wants to.
            let wants = !set.is_empty()
                || (prompt.interactive()
                    && prompt.confirm(&format!("{to} starts with {id}'s answers. Change any of them?")));
            let mut values = serde_json::Map::new();
            if wants {
                let current = client.call("workspaces.answers", id_params(id)).await?;
                let questions = with_current_answers(&current);
                values = if set.is_empty() {
                    answers(&questions, set, prompt, &Paths::from_env())?
                } else {
                    answers(&questions, set, &mut NoQuestions, &Paths::from_env())?
                };
                for value in values.values_mut() {
                    if value == "-" {
                        *value = json!("");
                    }
                }
            }
            let mut params = json!({"id": id, "to": to, "values": values});
            if let Some(label) = label {
                params["label"] = json!(label);
            }
            let data = client.call("workspaces.copy", params).await?;
            Ok(out(&data, copied(id, to, &data)))
        }
        WorkspacesCmd::Rename { id, to, move_schedules } => {
            let data = client.call("workspaces.rename", json!({"id": id, "to": to})).await?;
            let mut text = format!("✓ renamed {id} to {to}");
            for note in data["notes"].as_array().into_iter().flatten() {
                let _ = write!(text, "\n  note: {}", note.as_str().unwrap_or_default());
            }
            // Schedules that activate (or stop…) it by its old name would now fail: say so, and
            // move them only when the person says yes.
            let old = schedules::find(client, "workspaces.", &schedules::names(&[("id", id)])).await;
            let rename = schedules::Rename {
                from: id,
                to,
                changes: schedules::names(&[("id", to)]),
                undo: format!("shimmer workspaces rename {to} {id}"),
                yes: *move_schedules,
            };
            text.push_str(&schedules::follow_rename(client, prompt, &old, &rename).await);
            Ok(out(&data, text))
        }
        WorkspacesCmd::Reconfigure { id, set } => {
            let current = client.call("workspaces.answers", id_params(id)).await?;
            let questions = with_current_answers(&current);
            if prompt.interactive() {
                eprintln!(
                    "{id}'s answers ({}). Enter keeps an answer; - clears an optional one.",
                    cell(&current["template"])
                );
            }
            let mut values = answers(&questions, set, prompt, &Paths::from_env())?;
            for value in values.values_mut() {
                if value == "-" {
                    *value = json!("");
                }
            }
            let data = client
                .call("workspaces.reconfigure", json!({"id": id, "values": values}))
                .await
                .map_err(|e| how_to_answer(e, prompt))?;
            Ok(out(&data, reconfigured(id, &data)))
        }
        WorkspacesCmd::New { id, template, label, set } => {
            let all = client.call("workspaces.templates", json!({})).await?;
            let questions = all["templates"]
                .as_array()
                .and_then(|ts| ts.iter().find(|t| t["id"] == template.as_str()))
                .map(|t| t["questions"].clone())
                .unwrap_or(Value::Null);
            let values = answers(&questions, set, prompt, &Paths::from_env())?;
            let mut params = json!({"id": id, "template": template, "values": values});
            if let Some(label) = label {
                params["label"] = json!(label);
            }
            let data = client.call("workspaces.create", params).await.map_err(|e| how_to_answer(e, prompt))?;
            Ok(out(&data, created(id, template)))
        }
        WorkspacesCmd::Reset { id, yes } => {
            if !*yes {
                let current = client.call("workspaces.status", id_params(id)).await?;
                // Say what's at stake: whatever the failed launch started keeps running, and nothing
                // will stop it later.
                let question = "Reset it to ready without running its cleanup script? Anything its \
                                launch already started (a dev server, a VM, windows) keeps running, \
                                and stop won't know about it";
                if !confirm(prompt, false, "reset", question, id, &current)? {
                    return Ok(format!("{id} was not reset"));
                }
            }
            let data = client.call("workspaces.reset", id_params(id)).await?;
            Ok(out(&data, format!("✓ {id} reset to ready")))
        }
    }
}

/// The three queued ops, for what they print.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Queued {
    Activate,
    Cleanup,
    Stop,
    ForceRelaunch,
}

impl Queued {
    fn op(self) -> &'static str {
        match self {
            Self::Activate => "workspaces.activate",
            Self::Cleanup => "workspaces.cleanup",
            Self::Stop => "workspaces.stop",
            Self::ForceRelaunch => "workspaces.force_relaunch",
        }
    }

    fn what(self) -> &'static str {
        match self {
            Self::Activate => "launch",
            Self::Cleanup => "cleanup",
            Self::Stop => "stop",
            Self::ForceRelaunch => "forced relaunch",
        }
    }

    fn done(self, id: &str) -> String {
        match self {
            Self::Activate | Self::ForceRelaunch => format!("✓ {id} is active"),
            Self::Cleanup => format!("✓ {id} is cleaned up and ready"),
            Self::Stop => format!("✓ {id} is stopped (ready)"),
        }
    }
}

/// Send a queued op; with `wait`, follow its task to the end.
async fn queued(client: &mut Client, kind: Queued, id: &str, wait: bool, json: bool) -> Result<String> {
    if wait {
        // Before the request, so not even the first event can be missed.
        client.subscribe(&["queue.task.*"]).await?;
    }
    let handle = client.call(kind.op(), json!({"id": id})).await?;
    let task = handle["task_id"].as_str().ok_or_else(|| Error::internal("no task_id in the queued response"))?;
    if !wait {
        return Ok(if json { render::json(&handle) } else { queued_message(kind, id, task) });
    }
    match follow(client, task, json).await {
        Ok(result) => Ok(if json { render::json(&result) } else { with_log(kind.done(id), &result) }),
        Err(e) if json => Err(e),
        Err(e) => Err(explain(e, id)),
    }
}

/// The most log lines [`with_log`] prints.
const SHOWN_LOG_LINES: usize = 40;

/// After stop or cleanup: what the cleanup script said it did, e.g. "closed: the kitty terminal",
/// "left open, close it yourself: the cursor window …" (ADR 0026). After activate: the last
/// step's log when that step is supervised, e.g. update-everything's summary (ADR 0025). Read
/// from its log in the Shimmer folder; nothing extra when there's no log or it can't be read.
fn with_log(done: String, result: &Value) -> String {
    let Some(log) = result["log"].as_str().filter(|l| !l.is_empty()) else { return done };
    let Ok(text) = std::fs::read_to_string(shimmer_proto::paths::shimmer_home().join(log)) else { return done };
    let lines = shown_log(&text, log);
    if lines.is_empty() {
        done
    } else {
        format!("{done}\n{}", lines.join("\n"))
    }
}

/// A log's non-empty lines, indented. Only the last [`SHOWN_LOG_LINES`]: the end is what matters
/// (a summary, what was closed), and a long setup log isn't reprinted.
fn shown_log(text: &str, log: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().filter(|l| !l.trim().is_empty()).map(|l| format!("  {l}")).collect();
    if lines.len() > SHOWN_LOG_LINES {
        lines.drain(..lines.len() - SHOWN_LOG_LINES);
        lines.insert(0, format!("  … (the whole log: {log})"));
    }
    lines
}

fn queued_message(kind: Queued, id: &str, task: &str) -> String {
    format!("queued the {} of {id} (task {task})\ncheck on it with: shimmer workspaces status {id}", kind.what())
}

/// Follow task `task` over the subscription stream until it ends: its result, or the error it
/// failed with (e.g. `workspace_dirty`, with its detail). Progress notes go to stderr.
async fn follow(client: &mut Client, task: &str, json: bool) -> Result<Value> {
    let mut last_note = String::new();
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
            "queue.task.progress" => {
                let note = ev.payload["note"].as_str().unwrap_or_default();
                if !json && !note.is_empty() && note != last_note {
                    eprintln!("  {note} …");
                    last_note = note.to_owned();
                }
            }
            "queue.task.finished" => return Ok(ev.payload["result"].clone()),
            "queue.task.failed" => return Err(task_error(&ev.payload)),
            "queue.task.cancelled" => return Err(cancelled()),
            _ => {}
        }
    }
}

/// After `core.stream.lagged`: has the task ended? `None` while it is still queued or running.
async fn finished_task(client: &mut Client, task: &str) -> Result<Option<Result<Value>>> {
    let t = client.call("queue.task", json!({"task_id": task})).await?;
    Ok(match t["status"].as_str() {
        Some("succeeded") => Some(Ok(t["result"].clone())),
        Some("failed") => {
            Some(Err(Error::new(ErrorCode::ModuleError, t["error"].as_str().unwrap_or("the task failed").to_owned())))
        }
        Some("cancelled") => Some(Err(cancelled())),
        _ => None,
    })
}

/// The error a `queue.task.failed` event carries, rebuilt as the daemon sent it.
fn task_error(payload: &Value) -> Error {
    let code: ErrorCode = serde_json::from_value(payload["code"].clone()).unwrap_or(ErrorCode::ModuleError);
    let mut e = Error::new(code, payload["error"].as_str().unwrap_or("the task failed").to_owned());
    if let Some(detail) = payload.get("detail").filter(|d| !d.is_null()) {
        e = e.with_detail(detail.clone());
    }
    e
}

fn cancelled() -> Error {
    Error::new(ErrorCode::ModuleError, "the task was cancelled")
}

/// For people: a dirty workspace's error says where to look and what to do (§10.3), instead
/// of raw JSON detail. Other errors are shown as they are.
fn explain(e: Error, id: &str) -> Error {
    if e.code != ErrorCode::WorkspaceDirty {
        return e;
    }
    let detail = e.detail.clone().unwrap_or(Value::Null);
    let mut message = e.message.clone();
    if let Some(log) = detail["log"].as_str().filter(|l| !l.is_empty()) {
        // Why it failed is the end of the step's log (a template's check step says it in one line).
        if let Ok(text) = std::fs::read_to_string(shimmer_proto::paths::shimmer_home().join(log)) {
            for line in shown_log(&text, log) {
                let _ = write!(message, "\n{line}");
            }
        }
        let _ = write!(message, "\n  log: {log}");
    }
    let _ = write!(message, "\n{}", ways_out(id, detail["has_cleanup_script"] == true));
    Error::new(ErrorCode::WorkspaceDirty, message)
}

// ---------------------------------------------------------------- edit

/// The file to edit: `FILE` (default `workspace.toml`) inside `data/workspaces/<id>/` in the
/// Shimmer folder, the layout CLAUDE.md §7 documents. `FILE` must stay inside that folder.
fn edit_path(home: &std::path::Path, id: &str, file: Option<&str>) -> Result<std::path::PathBuf> {
    let file = file.unwrap_or("workspace.toml");
    let inside = !file.is_empty()
        && !file.starts_with('/')
        && std::path::Path::new(file).components().all(|c| matches!(c, std::path::Component::Normal(_)));
    if !inside {
        return Err(Error::invalid_params(format!(
            "'{file}' must be a file inside the workspace folder, like workspace.toml or steps/01-check.sh"
        )));
    }
    Ok(home.join("data/workspaces").join(id).join(file))
}

/// After editing: fine, or exactly what's wrong now.
fn edited(id: &str, data: &Value) -> String {
    match data["state"].as_str() {
        Some("invalid") => format!(
            "✗ {id} is now invalid: {}\n  fix it with: shimmer workspaces edit {id}",
            data["error"].as_str().unwrap_or("unknown problem")
        ),
        _ => format!("✓ {id} saved; it takes effect the next time you activate it"),
    }
}

// ---------------------------------------------------------------- templates (ADR 0025)

/// Where `~` and relative paths point: only the client knows (ADR 0025 §8).
struct Paths {
    home: Option<String>,
    cwd: Option<String>,
}

impl Paths {
    fn from_env() -> Self {
        Self { home: std::env::var("HOME").ok(), cwd: std::env::current_dir().ok().map(|d| d.display().to_string()) }
    }

    /// `~/x` → `$HOME/x`, `x` → `$PWD/x`; absolute paths and empty answers as they are.
    fn absolute(&self, answer: &str) -> String {
        let a = answer.trim();
        let joined = |base: &Option<String>, rest: &str| match base {
            Some(b) if rest.is_empty() => b.clone(),
            Some(b) => format!("{}/{rest}", b.trim_end_matches('/')),
            None => a.to_owned(),
        };
        if a.is_empty() || a.starts_with('/') {
            a.to_owned()
        } else if a == "~" || a.starts_with("~/") {
            joined(&self.home, a.trim_start_matches('~').trim_start_matches('/'))
        } else {
            joined(&self.cwd, a.trim_start_matches("./"))
        }
    }
}

/// The answers to send: `--set` ones first, then, at a terminal, each question not answered yet
/// (an empty answer takes the template's default). Without a terminal nothing is asked
/// (CLAUDE.md §15.1); the daemon then names any required answer that is missing.
fn answers(
    questions: &Value,
    set: &[(String, String)],
    prompt: &mut dyn Prompt,
    paths: &Paths,
) -> Result<serde_json::Map<String, Value>> {
    let kind_of = |name: &str| {
        questions.as_array().and_then(|qs| qs.iter().find(|q| q["name"] == name)).and_then(|q| q["kind"].as_str())
    };
    let fix = |name: &str, answer: &str| match kind_of(name) {
        Some("folder" | "file") => paths.absolute(answer),
        _ => answer.trim().to_owned(),
    };
    let mut out = serde_json::Map::new();
    for (name, answer) in set {
        out.insert(name.clone(), json!(fix(name, answer)));
    }
    if !prompt.interactive() {
        return Ok(out);
    }
    for q in questions.as_array().into_iter().flatten() {
        let Some(name) = q["name"].as_str() else { continue };
        if out.contains_key(name) {
            continue;
        }
        let mut question = q["prompt"].as_str().unwrap_or(name).to_owned();
        if let Some(choices) = q["choices"].as_array() {
            let list: Vec<&str> = choices.iter().filter_map(Value::as_str).collect();
            let _ = write!(question, " ({})", list.join(", "));
        }
        match q["default"].as_str() {
            Some(d) => {
                let _ = write!(question, " [{d}]");
            }
            None if q["required"] != true => question.push_str(" [none]"),
            None => {}
        }
        question.push_str(": ");
        if let Some(help) = q["help"].as_str() {
            question = format!(
                "  {help}
{question}"
            );
        }
        loop {
            let Some(answer) = prompt.ask(&question) else { return Ok(out) };
            if !answer.is_empty() {
                out.insert(name.to_owned(), json!(fix(name, &answer)));
                break;
            }
            if q["required"] != true {
                break;
            }
            eprintln!("  this one is required");
        }
    }
    Ok(out)
}

/// Without a terminal, a missing required answer should say how to give it.
fn how_to_answer(e: Error, prompt: &dyn Prompt) -> Error {
    if e.code == ErrorCode::InvalidParams && e.message.contains(" is required") && !prompt.interactive() {
        let name = e.message.split(' ').next().unwrap_or("QUESTION");
        return Error::new(
            e.code,
            format!(
                "{}
  answer it with --set {name}=…",
                e.message
            ),
        );
    }
    e
}

/// The scripts that run, in order: each step's, then cleanup's, then the other files by name.
/// `t` is a template with its `files` (`workspaces.templates` with `files: true`).
fn scripts_of(t: &Value) -> String {
    let id = t["id"].as_str().unwrap_or_default();
    let files = t["files"].as_array().map(Vec::as_slice).unwrap_or_default();
    let steps = t["steps"].as_array().map(Vec::as_slice).unwrap_or_default();
    let mut shown: Vec<&str> = Vec::new();
    let mut out = String::from("Scripts, exactly as a workspace made from it gets them (yours to edit after)\n");
    let show = |out: &mut String, heading: String, file: &Value| {
        let _ = write!(out, "\n── {heading} ──\n{}\n", file["text"].as_str().unwrap_or_default().trim_end());
    };
    for (i, step) in steps.iter().enumerate() {
        let name = step["name"].as_str().unwrap_or_default();
        // steps/NN-NAME.sh (ADR 0012); a template ships the scripts for its platforms.
        let prefix = format!("steps/{:02}-{name}.", i + 1);
        for file in files.iter().filter(|f| f["path"].as_str().is_some_and(|p| p.starts_with(&prefix))) {
            let path = file["path"].as_str().unwrap_or_default();
            shown.push(path);
            show(&mut out, format!("step {}/{}: {name} · {} · {path}", i + 1, steps.len(), how_it_runs(step)), file);
        }
    }
    for file in files.iter().filter(|f| f["path"].as_str().is_some_and(|p| p.starts_with("cleanup."))) {
        let path = file["path"].as_str().unwrap_or_default();
        shown.push(path);
        show(&mut out, format!("when you stop it, or clean up a failed launch · {path}"), file);
    }
    let others: Vec<&str> = files.iter().filter_map(|f| f["path"].as_str()).filter(|p| !shown.contains(p)).collect();
    if !others.is_empty() {
        let _ = write!(out, "\nAlso in it: {}\nsee one: shimmer workspaces peek {id} --file PATH", others.join(", "));
    }
    out
}

/// One file of a template, as it is; `not_found` naming the files there are.
fn template_file(t: &Value, path: &str) -> Result<String> {
    let files = t["files"].as_array().map(Vec::as_slice).unwrap_or_default();
    match files.iter().find(|f| f["path"] == path) {
        Some(f) => Ok(f["text"].as_str().unwrap_or_default().trim_end().to_owned()),
        None => {
            let paths: Vec<&str> = files.iter().filter_map(|f| f["path"].as_str()).collect();
            Err(Error::not_found(format!(
                "{} has no file '{path}' (it has: {})",
                t["id"].as_str().unwrap_or_default(),
                paths.join(", ")
            )))
        }
    }
}

/// "30 s", "3 min", "1 h": a step's time limit as people say it.
fn duration(seconds: i64) -> String {
    match seconds {
        s if s >= 3600 && s % 3600 == 0 => format!("{} h", s / 3600),
        s if s >= 60 && s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

/// How a step runs, in words: a supervised step is waited on, a detached one left running.
fn how_it_runs(step: &Value) -> String {
    match (step["mode"].as_str(), step["timeout_s"].as_i64()) {
        (Some("supervised"), Some(t)) => format!("waits, up to {}", duration(t)),
        (Some("supervised"), None) => "waits".to_owned(),
        _ => "starts, keeps running".to_owned(),
    }
}

/// `workspaces.templates`, grouped by category; with `which`, one category or one template in full.
fn templates(data: &Value, which: Option<&str>) -> Result<String> {
    render::templates_by_category(
        data,
        which,
        "workspaces",
        ("make a workspace from it:", "shimmer workspaces new NAME --from TEMPLATE"),
        template_in_full,
    )
}

/// `peek TEMPLATE`: everything about one template before making one from it.
fn peek(data: &Value, id: &str) -> Result<String> {
    Ok(template_in_full(render::template_named(data, id, "workspaces")?, render::categories(data)))
}

/// One template: what it does, what to know, what it needs, its steps in order, what stop does,
/// and each question with its choices, default and help.
fn template_in_full(t: &Value, categories: &[Value]) -> String {
    let id = t["id"].as_str().unwrap_or_default();
    let list = |key: &str| t[key].as_array().cloned().unwrap_or_default();
    let category = categories.iter().find(|c| c["id"] == t["category"]).map(|c| cell(&c["label"]));
    let mut out = format!("{id}: {}", cell(&t["label"]));
    if let Some(category) = category {
        let _ = write!(out, "  ({category})");
    }
    let _ = writeln!(out, "\n  {}", wrap(t["description"].as_str().unwrap_or_default(), 88, "  "));

    for (heading, key) in [("Good to know", "good_to_know"), ("Needs", "needs")] {
        let items = list(key);
        if items.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n{heading}");
        for item in &items {
            let _ = writeln!(out, "  • {}", wrap(item.as_str().unwrap_or_default(), 86, "    "));
        }
    }

    let steps = list("steps");
    if !steps.is_empty() {
        let _ = writeln!(out, "\nWhen you activate it ({} steps, one after another)", steps.len());
        let name_width = steps.iter().map(|s| cell(&s["name"]).chars().count()).max().unwrap_or(0);
        let how_width = steps.iter().map(|s| how_it_runs(s).chars().count()).max().unwrap_or(0);
        for (i, step) in steps.iter().enumerate() {
            let name = step["name"].as_str().unwrap_or_default();
            let line = format!("  {:>2}. {name:<name_width$}  {:<how_width$}  ", i + 1, how_it_runs(step));
            let indent = " ".repeat(line.chars().count());
            let width = 100usize.saturating_sub(indent.len()).max(30);
            let what = wrap(step["description"].as_str().unwrap_or_default(), width, &indent);
            let _ = writeln!(out, "{}", format!("{line}{what}").trim_end());
        }
    }
    if let Some(stop) = t["on_stop"].as_str() {
        let _ = writeln!(out, "\nWhen you stop it\n  {}", wrap(stop, 88, "  "));
    }

    let questions = list("questions");
    if questions.is_empty() {
        out.push_str("\nNo questions\n");
    } else {
        out.push_str("\nQuestions (asked when you make one; or answer with --set NAME=ANSWER)\n");
        let width = questions.iter().map(|q| cell(&q["name"]).chars().count()).max().unwrap_or(0);
        let pad = " ".repeat(width + 4);
        for q in &questions {
            let name = q["name"].as_str().unwrap_or_default();
            let _ = writeln!(out, "  {name:<width$}  {}", q["prompt"].as_str().unwrap_or_default());
            let mut facts = Vec::new();
            if let Some(choices) = q["choices"].as_array() {
                facts.push(choices.iter().map(|c| c.as_str().unwrap_or_default()).collect::<Vec<_>>().join(" | "));
            } else {
                facts.push(q["kind"].as_str().unwrap_or_default().to_owned());
            }
            if q["required"] == true {
                facts.push("required".into());
            }
            if let Some(d) = q["default"].as_str() {
                facts.push(format!("default: {d}"));
            }
            let _ = writeln!(out, "{pad}{}", facts.join("   "));
            if let Some(help) = q["help"].as_str() {
                let _ = writeln!(out, "{pad}{}", wrap(help, 84, &pad));
            }
        }
    }
    let _ = write!(out, "\nmake one: shimmer workspaces new NAME --from {id}");
    out
}

/// The template's questions with each current answer as its default, so Enter keeps it.
fn with_current_answers(current: &Value) -> Value {
    let mut questions = current["questions"].clone();
    for q in questions.as_array_mut().into_iter().flatten() {
        let name = q["name"].as_str().unwrap_or_default().to_owned();
        match current["answers"][&name].as_str() {
            Some(now) if !now.is_empty() => q["default"] = json!(now),
            // Answered with nothing: shown as [none], and Enter keeps it that way.
            Some(_) => {
                if let Some(q) = q.as_object_mut() {
                    q.remove("default");
                }
            }
            None => {}
        }
    }
    questions
}

/// What copy made: the answers that differ, or a warning that none do.
fn copied(id: &str, to: &str, data: &Value) -> String {
    let changed = data["changed"].as_array().map(Vec::as_slice).unwrap_or_default();
    let shown = |v: &Value| match v.as_str() {
        Some("") | None => "(none)".to_owned(),
        Some(s) => s.to_owned(),
    };
    let mut out = format!("✓ copied {id} to {to}");
    for c in changed {
        let _ = write!(out, "\n  {}: {} → {}", cell(&c["name"]), shown(&c["from"]), shown(&c["to"]));
    }
    if changed.is_empty() {
        let _ = write!(
            out,
            "\n  it has the same answers as {id}: running both at once means the same folders and ports.\n  \
             change what should differ with: shimmer workspaces reconfigure {to}"
        );
    }
    let _ = write!(out, "\n  start it: shimmer workspaces activate {to} --wait");
    out
}

/// What reconfigure changed, one line each.
fn reconfigured(id: &str, data: &Value) -> String {
    let changed = data["changed"].as_array().map(Vec::as_slice).unwrap_or_default();
    if changed.is_empty() {
        return format!("nothing changed in {id}");
    }
    let shown = |v: &Value| match v.as_str() {
        Some("") | None => "(none)".to_owned(),
        Some(s) => s.to_owned(),
    };
    let mut out = format!("✓ reconfigured {id}");
    for c in changed {
        let _ = write!(out, "\n  {}: {} → {}", cell(&c["name"]), shown(&c["from"]), shown(&c["to"]));
    }
    out.push_str("\n  it takes effect the next time you activate it");
    out
}

fn created(id: &str, template: &str) -> String {
    [
        format!("✓ created workspace {id} from {template}"),
        format!("  start it: shimmer workspaces activate {id} --wait"),
        format!("  change an answer: shimmer workspaces edit {id}   (the [env] table at the bottom)"),
    ]
    .join("\n")
}

// ---------------------------------------------------------------- output

/// `workspaces.list` as a table. Everything shown comes from the daemon (§2).
pub fn list(data: &Value) -> String {
    let items = data["workspaces"].as_array().map(Vec::as_slice).unwrap_or_default();
    if items.is_empty() {
        return "no workspaces yet: add a folder with a workspace.toml under data/workspaces/ \
                in your Shimmer folder (ADR 0012)"
            .into();
    }
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|w| {
            // The error first: for an invalid workspace, that is what needs fixing.
            let note = w.get("error").or(w.get("dirty_reason")).map(first_line).unwrap_or_default();
            vec![cell(&w["id"]), cell(&w["state"]), cell(&w["label"]), note]
        })
        .collect();
    let mut out = table(&["ID".into(), "STATE".into(), "LABEL".into(), "WHY".into()], &rows);
    if items.iter().any(|w| matches!(w["state"].as_str(), Some("dirty" | "invalid"))) {
        out.push_str("\n\nmore detail: shimmer workspaces status NAME");
    }
    out
}

/// `workspaces.status` in full.
pub fn status(data: &Value) -> String {
    let id = data["id"].as_str().unwrap_or_default();
    let mut out = format!("{id}  ({})", cell(&data["state"]));
    if let Some(label) = data["label"].as_str() {
        let _ = write!(out, "  {label}");
    }
    if let Some(d) = data["description"].as_str() {
        let _ = write!(out, "\n  {d}");
    }
    if let Some(e) = data["error"].as_str() {
        let _ = write!(out, "\n  invalid: {}", e.trim_end().replace('\n', "\n    "));
    }
    if let Some(steps) = data["steps"].as_array() {
        out.push_str("\n  steps:");
        let width = steps.iter().filter_map(|s| s["name"].as_str()).map(str::len).max().unwrap_or(0);
        for s in steps {
            let mode = match s["timeout_s"].as_u64() {
                Some(t) => format!("{}, timeout {t}s", cell(&s["mode"])),
                None => cell(&s["mode"]),
            };
            let _ = write!(out, "\n    {}  {:width$}  {mode}", cell(&s["index"]), cell(&s["name"]));
            if let Some(d) = s["description"].as_str() {
                let _ = write!(out, "  ({d})");
            }
        }
    }
    let _ = match (data["has_cleanup_script"].as_bool(), data["cleanup_timeout_s"].as_u64()) {
        (Some(true), Some(t)) => write!(out, "\n  cleanup: cleanup script, timeout {t}s"),
        (Some(true), None) => write!(out, "\n  cleanup: cleanup script"),
        _ => write!(out, "\n  cleanup: none (reset clears dirty)"),
    };
    if let Some(step) = data["running_step"].as_str() {
        let _ = write!(out, "\n  running: step {step}");
    }
    if let Some(d) = data.get("dirty").filter(|d| d.is_object()) {
        let _ = write!(
            out,
            "\n  dirty: step {}: {} (at {})",
            cell(&d["failed_step"]),
            d["reason"].as_str().unwrap_or_default(),
            cell(&d["failed_at"])
        );
        if let Some(log) = d["log"].as_str().filter(|l| !l.is_empty()) {
            let _ = write!(out, "\n  log: {log}");
        }
    }
    match data.get("last_session").filter(|l| l.is_object()) {
        Some(l) => {
            let forced = if l["forced"] == true { ", forced" } else { "" };
            let _ = write!(out, "\n  last session: {} at {}{forced}", cell(&l["outcome"]), cell(&l["started_at"]));
        }
        None => out.push_str("\n  last session: none yet"),
    }
    if data["state"] == "dirty" {
        let _ = write!(out, "\n{}", ways_out(id, data["has_cleanup_script"] == true));
    }
    out
}

/// What to do about a dirty workspace (§10.3), shown wherever the CLI reports one.
pub fn ways_out(id: &str, has_cleanup_script: bool) -> String {
    let mut out = String::from("  fix it with one of:");
    if has_cleanup_script {
        let _ = write!(out, "\n    shimmer workspaces cleanup {id}          run its cleanup script");
    }
    let _ = write!(
        out,
        "\n    shimmer workspaces force-relaunch {id}   launch it anyway\n    \
         shimmer workspaces reset {id}            mark it ready without cleanup (what it started keeps running)"
    );
    out
}

/// One line about the workspace's state, shown before asking to confirm.
fn summary(status: &Value) -> String {
    let id = status["id"].as_str().unwrap_or_default();
    match status["state"].as_str() {
        Some("dirty") => {
            let reason = status["dirty_reason"].as_str().unwrap_or("failed");
            let mut s = format!("{id} is dirty: {reason}");
            if let Some(log) = status["log"].as_str().filter(|l| !l.is_empty()) {
                let _ = write!(s, "\n  log: {log}");
            }
            s
        }
        Some(state) => format!("{id} is {state}"),
        None => id.to_owned(),
    }
}

fn first_line(v: &Value) -> String {
    let full = v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string());
    cell(&Value::String(full.lines().next().unwrap_or_default().to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str]) -> std::result::Result<WorkspacesCmd, String> {
        parse(words.iter().map(|w| w.to_string()).collect())
    }

    #[test]
    fn commands_parse() {
        assert_eq!(parse_words(&[]).unwrap(), WorkspacesCmd::Help);
        assert_eq!(parse_words(&["help"]).unwrap(), WorkspacesCmd::Help);
        assert_eq!(parse_words(&["list"]).unwrap(), WorkspacesCmd::List);
        assert_eq!(parse_words(&["status", "deep-work"]).unwrap(), WorkspacesCmd::Status { id: "deep-work".into() });
        assert_eq!(
            parse_words(&["reset", "deep-work"]).unwrap(),
            WorkspacesCmd::Reset { id: "deep-work".into(), yes: false }
        );
        for yes in [&["reset", "--yes", "deep-work"][..], &["reset", "deep-work", "-y"]] {
            assert_eq!(parse_words(yes).unwrap(), WorkspacesCmd::Reset { id: "deep-work".into(), yes: true });
        }
        assert_eq!(
            parse_words(&["stop", "site", "--wait"]).unwrap(),
            WorkspacesCmd::Stop { id: "site".into(), wait: true }
        );
        assert_eq!(parse_words(&["remove", "site"]).unwrap(), WorkspacesCmd::Remove { id: "site".into() });
        assert_eq!(parse_words(&["restore", "site"]).unwrap(), WorkspacesCmd::Restore { id: "site".into() });
        assert!(parse_words(&["remove", "site", "--wait"]).unwrap_err().contains("takes no --wait"));
    }

    #[test]
    fn queued_commands_take_wait_and_force_relaunch_takes_yes() {
        assert_eq!(
            parse_words(&["activate", "deep-work"]).unwrap(),
            WorkspacesCmd::Activate { id: "deep-work".into(), wait: false }
        );
        assert_eq!(
            parse_words(&["activate", "--wait", "deep-work"]).unwrap(),
            WorkspacesCmd::Activate { id: "deep-work".into(), wait: true }
        );
        assert_eq!(
            parse_words(&["cleanup", "deep-work", "--wait"]).unwrap(),
            WorkspacesCmd::Cleanup { id: "deep-work".into(), wait: true }
        );
        assert_eq!(
            parse_words(&["force-relaunch", "deep-work", "--yes", "--wait"]).unwrap(),
            WorkspacesCmd::ForceRelaunch { id: "deep-work".into(), yes: true, wait: true }
        );
        for (words, expected) in [
            (&["activate", "x", "--yes"][..], "'workspaces activate' takes no --yes"),
            (&["reset", "x", "--wait"], "'workspaces reset' takes no --wait"),
            (&["status", "x", "--wait"], "'workspaces status' takes no --wait"),
            (&["force_relaunch", "x"], "unknown workspaces command 'force_relaunch'"),
        ] {
            let e = parse_words(words).unwrap_err();
            assert!(e.contains(expected), "{words:?}: {e}");
        }
    }

    #[test]
    fn without_wait_it_points_to_status_never_to_a_second_launch() {
        // Re-running `activate --wait` would queue another launch, so the hint is `status`.
        let m = queued_message(Queued::ForceRelaunch, "deep-work", "01TASK");
        assert_eq!(
            m,
            "queued the forced relaunch of deep-work (task 01TASK)\ncheck on it with: shimmer workspaces status deep-work"
        );
        assert!(!m.contains("--wait"), "{m}");
    }

    #[test]
    fn a_failed_task_keeps_its_code_and_detail() {
        let payload = json!({"task_id": "01", "error": "workspace 'deep-work' failed at step 1/3 setup (exit code 1)",
                             "code": "workspace_dirty",
                             "detail": {"workspace": "deep-work", "log": "logs/x.log", "has_cleanup_script": true}});
        let e = task_error(&payload);
        assert_eq!(e.code, ErrorCode::WorkspaceDirty);
        assert_eq!(e.detail.as_ref().unwrap()["log"], "logs/x.log");

        let other =
            task_error(&json!({"task_id": "01", "error": "no launch backend registered", "code": "unavailable"}));
        assert_eq!((other.code, other.detail), (ErrorCode::Unavailable, None));
        assert_eq!(task_error(&json!({"task_id": "01"})).code, ErrorCode::ModuleError, "unknown code still fails");
    }

    #[test]
    fn a_dirty_failure_is_explained_for_people() {
        let e = task_error(
            &json!({"error": "workspace 'deep-work' failed at step 1/3 setup (exit code 1) and was not cleaned up",
                                    "code": "workspace_dirty",
                                    "detail": {"log": "logs/x.log", "has_cleanup_script": false}}),
        );
        let explained = explain(e, "deep-work");
        assert_eq!(explained.code, ErrorCode::WorkspaceDirty);
        assert_eq!(explained.detail, None, "the detail is in the message now, not as raw JSON");
        assert_eq!(
            explained.message,
            format!(
                "workspace 'deep-work' failed at step 1/3 setup (exit code 1) and was not cleaned up\n  log: logs/x.log\n{}",
                ways_out("deep-work", false)
            )
        );
        // Anything else is passed through untouched.
        let other = explain(Error::unavailable("no launch backend registered"), "deep-work");
        assert_eq!(other.message, "no launch backend registered");
    }

    #[test]
    fn misuse_is_explained() {
        let cases: [(&[&str], &str); 6] = [
            (&["status"], "usage: shimmer workspaces status NAME"),
            (&["status", "a", "b"], "usage: shimmer workspaces status NAME"),
            (&["list", "x"], "'workspaces list' takes no arguments"),
            (&["list", "--yes"], "'workspaces list' takes no --yes"),
            (&["reset", "x", "--force"], "unknown option '--force'"),
            (&["launch", "x"], "unknown workspaces command 'launch'"),
        ];
        for (words, expected) in cases {
            let e = parse_words(words).unwrap_err();
            assert!(e.contains(expected), "{words:?}: {e}");
        }
    }

    fn sample_list() -> Value {
        json!({"workspaces": [
            {"id": "broken", "state": "invalid", "error": "broken/workspace.toml: TOML parse error\n  | …"},
            {"id": "deep-work", "label": "Deep Work", "state": "dirty", "dirty_reason": "step 1/3 setup: exit code 1"},
            {"id": "focus", "label": "Focus", "state": "ready"},
        ]})
    }

    #[test]
    fn list_is_a_table_with_why() {
        let out = list(&sample_list());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "ID         STATE    LABEL      WHY");
        assert_eq!(lines[1], "broken     invalid  -          broken/workspace.toml: TOML parse error");
        assert_eq!(lines[2], "deep-work  dirty    Deep Work  step 1/3 setup: exit code 1");
        assert_eq!(lines[3], "focus      ready    Focus      ");
        assert_eq!(lines.last().unwrap(), &"more detail: shimmer workspaces status NAME");
    }

    #[test]
    fn an_empty_list_says_how_to_add_one() {
        assert!(list(&json!({"workspaces": []})).starts_with("no workspaces yet"));
        let healthy = list(&json!({"workspaces": [{"id": "a", "label": "A", "state": "active"}]}));
        assert!(!healthy.contains("more detail"), "{healthy}");
    }

    #[test]
    fn status_of_a_dirty_workspace_shows_the_failure_and_the_ways_out() {
        let data = json!({
            "id": "deep-work", "label": "Deep Work", "description": "Shimmer on Hyprland", "state": "dirty",
            "steps": [
                {"index": 1, "name": "setup", "mode": "supervised", "timeout_s": 60, "description": "pull and start db"},
                {"index": 2, "name": "editor", "mode": "detached"},
            ],
            "has_cleanup_script": true, "cleanup_timeout_s": 30,
            "dirty_reason": "step 1/2 setup: exit code 1", "log": "logs/deep-work-01.log",
            "dirty": {"reason": "exit code 1", "failed_step": "1/2 setup",
                      "failed_at": "2026-09-16T09:12:44Z", "log": "logs/deep-work-01.log"},
            "last_session": {"id": "01", "started_at": "2026-09-16T09:12:30Z", "forced": true, "outcome": "dirty"},
        });
        assert_eq!(
            status(&data),
            "deep-work  (dirty)  Deep Work
  Shimmer on Hyprland
  steps:
    1  setup   supervised, timeout 60s  (pull and start db)
    2  editor  detached
  cleanup: cleanup script, timeout 30s
  dirty: step 1/2 setup: exit code 1 (at 2026-09-16T09:12:44Z)
  log: logs/deep-work-01.log
  last session: dirty at 2026-09-16T09:12:30Z, forced
  fix it with one of:
    shimmer workspaces cleanup deep-work          run its cleanup script
    shimmer workspaces force-relaunch deep-work   launch it anyway
    shimmer workspaces reset deep-work            mark it ready without cleanup (what it started keeps running)"
        );
    }

    #[test]
    fn status_of_a_new_or_invalid_workspace() {
        let new = json!({"id": "focus", "label": "Focus", "state": "ready", "has_cleanup_script": false,
                         "steps": [{"index": 1, "name": "setup", "mode": "detached"}], "last_session": null});
        let out = status(&new);
        assert!(out.contains("cleanup: none (reset clears dirty)"), "{out}");
        assert!(out.ends_with("last session: none yet"), "{out}");

        let invalid = json!({"id": "broken", "state": "invalid", "has_cleanup_script": false,
                             "error": "broken/workspace.toml: line 1\n  | [workspace\n", "last_session": null});
        let out = status(&invalid);
        assert!(
            out.starts_with("broken  (invalid)\n  invalid: broken/workspace.toml: line 1\n      | [workspace"),
            "{out}"
        );
    }

    /// A scripted person at the keyboard.
    struct Answer {
        interactive: bool,
        yes: bool,
        asked: Vec<String>,
    }

    impl Prompt for Answer {
        fn interactive(&self) -> bool {
            self.interactive
        }
        fn confirm(&mut self, question: &str) -> bool {
            self.asked.push(question.to_owned());
            self.yes
        }
    }

    fn answer(interactive: bool, yes: bool) -> Answer {
        Answer { interactive, yes, asked: Vec::new() }
    }

    #[test]
    fn confirming_asks_a_person_and_respects_no() {
        let dirty = json!({"id": "deep-work", "state": "dirty", "dirty_reason": "step 1/3 setup: exit code 1"});
        let mut p = answer(true, true);
        assert!(confirm(&mut p, false, "reset", "Reset?", "deep-work", &dirty).unwrap());
        assert_eq!(p.asked, ["Reset?"]);

        let mut p = answer(true, false);
        assert!(!confirm(&mut p, false, "reset", "Reset?", "deep-work", &dirty).unwrap());
    }

    #[test]
    fn yes_skips_the_question_and_scripts_never_wait_on_one() {
        let s = json!({"id": "deep-work", "state": "dirty"});
        let mut p = answer(false, false);
        assert!(confirm(&mut p, true, "reset", "Reset?", "deep-work", &s).unwrap());
        assert!(p.asked.is_empty());

        // No person and no --yes: a clear error, never a hang.
        let e = confirm(&mut p, false, "reset", "Reset?", "deep-work", &s).unwrap_err();
        assert_eq!(e.message, "refusing to reset 'deep-work' without confirmation; pass --yes to do it anyway");
        assert!(p.asked.is_empty());
    }

    #[test]
    fn edit_parses_and_stays_inside_the_folder() {
        assert_eq!(
            parse_words(&["edit", "site"]).unwrap(),
            WorkspacesCmd::Edit { id: "site".into(), file: None, path_only: false }
        );
        assert_eq!(
            parse_words(&["edit", "site", "steps/01-check.sh", "--path"]).unwrap(),
            WorkspacesCmd::Edit { id: "site".into(), file: Some("steps/01-check.sh".into()), path_only: true }
        );
        assert!(parse_words(&["edit"]).unwrap_err().contains("usage"));
        assert!(parse_words(&["status", "site", "--path"]).unwrap_err().contains("unknown option"));

        let home = std::path::Path::new("/home/me/.local/share/shimmer");
        assert_eq!(
            edit_path(home, "site", None).unwrap(),
            std::path::Path::new("/home/me/.local/share/shimmer/data/workspaces/site/workspace.toml")
        );
        assert!(edit_path(home, "site", Some("steps/09-terminal.sh")).unwrap().ends_with("site/steps/09-terminal.sh"));
        for bad in ["../records/x.toml", "/etc/passwd", "steps/../../x", ""] {
            assert!(edit_path(home, "site", Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn after_editing_it_says_whether_the_file_still_works() {
        let ok = json!({"id": "site", "state": "ready"});
        assert!(edited("site", &ok).starts_with("✓ site saved"));
        let broken =
            json!({"id": "site", "state": "invalid", "error": "site/workspace.toml: unknown field `tiemout_s`"});
        let out = edited("site", &broken);
        assert!(out.contains("unknown field `tiemout_s`") && out.contains("shimmer workspaces edit site"), "{out}");
    }

    /// A scripted person who types these answers, in order.
    struct Typist {
        lines: Vec<&'static str>,
        asked: Vec<String>,
    }

    impl Prompt for Typist {
        fn interactive(&self) -> bool {
            true
        }
        fn confirm(&mut self, _: &str) -> bool {
            false
        }
        fn ask(&mut self, question: &str) -> Option<String> {
            self.asked.push(question.to_owned());
            (!self.lines.is_empty()).then(|| self.lines.remove(0).to_owned())
        }
    }

    fn paths() -> Paths {
        Paths { home: Some("/home/me".into()), cwd: Some("/home/me/code".into()) }
    }

    #[test]
    fn new_parses_from_label_and_set() {
        let words = [
            "new",
            "site",
            "--from",
            "web-project",
            "--set",
            "PROJECT_DIR=~/code/site",
            "--set=LINKS=https://a.dev b",
            "--label",
            "My site",
        ];
        assert_eq!(
            parse(words.iter().map(|s| s.to_string()).collect()).unwrap(),
            WorkspacesCmd::New {
                id: "site".into(),
                template: "web-project".into(),
                label: Some("My site".into()),
                set: vec![("PROJECT_DIR".into(), "~/code/site".into()), ("LINKS".into(), "https://a.dev b".into())]
            }
        );
        let bad = |w: &[&str]| parse(w.iter().map(|s| s.to_string()).collect()).unwrap_err();
        assert!(bad(&["new", "site"]).contains("needs --from"));
        assert!(bad(&["new", "--from", "x"]).contains("usage"));
        assert!(bad(&["new", "site", "--from", "x", "--set", "nope"]).contains("QUESTION=ANSWER"));
        assert!(bad(&["new", "site", "--from", "x", "--yes"]).contains("not '--yes'"));
        assert_eq!(parse(vec!["templates".into()]).unwrap(), WorkspacesCmd::Templates { which: None });
        assert_eq!(
            parse(vec!["templates".into(), "upkeep".into()]).unwrap(),
            WorkspacesCmd::Templates { which: Some("upkeep".into()) }
        );
        assert!(parse(vec!["templates".into(), "a".into(), "b".into()]).is_err());
        let peek_of = |words: &[&str]| parse(words.iter().map(|w| (*w).to_owned()).collect());
        assert_eq!(
            peek_of(&["peek", "free-disk"]).unwrap(),
            WorkspacesCmd::Peek { template: "free-disk".into(), scripts: false, file: None }
        );
        assert_eq!(
            peek_of(&["peek", "free-disk", "--scripts"]).unwrap(),
            WorkspacesCmd::Peek { template: "free-disk".into(), scripts: true, file: None }
        );
        let file = Some("lib/shimmer-open.sh".to_owned());
        assert_eq!(
            peek_of(&["peek", "--file", "lib/shimmer-open.sh", "free-disk"]).unwrap(),
            WorkspacesCmd::Peek { template: "free-disk".into(), scripts: false, file: file.clone() }
        );
        assert_eq!(
            peek_of(&["peek", "free-disk", "--file=lib/shimmer-open.sh"]).unwrap(),
            WorkspacesCmd::Peek { template: "free-disk".into(), scripts: false, file }
        );
        assert!(peek_of(&["peek"]).is_err());
        assert_eq!(
            peek_of(&["reconfigure", "site", "--set", "CODE_EDITOR=cursor", "--set=LINKS="]).unwrap(),
            WorkspacesCmd::Reconfigure {
                id: "site".into(),
                set: vec![("CODE_EDITOR".into(), "cursor".into()), ("LINKS".into(), String::new())]
            }
        );
        assert!(peek_of(&["reconfigure"]).is_err());
        assert_eq!(
            peek_of(&["rename", "site", "portfolio"]).unwrap(),
            WorkspacesCmd::Rename { id: "site".into(), to: "portfolio".into(), move_schedules: false }
        );
        assert_eq!(
            peek_of(&["rename", "--move-schedules", "site", "portfolio"]).unwrap(),
            WorkspacesCmd::Rename { id: "site".into(), to: "portfolio".into(), move_schedules: true }
        );
        assert!(peek_of(&["rename", "site"]).is_err());
        assert_eq!(
            peek_of(&["copy", "site", "blog", "--set", "PROJECT_DIR=~/code/blog", "--label", "My blog"]).unwrap(),
            WorkspacesCmd::Copy {
                id: "site".into(),
                to: "blog".into(),
                set: vec![("PROJECT_DIR".into(), "~/code/blog".into())],
                label: Some("My blog".into())
            }
        );
        assert_eq!(
            peek_of(&["copy", "site", "blog", "--label=B"]).unwrap(),
            WorkspacesCmd::Copy { id: "site".into(), to: "blog".into(), set: vec![], label: Some("B".into()) }
        );
        assert!(peek_of(&["copy", "site"]).is_err());
        assert!(peek_of(&["copy", "a", "b", "--label"]).unwrap_err().contains("needs a value"));
        assert!(peek_of(&["copy", "a", "b", "--wait"]).unwrap_err().contains("unknown option"));
        assert!(peek_of(&["rename", "a", "b", "c"]).is_err());
        assert!(peek_of(&["rename", "a", "b", "--yes"]).unwrap_err().contains("unknown option '--yes'"));
        assert!(peek_of(&["reconfigure", "a", "b"]).is_err());
        assert!(peek_of(&["reconfigure", "a", "--set", "nope"]).unwrap_err().contains("QUESTION=ANSWER"));
        assert!(peek_of(&["reconfigure", "a", "--wait"]).unwrap_err().contains("takes --set"));
        assert!(peek_of(&["peek", "a", "b"]).is_err());
        assert!(peek_of(&["peek", "a", "--file"]).unwrap_err().contains("needs a path"));
        assert!(peek_of(&["peek", "a", "--scripts", "--file", "x"]).unwrap_err().contains("not both"));
        assert!(peek_of(&["peek", "a", "--nope"]).unwrap_err().contains("unknown option '--nope'"));
    }

    #[test]
    fn paths_become_absolute_on_the_client() {
        let p = paths();
        assert_eq!(p.absolute("~/code/site"), "/home/me/code/site");
        assert_eq!(p.absolute("~"), "/home/me");
        assert_eq!(p.absolute("./site"), "/home/me/code/site");
        assert_eq!(p.absolute("site"), "/home/me/code/site");
        assert_eq!(p.absolute("/srv/site"), "/srv/site");
        assert_eq!(p.absolute(""), "");
    }

    #[test]
    fn answers_asks_what_set_left_and_retries_required_ones() {
        let questions = json!([
            {"name": "PROJECT_DIR", "prompt": "Project folder", "kind": "folder", "required": true},
            {"name": "CODE_EDITOR", "prompt": "Editor", "kind": "choice", "choices": ["vscode", "cursor"], "default": "vscode", "required": false},
            {"name": "LINKS", "prompt": "Links", "kind": "urls", "required": false, "help": "Space-separated."}
        ]);
        let mut p = Typist { lines: vec!["", "~/site", "", "https://me.dev"], asked: Vec::new() };
        let got = answers(&questions, &[], &mut p, &paths()).unwrap();
        assert_eq!(
            Value::Object(got),
            json!({"PROJECT_DIR": "/home/me/site", "LINKS": "https://me.dev"}),
            "empty = the default"
        );
        assert_eq!(p.asked[0], "Project folder: ");
        assert_eq!(p.asked[1], "Project folder: ", "required: asked again");
        assert_eq!(p.asked[2], "Editor (vscode, cursor) [vscode]: ");
        assert_eq!(p.asked[3], "  Space-separated.\nLinks [none]: ");

        let mut p = Typist { lines: vec![], asked: Vec::new() };
        let set = [("PROJECT_DIR".to_owned(), "~/x".to_owned())];
        let got = answers(&questions, &set, &mut p, &paths()).unwrap();
        assert_eq!(got["PROJECT_DIR"], "/home/me/x");
        assert_eq!(p.asked.len(), 1, "stops when nothing more can be read");

        // No terminal: nothing is asked, only --set is sent.
        let mut p = answer(false, false);
        let got = answers(&questions, &set, &mut p, &paths()).unwrap();
        assert_eq!(got.len(), 1);
        let e = how_to_answer(Error::invalid_params("PROJECT_DIR (Project folder) is required"), &p);
        assert!(e.message.ends_with("answer it with --set PROJECT_DIR=…"), "{}", e.message);
    }

    #[test]
    fn templates_and_created_output() {
        let data = json!({
        "categories": [{"id": "code", "label": "Coding"}, {"id": "testing", "label": "Testing Shimmer"}],
        "templates": [
            {"id": "web-project", "label": "Web project", "category": "code", "description": "Editor and dev server",
             "good_to_know": ["Pulls on open"], "needs": ["An editor"], "on_stop": "Stops the dev server.",
             "steps": [{"name": "check", "mode": "supervised", "timeout_s": 30, "description": "Checks"},
                       {"name": "editor", "mode": "detached", "description": "Opens the editor"}],
             "questions": [{"name": "PROJECT_DIR", "prompt": "Project folder", "kind": "folder", "required": true},
                           {"name": "MODE", "prompt": "Mode", "kind": "choice", "choices": ["a", "b"], "default": "a",
                            "required": false, "help": "a: one way."}]},
            {"id": "smoke-test", "label": "Smoke test", "category": "testing", "description": "Opens nothing",
             "questions": []},
        ]});
        let list = templates(&data, None).unwrap();
        assert!(
            list.starts_with(
                "Coding (code)\n  web-project  Editor and dev server\n\nTesting Shimmer (testing)\n  smoke-test   Opens nothing\n\n"
            ),
            "{list}"
        );
        assert!(list.contains("shimmer workspaces peek TEMPLATE"), "{list}");
        let only = templates(&data, Some("testing")).unwrap();
        assert!(only.starts_with("Testing Shimmer (testing)\n  smoke-test   Opens nothing\n"), "{only}");
        assert!(!only.contains("web-project"), "{only}");
        let full = peek(&data, "web-project").unwrap();
        assert_eq!(templates(&data, Some("web-project")).unwrap(), full, "templates TEMPLATE is peek");
        assert!(full.contains("\nGood to know\n  • Pulls on open\n\nNeeds\n  • An editor\n"), "{full}");
        assert!(
            full.contains(
                "\nWhen you activate it (2 steps, one after another)\n   1. check   waits, up to 30 s      Checks\n   2. editor  starts, keeps running  Opens the editor\n"
            ),
            "{full}"
        );
        assert!(full.contains("\nWhen you stop it\n  Stops the dev server.\n"), "{full}");
        let e = peek(&data, "testing").unwrap_err();
        assert!(e.message.contains("'testing' is a category"), "{}", e.message);
        let e = peek(&data, "nope").unwrap_err();
        assert!(e.message.contains("no template 'nope' (there are: web-project, smoke-test)"), "{}", e.message);
        assert_eq!([duration(30), duration(180), duration(3600), duration(90)], ["30 s", "3 min", "1 h", "90 s"]);

        let mut with_files = data["templates"][0].clone();
        with_files["files"] = json!([
            {"path": "workspace.toml", "text": "[workspace]\n"},
            {"path": "steps/01-check.sh", "text": "echo checking\n"},
            {"path": "steps/02-editor.sh", "text": "code .\n"},
            {"path": "cleanup.sh", "text": "echo bye\n"},
            {"path": "lib/shimmer-open.sh", "text": "helpers\n"},
        ]);
        let scripts = scripts_of(&with_files);
        assert!(
            scripts.contains("\n── step 1/2: check · waits, up to 30 s · steps/01-check.sh ──\necho checking\n"),
            "{scripts}"
        );
        assert!(
            scripts.contains("\n── step 2/2: editor · starts, keeps running · steps/02-editor.sh ──\ncode .\n"),
            "{scripts}"
        );
        assert!(
            scripts.contains("── when you stop it, or clean up a failed launch · cleanup.sh ──\necho bye\n"),
            "{scripts}"
        );
        assert!(scripts.ends_with("Also in it: workspace.toml, lib/shimmer-open.sh\nsee one: shimmer workspaces peek web-project --file PATH"), "{scripts}");
        assert_eq!(template_file(&with_files, "lib/shimmer-open.sh").unwrap(), "helpers");
        let e = template_file(&with_files, "nope.sh").unwrap_err();
        assert!(
            e.message.contains("web-project has no file 'nope.sh' (it has: workspace.toml, steps/01-check.sh"),
            "{}",
            e.message
        );
        assert!(full.starts_with("web-project: Web project  (Coding)\n  Editor and dev server\n"), "{full}");
        assert!(full.contains("  PROJECT_DIR  Project folder\n               folder   required\n"), "{full}");
        assert!(
            full.contains("  MODE         Mode\n               a | b   default: a\n               a: one way.\n"),
            "{full}"
        );
        assert!(full.ends_with("make one: shimmer workspaces new NAME --from web-project"), "{full}");
        let e = templates(&data, Some("games")).unwrap_err();
        assert!(e.message.contains("no template or category 'games' (categories: code, testing"), "{}", e.message);
        assert!(created("site", "web-project")
            .starts_with("✓ created workspace site from web-project\n  start it: shimmer workspaces activate site"));
    }

    #[test]
    fn ways_out_only_offers_cleanup_when_there_is_a_script() {
        assert!(ways_out("deep-work", true).contains("workspaces cleanup deep-work"));
        let without = ways_out("deep-work", false);
        assert!(!without.contains("cleanup deep-work"), "{without}");
        assert!(without.contains("force-relaunch deep-work") && without.contains("reset deep-work"), "{without}");
    }

    #[test]
    fn the_summary_says_what_is_being_overridden() {
        let dirty = json!({"id": "deep-work", "state": "dirty", "dirty_reason": "step 1/3 setup: exit code 1",
                           "log": "logs/x.log"});
        assert_eq!(summary(&dirty), "deep-work is dirty: step 1/3 setup: exit code 1\n  log: logs/x.log");
        assert_eq!(summary(&json!({"id": "focus", "state": "ready"})), "focus is ready");
    }

    #[test]
    fn a_long_log_shows_only_its_end() {
        let text: String = (1..=45).map(|i| format!("line {i}\n\n")).collect();
        let shown = shown_log(&text, "logs/x.log");
        assert_eq!(shown.len(), 41);
        assert_eq!(shown[0], "  … (the whole log: logs/x.log)");
        assert_eq!(shown[1], "  line 6");
        assert_eq!(shown[40], "  line 45");
        assert_eq!(shown_log("a\n\nb\n", "l"), ["  a", "  b"]);
    }

    #[test]
    fn reconfigure_asks_with_the_current_answers_and_says_what_changed() {
        let current = json!({"template": "web-project", "questions": [
            {"name": "CODE_EDITOR", "prompt": "Editor", "kind": "choice", "choices": ["vscode", "cursor"], "default": "vscode"},
            {"name": "LINKS", "prompt": "Links", "kind": "urls"},
            {"name": "NEW_ONE", "prompt": "New", "kind": "text", "default": "auto"}],
            "answers": {"CODE_EDITOR": "cursor", "LINKS": "", "NEW_ONE": null}});
        let q = with_current_answers(&current);
        assert_eq!(q[0]["default"], "cursor", "the current answer");
        assert!(q[1].get("default").is_none(), "answered with nothing: no default");
        assert_eq!(q[2]["default"], "auto", "a question it has no answer to yet keeps the template's default");

        let data = json!({"changed": [{"name": "CODE_EDITOR", "from": "vscode", "to": "cursor"},
                                      {"name": "LINKS", "from": "https://a.dev", "to": ""}]});
        assert_eq!(
            reconfigured("site", &data),
            "✓ reconfigured site\n  CODE_EDITOR: vscode → cursor\n  LINKS: https://a.dev → (none)\n  it takes effect the next time you activate it"
        );
        assert_eq!(reconfigured("site", &json!({"changed": []})), "nothing changed in site");
    }

    #[test]
    fn copy_says_what_differs_or_warns_that_nothing_does() {
        let changed = json!({"changed": [{"name": "PROJECT_DIR", "from": "/a", "to": "/b"}]});
        assert_eq!(
            copied("site", "blog", &changed),
            "✓ copied site to blog\n  PROJECT_DIR: /a → /b\n  start it: shimmer workspaces activate blog --wait"
        );
        let same = copied("site", "blog", &json!({"changed": []}));
        assert!(same.contains("same answers as site") && same.contains("reconfigure blog"), "{same}");
    }
}
