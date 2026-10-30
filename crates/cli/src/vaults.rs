//! `shimmer vaults …` and `--vault NAME` (ADR 0041): named Shimmer folders you can switch between.
//!
//! A vault is a `$SHIMMER_HOME` with a name; `main` is the platform's default folder. Nothing in the
//! daemon, the protocol or any module knows vaults exist: [`enter`] picks the folder once, before
//! anything else runs, by re-running this binary with `SHIMMER_HOME` and `SHIMMER_SOCKET` set, so
//! every other part of the CLI sees one `$SHIMMER_HOME`, as it always has (§3). The vaults themselves
//! are kept in the CLI's own `cli.toml` (§4), never in a Shimmer folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{json, Value};
use shimmer_core::{Error, Result};
use shimmer_proto::{ops, paths};

use crate::client::Client;
use crate::packs::settings::{self, vault_name_problem, Settings, Vault};
use crate::render::{self, table};
use crate::style::{paint, Role};
use crate::workspaces::Prompt;

pub const USAGE: &str = "usage: shimmer vaults <command>

commands:
  list [--removed]           every vault: its folder, the one in use, whether its daemon runs;
                             --removed: the ones you removed, to restore or purge
  show VAULT                 one vault in full: folder, socket, daemon, size, made or adopted
  status                     which vault your commands use now, and why
  new NAME [--path DIR]      a new, empty vault, beside your main folder unless --path says
                             where; DIR must be empty, or already a Shimmer folder
  copy VAULT NEW [--path DIR] [--live]
                             a new vault with VAULT's data, to try things on safely.
                             Its schedules, automations and sources start switched off
                             and its notifications paused, so nothing runs twice;
                             --live: leave them as they are
  use VAULT                  make plain commands use it; 'use main' goes back
  rename VAULT NEW           its name only; the folder stays where it is
  remove VAULT               forget it and stop its daemon; its files are kept
  restore REMOVED            remember a removed vault again
  purge [REMOVED] [--yes]    delete removed vaults' folders for good, or one (asks first)

Run one command in another vault: shimmer --vault NAME <command>.
Which vault a command uses: --vault, else SHIMMER_HOME, else the one chosen with
'vaults use', else main. Vaults are kept in ~/.config/shimmer/cli.toml.";

/// The vault that is always there: your default folder.
pub const MAIN: &str = "main";

#[derive(Clone, Debug, PartialEq)]
pub enum VaultsCmd {
    Help,
    List { removed: bool },
    Show { name: String },
    Status,
    New { name: String, path: Option<PathBuf> },
    Copy { name: String, new: String, path: Option<PathBuf>, live: bool },
    Use { name: String },
    Rename { name: String, new: String },
    Remove { name: String },
    Restore { name: String },
    Purge { name: Option<String>, yes: bool },
}

/// `rest` is what follows `shimmer vaults`. Nothing after it is the same as `list`.
pub fn parse(rest: Vec<String>) -> std::result::Result<VaultsCmd, String> {
    let mut words = rest.into_iter();
    let Some(verb) = words.next() else { return Ok(VaultsCmd::List { removed: false }) };
    let (options, switches): (&[&'static str], &[&'static str]) = match verb.as_str() {
        "list" => (&[], &["--removed"]),
        "new" => (&["--path"], &[]),
        "copy" => (&["--path"], &["--live"]),
        "purge" => (&[], &["--yes"]),
        _ => (&[], &[]),
    };
    let flags = crate::flags::Flags::split(words.collect(), options, switches)
        .map_err(|e| format!("{e} for 'vaults {verb}'"))?;
    let args = flags.positional.as_slice();
    let usage = |what: &str| format!("usage: shimmer vaults {verb} {what}");
    match (verb.as_str(), args) {
        ("help", []) => Ok(VaultsCmd::Help),
        ("list", []) => Ok(VaultsCmd::List { removed: flags.has("--removed") }),
        ("status", []) => Ok(VaultsCmd::Status),
        ("show", [name]) => Ok(VaultsCmd::Show { name: name.clone() }),
        ("new", [name]) => Ok(VaultsCmd::New { name: name.clone(), path: flags.value("--path").map(PathBuf::from) }),
        ("copy", [name, new]) => Ok(VaultsCmd::Copy {
            name: name.clone(),
            new: new.clone(),
            path: flags.value("--path").map(PathBuf::from),
            live: flags.has("--live"),
        }),
        ("use", [name]) => Ok(VaultsCmd::Use { name: name.clone() }),
        ("rename", [name, new]) => Ok(VaultsCmd::Rename { name: name.clone(), new: new.clone() }),
        ("remove", [name]) => Ok(VaultsCmd::Remove { name: name.clone() }),
        ("restore", [name]) => Ok(VaultsCmd::Restore { name: name.clone() }),
        ("purge", [] | [_]) => Ok(VaultsCmd::Purge { name: args.first().cloned(), yes: flags.has("--yes") }),
        ("help" | "list" | "status", _) => Err(format!("'vaults {verb}' takes no arguments")),
        ("show" | "use" | "remove", _) => Err(usage("VAULT")),
        ("new", _) => Err(usage("NAME [--path DIR]")),
        ("rename", _) => Err(usage("VAULT NEW")),
        ("copy", _) => Err(usage("VAULT NEW [--path DIR] [--live]")),
        ("restore", _) => Err(usage("REMOVED")),
        ("purge", _) => Err(usage("[REMOVED] [--yes]")),
        (other, _) => Err(crate::suggest::unknown_command(Some("vaults"), other, USAGE)),
    }
}

// ---------------------------------------------------------------- which vault (ADR 0041 §3)

/// Why a command uses the vault it does, in the order they're tried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// `--vault NAME` on the line.
    Flag,
    /// `SHIMMER_HOME` in the environment.
    Env,
    /// Chosen with `shimmer vaults use`.
    Used,
    /// Nothing else said: main.
    Main,
}

impl Why {
    fn key(self) -> &'static str {
        match self {
            Why::Flag => "flag",
            Why::Env => "env",
            Why::Used => "use",
            Why::Main => "main",
        }
    }
}

/// The folder a command uses, its vault's name if it is one, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    /// `None`: `SHIMMER_HOME` names a folder that isn't a vault.
    pub name: Option<String>,
    pub home: PathBuf,
    pub why: Why,
}

/// Which vault a command uses (§3): `--vault`, else `SHIMMER_HOME`, else `vaults use`'s choice,
/// else main. A plain function of its inputs, so it's tested without touching the environment.
pub fn resolve(flag: Option<&str>, env_home: Option<&Path>, settings: &Settings, main: &Path) -> Result<Choice> {
    if let Some(name) = flag {
        let home = path_of(name, settings, main)?;
        return Ok(Choice { name: Some(name.to_owned()), home, why: Why::Flag });
    }
    if let Some(home) = env_home {
        return Ok(Choice { name: name_of(home, settings, main), home: home.to_path_buf(), why: Why::Env });
    }
    match &settings.vault {
        Some(name) => {
            let home = path_of(name, settings, main).map_err(|_| {
                Error::not_found(format!(
                    "cli.toml says to use vault '{name}', which isn't one; go back with: shimmer vaults use main"
                ))
            })?;
            Ok(Choice { name: Some(name.clone()), home, why: Why::Used })
        }
        None => Ok(Choice { name: Some(MAIN.into()), home: main.to_path_buf(), why: Why::Main }),
    }
}

/// A vault's folder, by name.
fn path_of(name: &str, settings: &Settings, main: &Path) -> Result<PathBuf> {
    if name == MAIN {
        return Ok(main.to_path_buf());
    }
    if let Some(v) = settings.vaults.get(name) {
        return Ok(v.path.clone());
    }
    Err(match settings.removed_vaults.contains_key(name) {
        true => {
            Error::not_found(format!("vault '{name}' was removed; bring it back with: shimmer vaults restore {name}"))
        }
        false => Error::not_found(format!("no vault named '{name}' (see 'shimmer vaults list')")),
    })
}

/// The vault whose folder `home` is, if any. Compared resolved (ADR 0039 §1), so a trailing `/` or a
/// symlink still names the same vault.
fn name_of(home: &Path, settings: &Settings, main: &Path) -> Option<String> {
    let home = paths::resolve(home);
    if paths::resolve(main) == home {
        return Some(MAIN.into());
    }
    settings.vaults.iter().find(|(_, v)| paths::resolve(&v.path) == home).map(|(n, _)| n.clone())
}

/// The vault this process's `$SHIMMER_HOME` is, for `doctor` and `startup status` to name (§5).
/// `None` for main, or a folder that isn't a vault: nothing worth saying.
pub fn current_name() -> Option<String> {
    name_for(&paths::shimmer_home())
}

/// The vault whose folder `home` is, by `cli.toml`; `None` for main or a folder that isn't one.
pub(crate) fn name_for(home: &Path) -> Option<String> {
    let saved = settings::path().and_then(|p| settings::load(&p).ok()).unwrap_or_default();
    name_of(home, &saved, &main_home()).filter(|n| n != MAIN)
}

/// The platform's default Shimmer folder, whatever `SHIMMER_HOME` says: what
/// `proto::paths::shimmer_home` gives with it unset (CLAUDE.md §7).
pub fn main_home() -> PathBuf {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    main_home_from(var("HOME"), var("XDG_DATA_HOME"), cfg!(target_os = "macos"))
}

fn main_home_from(home: Option<PathBuf>, xdg_data_home: Option<PathBuf>, macos: bool) -> PathBuf {
    let home = home.unwrap_or_default();
    match (macos, xdg_data_home) {
        (true, _) => home.join("Library/Application Support/shimmer"),
        (false, Some(x)) => x.join("shimmer"),
        (false, None) => home.join(".local/share/shimmer"),
    }
}

/// Where `vaults new NAME` puts a vault without `--path`: beside the main folder, never inside it,
/// since that's the one people sync with git (§5).
fn default_path(name: &str, main: &Path) -> PathBuf {
    main.parent().unwrap_or(main).join("shimmer-vaults").join(name)
}

// ---------------------------------------------------------------- entering a vault (§3)

/// What the binary does before anything else.
#[derive(Debug, PartialEq)]
enum Plan {
    /// Carry on: this process already has the right folder (or the command doesn't need one).
    Stay,
    /// Run again with `SHIMMER_HOME` = `home` and its socket, with these arguments.
    Rerun { home: PathBuf, args: Vec<String> },
    /// Stop with this message, and exit 1.
    Fail(String),
}

/// Called first thing by the binary, before the async runtime starts. `None`: carry on. `Some`:
/// this process is done, with that exit code (it ran itself again in another vault, or couldn't).
///
/// Re-running instead of setting the environment in-process: `std::env::set_var` is `unsafe` from
/// Rust 2024, and the workspace forbids `unsafe` (ADR 0041 §3). It only happens when the folder
/// changes, so the re-run, whose `SHIMMER_HOME` is already that folder, never re-runs again.
pub fn enter() -> Option<ExitCode> {
    let argv: Vec<String> = std::env::args().collect();
    let (program, args) = argv.split_first()?;
    let shimmer = Path::new(program).file_stem().is_some_and(|s| s == "shimmer");
    let env_home = std::env::var_os("SHIMMER_HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    match plan(args, shimmer, env_home.as_deref(), settings::path().as_deref(), &main_home()) {
        Plan::Stay => None,
        Plan::Fail(why) => {
            eprintln!("{} {why}", crate::style::paint_err(Role::Error, "shimmer:"));
            Some(ExitCode::FAILURE)
        }
        Plan::Rerun { home, args } => Some(rerun(program, &home, &args)),
    }
}

/// [`enter`]'s decision, from its inputs. `shimmer`: started as `shimmer`, not through a pack's
/// alias, whose words are all the alias's (`ikuzo daemon` activates a workspace called `daemon`).
fn plan(args: &[String], shimmer: bool, env_home: Option<&Path>, cli_toml: Option<&Path>, main: &Path) -> Plan {
    // `shimmer daemon` and `mockd` never re-run: auto-start and login services pin their folder.
    if shimmer && matches!(args.first().map(String::as_str), Some("daemon" | "mockd")) {
        return Plan::Stay;
    }
    let (flag, rest) = take_vault_flag(args);
    let flag = match flag {
        Some(Ok(name)) => Some(name),
        Some(Err(why)) => return Plan::Fail(why),
        None => None,
    };
    // `shimmer vaults …` works on cli.toml, and reports which vault and why as this process sees it.
    if shimmer && crate::packs::active::command_word(&rest).is_some_and(|i| rest[i] == "vaults") {
        return Plan::Stay;
    }
    let settings = match cli_toml.map(settings::load) {
        Some(Ok(s)) => s,
        // A broken cli.toml is the active pack's warning already; only a named vault needs it.
        Some(Err(problems)) if flag.is_some() => return Plan::Fail(problems.join("\n")),
        Some(Err(_)) | None => Settings::default(),
    };
    let choice = match resolve(flag.as_deref(), env_home, &settings, main) {
        Ok(c) => c,
        Err(e) => return Plan::Fail(e.message),
    };
    let current = env_home.unwrap_or(main);
    if paths::resolve(current) == paths::resolve(&choice.home) {
        return Plan::Stay;
    }
    Plan::Rerun { home: choice.home, args: rest }
}

/// `--vault NAME` (or `--vault=NAME`) taken off the front of the line, before the command word, as
/// `--pack` is: `shimmer records add … --vault x` still reaches a records field called `vault`.
fn take_vault_flag(args: &[String]) -> (Option<std::result::Result<String, String>>, Vec<String>) {
    let end = crate::packs::active::command_word(args).unwrap_or(args.len());
    let mut flag = None;
    let mut rest = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < end {
        let arg = &args[i];
        let value = match arg.strip_prefix("--vault") {
            Some("") => {
                i += 1;
                Some(args.get(i).cloned().unwrap_or_default())
            }
            Some(inline) if inline.starts_with('=') => Some(inline[1..].to_owned()),
            _ => None,
        };
        match value {
            Some(v) if v.is_empty() => flag = Some(Err("--vault needs a value: a vault's name".to_owned())),
            Some(v) => flag = Some(Ok(v)),
            None => rest.push(arg.clone()),
        }
        i += 1;
    }
    rest.extend(args[end.min(args.len())..].iter().cloned());
    (flag, rest)
}

/// Run this binary again in `home`, keeping the name it was started as, so a pack's alias still
/// runs as that alias. On Unix the process is replaced and keeps its pid.
fn rerun(program: &str, home: &Path, args: &[String]) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("shimmer: can't find the shimmer binary to run in {}: {e}", home.display());
            return ExitCode::FAILURE;
        }
    };
    let err = std::process::Command::new(exe)
        .arg0(program)
        .args(args)
        .env("SHIMMER_HOME", home)
        .env("SHIMMER_SOCKET", paths::default_socket(home))
        .exec();
    eprintln!("shimmer: couldn't run in {}: {err}", home.display());
    ExitCode::FAILURE
}

// ---------------------------------------------------------------- the commands

/// Everything the commands read from outside, spelled out so tests can use temp folders.
pub struct Env {
    pub cli_toml: Option<PathBuf>,
    /// The platform's default folder: main.
    pub main: PathBuf,
    /// `SHIMMER_HOME` as this process got it.
    pub env_home: Option<PathBuf>,
    /// `--vault NAME`, if given.
    pub flag: Option<String>,
}

impl Env {
    pub fn from_process(flag: Option<String>) -> Self {
        Self {
            cli_toml: settings::path(),
            main: main_home(),
            env_home: std::env::var_os("SHIMMER_HOME").filter(|h| !h.is_empty()).map(PathBuf::from),
            flag,
        }
    }

    fn load(&self) -> Result<(PathBuf, Settings)> {
        let path = self.cli_toml.clone().ok_or_else(|| {
            Error::unavailable("can't find your config folder: neither XDG_CONFIG_HOME nor HOME is set")
        })?;
        let saved = settings::load(&path).map_err(|problems| {
            Error::invalid_params(format!("{}\nFix or delete {} and try again.", problems.join("\n"), path.display()))
        })?;
        Ok((path, saved))
    }

    fn choice(&self, settings: &Settings) -> Result<Choice> {
        resolve(self.flag.as_deref(), self.env_home.as_deref(), settings, &self.main)
    }
}

fn save(path: &Path, settings: &Settings) -> Result<()> {
    settings::save(path, settings).map_err(|e| Error::unavailable(format!("couldn't save {}: {e}", path.display())))
}

pub async fn run(cmd: &VaultsCmd, json: bool, env: &Env, prompt: &mut dyn Prompt) -> Result<String> {
    let out = |data: Value, text: String| if json { render::json(&data) } else { text };
    match cmd {
        VaultsCmd::Help => Ok(USAGE.into()),
        VaultsCmd::List { removed: false } => list(env).await.map(|(d, t)| out(d, t)),
        VaultsCmd::List { removed: true } => list_removed(env).map(|(d, t)| out(d, t)),
        VaultsCmd::Show { name } => show(name, env).await.map(|(d, t)| out(d, t)),
        VaultsCmd::Status => status(env).await.map(|(d, t)| out(d, t)),
        VaultsCmd::New { name, path } => new(name, path.as_deref(), env).map(|(d, t)| out(d, t)),
        VaultsCmd::Copy { name, new, path, live } => {
            copy(name, new, path.as_deref(), *live, env).await.map(|(d, t)| out(d, t))
        }
        VaultsCmd::Use { name } => use_vault(name, env).map(|(d, t)| out(d, t)),
        VaultsCmd::Rename { name, new } => rename(name, new, env).map(|(d, t)| out(d, t)),
        VaultsCmd::Remove { name } => remove(name, env).await.map(|(d, t)| out(d, t)),
        VaultsCmd::Restore { name } => restore(name, env).map(|(d, t)| out(d, t)),
        VaultsCmd::Purge { name, yes } => purge(name.as_deref(), *yes, env, prompt).await.map(|(d, t)| out(d, t)),
    }
}

/// Every vault, main first, with its folder.
fn all(settings: &Settings, main: &Path) -> Vec<(String, PathBuf, Option<bool>)> {
    let mut out = vec![(MAIN.to_owned(), main.to_path_buf(), None)];
    out.extend(settings.vaults.iter().map(|(n, v)| (n.clone(), v.path.clone(), Some(v.made))));
    out
}

async fn list(env: &Env) -> Result<(Value, String)> {
    let (_, saved) = env.load()?;
    let choice = env.choice(&saved).ok();
    let in_use = choice.as_ref().and_then(|c| c.name.clone());
    let mut rows = Vec::new();
    let mut data = Vec::new();
    for (name, path, made) in all(&saved, &env.main) {
        let up = daemon_up(&path).await;
        let mark = if in_use.as_deref() == Some(name.as_str()) { "*" } else { "" };
        rows.push(vec![mark.into(), name.clone(), path.display().to_string(), running(up).into()]);
        data.push(json!({"name": name, "path": path, "in_use": !mark.is_empty(), "daemon": running(up), "made": made}));
    }
    let mut text = table(&["".into(), "VAULT".into(), "FOLDER".into(), "DAEMON".into()], &rows);
    text.push_str("\n\n");
    text.push_str(&match &choice {
        Some(c) => format!("* in use: {}. {}", in_use_name(c), because(c, &saved)),
        None => "! cli.toml names a vault that isn't one: shimmer vaults use main".into(),
    });
    let why = choice.as_ref().map(|c| c.why.key());
    Ok((json!({"vaults": data, "in_use": in_use, "why": why}), text))
}

fn list_removed(env: &Env) -> Result<(Value, String)> {
    let (_, saved) = env.load()?;
    if saved.removed_vaults.is_empty() {
        return Ok((json!({"removed": []}), "No removed vaults.".into()));
    }
    let mut rows = Vec::new();
    let mut data = Vec::new();
    for (name, v) in &saved.removed_vaults {
        let purge = if v.made { "deletes the folder" } else { "forgets it (adopted)" };
        rows.push(vec![name.clone(), v.path.display().to_string(), purge.into()]);
        data.push(json!({"name": name, "path": v.path, "made": v.made, "exists": v.path.is_dir()}));
    }
    let text = format!(
        "{}\n\nBring one back with: shimmer vaults restore NAME. Delete for good with: shimmer vaults purge NAME",
        table(&["VAULT".into(), "FOLDER".into(), "PURGE".into()], &rows)
    );
    Ok((json!({"removed": data}), text))
}

async fn show(name: &str, env: &Env) -> Result<(Value, String)> {
    let (_, saved) = env.load()?;
    let home = path_of(name, &saved, &env.main)?;
    let made = saved.vaults.get(name).map(|v| v.made);
    let socket = paths::default_socket(&home);
    let up = daemon_up(&home).await;
    let in_use = env.choice(&saved).ok().and_then(|c| c.name).as_deref() == Some(name);
    let size = folder_size(&home);
    let kind = match made {
        None => "your default folder",
        Some(true) => "made by Shimmer: purge deletes it",
        Some(false) => "adopted: purge only forgets it, never deletes it",
    };
    let mut text = format!("{}{}\n", paint(Role::Name, name), if in_use { "  (in use)" } else { "" });
    let mut line = |k: &str, v: String| text.push_str(&format!("  {:<8}{v}\n", format!("{k}:")));
    line("folder", home.display().to_string());
    if !home.is_dir() {
        line("", "! the folder isn't there".into());
    }
    line("socket", socket.display().to_string());
    line("daemon", running(up).into());
    line("size", size.map(human).unwrap_or_else(|| "-".into()));
    line("kind", kind.into());
    let data = json!({
        "name": name, "path": home, "socket": socket, "daemon": running(up), "made": made,
        "size_bytes": size, "in_use": in_use, "exists": home.is_dir(),
    });
    Ok((data, text.trim_end().to_owned()))
}

async fn status(env: &Env) -> Result<(Value, String)> {
    let (_, saved) = env.load()?;
    let choice = env.choice(&saved)?;
    let mut up = Vec::new();
    for (name, path, _) in all(&saved, &env.main) {
        if daemon_up(&path).await {
            up.push(name);
        }
    }
    let text = format!(
        "in use: {}  ({})\n  {}\ndaemons running: {}",
        in_use_name(&choice),
        choice.home.display(),
        because(&choice, &saved),
        if up.is_empty() { "none".into() } else { up.join(", ") }
    );
    let data = json!({"vault": choice.name, "path": choice.home, "why": choice.why.key(), "running": up});
    Ok((data, text))
}

fn in_use_name(c: &Choice) -> String {
    match &c.name {
        Some(name) => paint(Role::Name, name),
        None => "a folder that isn't a vault".into(),
    }
}

/// Why `c` is the vault in use, as a sentence for people.
fn because(c: &Choice, saved: &Settings) -> String {
    match c.why {
        Why::Flag => "Because --vault named it, for this command only.".into(),
        Why::Env => {
            let used = saved.vault.as_deref().map(|u| format!(" (instead of '{u}', chosen with 'vaults use')"));
            format!("Because SHIMMER_HOME is set{}.", used.unwrap_or_default())
        }
        Why::Used => "Chosen with 'shimmer vaults use'; go back with: shimmer vaults use main".into(),
        Why::Main => "The default. Switch with: shimmer vaults use NAME".into(),
    }
}

fn new(name: &str, path: Option<&Path>, env: &Env) -> Result<(Value, String)> {
    let (file, mut saved) = env.load()?;
    check_new_name(name, &saved)?;
    let path = target_path(name, path, &saved, env)?;
    let made = if !path.exists() {
        std::fs::create_dir_all(&path)
            .map_err(|e| Error::unavailable(format!("couldn't make {}: {e}", path.display())))?;
        true
    } else if !path.is_dir() {
        return Err(Error::invalid_params(format!("{} isn't a folder", path.display())));
    } else if is_empty(&path) {
        true
    } else if looks_like_shimmer(&path) {
        false
    } else {
        return Err(Error::invalid_params(format!(
            "{} has files in it and isn't a Shimmer folder; use an empty folder or a new path",
            path.display()
        )));
    };
    saved.vaults.insert(name.to_owned(), Vault { path: path.clone(), made });
    save(&file, &saved)?;
    let how = if made { "new and empty" } else { "an existing Shimmer folder, adopted" };
    let text = format!(
        "✓ vault {} at {} ({how})\n  Use it once with: shimmer --vault {name} <command>. Make it the default with: shimmer vaults use {name}",
        paint(Role::Name, name),
        path.display()
    );
    Ok((json!({"name": name, "path": path, "made": made}), text))
}

/// Where a new vault goes: `--path` made absolute, else beside main. Never inside main (its files
/// would be synced with main's, and main's daemon would sit around another vault's), and never a
/// folder that's already a vault.
fn target_path(name: &str, path: Option<&Path>, saved: &Settings, env: &Env) -> Result<PathBuf> {
    let path = match path {
        Some(p) => std::path::absolute(p).map_err(|e| Error::invalid_params(format!("{}: {e}", p.display())))?,
        None => default_path(name, &env.main),
    };
    if paths::resolve(&path).starts_with(paths::resolve(&env.main)) {
        return Err(Error::invalid_params(format!(
            "{} is inside your main folder ({}), which is meant to be synced with git; pick a folder outside it",
            path.display(),
            env.main.display()
        )));
    }
    if let Some(other) = name_of(&path, saved, &env.main) {
        return Err(Error::conflict(format!("{} is already the vault '{other}'", path.display())));
    }
    Ok(path)
}

fn check_new_name(name: &str, saved: &Settings) -> Result<()> {
    if let Some(why) = vault_name_problem(name) {
        return Err(Error::invalid_params(format!("'{name}' can't be a vault's name: it {why}")));
    }
    if name == MAIN {
        return Err(Error::invalid_params("'main' is your default folder's name; pick another"));
    }
    if saved.vaults.contains_key(name) {
        return Err(Error::conflict(format!("there's already a vault named '{name}'")));
    }
    if saved.removed_vaults.contains_key(name) {
        return Err(Error::conflict(format!(
            "a removed vault is named '{name}': restore it, or purge it first (shimmer vaults purge {name})"
        )));
    }
    Ok(())
}

fn use_vault(name: &str, env: &Env) -> Result<(Value, String)> {
    let (file, mut saved) = env.load()?;
    let home = path_of(name, &saved, &env.main)?;
    if name != MAIN && !home.is_dir() {
        return Err(Error::not_found(format!(
            "vault '{name}'s folder isn't there (expected at {}); put it back, or remove the vault",
            home.display()
        )));
    }
    saved.vault = (name != MAIN).then(|| name.to_owned());
    save(&file, &saved)?;
    let mut text = format!("✓ in use: {} ({})", paint(Role::Name, name), home.display());
    if env.env_home.is_some() {
        text.push_str(&format!(
            "\n{} SHIMMER_HOME is set here, and it comes first: plain commands still go to {}",
            paint(Role::Warning, "!"),
            env.env_home.as_deref().unwrap_or(Path::new("")).display()
        ));
    }
    Ok((json!({"vault": name, "path": home}), text))
}

fn rename(name: &str, new: &str, env: &Env) -> Result<(Value, String)> {
    let (file, mut saved) = env.load()?;
    if name == MAIN {
        return Err(Error::invalid_params("main can't be renamed: it's your default folder"));
    }
    path_of(name, &saved, &env.main)?;
    check_new_name(new, &saved)?;
    let vault = saved.vaults.remove(name).expect("checked above");
    saved.vaults.insert(new.to_owned(), vault);
    if saved.vault.as_deref() == Some(name) {
        saved.vault = Some(new.to_owned());
    }
    save(&file, &saved)?;
    Ok((json!({"name": new, "was": name}), format!("✓ vault {name} is now {}", paint(Role::Name, new))))
}

async fn remove(name: &str, env: &Env) -> Result<(Value, String)> {
    let (file, mut saved) = env.load()?;
    if name == MAIN {
        return Err(Error::invalid_params("main can't be removed: it's your default folder"));
    }
    let home = path_of(name, &saved, &env.main)?;
    if saved.vault.as_deref() == Some(name) || env.choice(&saved).ok().and_then(|c| c.name).as_deref() == Some(name) {
        return Err(Error::conflict(format!(
            "'{name}' is the vault in use; switch first (shimmer vaults use main), then remove it"
        )));
    }
    if login_vault(&saved).is_some_and(|login| paths::resolve(&login) == paths::resolve(&home)) {
        return Err(Error::conflict(format!(
            "'{name}' is the vault 'shimmer startup' starts at login; run 'shimmer --vault main startup on' or 'startup off' first"
        )));
    }
    if saved.removed_vaults.contains_key(name) {
        return Err(Error::conflict(format!(
            "a removed vault is already named '{name}': purge it first (shimmer vaults purge {name})"
        )));
    }
    // Nothing can name it once it's forgotten, so its daemon would run on unseen.
    let stopped = stop_daemon(&home).await?;
    let vault = saved.vaults.remove(name).expect("checked above");
    saved.removed_vaults.insert(name.to_owned(), vault);
    save(&file, &saved)?;
    let mut text = format!("✓ removed vault {name}; its files are kept at {}", home.display());
    if stopped {
        text.push_str("\n  its daemon was stopped");
    }
    text.push_str(&format!("\n  Bring it back with: shimmer vaults restore {name}"));
    Ok((json!({"removed": name, "path": home, "daemon_stopped": stopped}), text))
}

fn restore(name: &str, env: &Env) -> Result<(Value, String)> {
    let (file, mut saved) = env.load()?;
    let Some(vault) = saved.removed_vaults.get(name).cloned() else {
        return Err(Error::not_found(format!("no removed vault named '{name}' (see 'shimmer vaults list --removed')")));
    };
    if saved.vaults.contains_key(name) {
        return Err(Error::conflict(format!("there's a vault named '{name}' again; rename it first")));
    }
    let missing = !vault.path.is_dir();
    saved.removed_vaults.remove(name);
    saved.vaults.insert(name.to_owned(), vault.clone());
    save(&file, &saved)?;
    let mut text = format!("✓ restored vault {} ({})", paint(Role::Name, name), vault.path.display());
    if missing {
        text.push_str(&format!("\n{} its folder isn't there any more", paint(Role::Warning, "!")));
    }
    Ok((json!({"restored": name, "path": vault.path, "exists": !missing}), text))
}

async fn purge(name: Option<&str>, yes: bool, env: &Env, prompt: &mut dyn Prompt) -> Result<(Value, String)> {
    let (file, mut saved) = env.load()?;
    let which: BTreeMap<String, Vault> = match name {
        Some(n) => match saved.removed_vaults.get(n) {
            Some(v) => BTreeMap::from([(n.to_owned(), v.clone())]),
            None => {
                return Err(Error::not_found(format!(
                    "no removed vault named '{n}' (see 'shimmer vaults list --removed')"
                )))
            }
        },
        None => saved.removed_vaults.clone(),
    };
    if which.is_empty() {
        return Ok((json!({"purged": 0}), "nothing to purge: no removed vaults".into()));
    }
    let deletes: Vec<&String> = which.iter().filter(|(_, v)| deletable(v)).map(|(n, _)| n).collect();
    let message = format!(
        "{} removed vault(s): {}. Deletes the folders of {}; the others are only forgotten",
        which.len(),
        which.keys().cloned().collect::<Vec<_>>().join(", "),
        if deletes.is_empty() {
            "none".into()
        } else {
            deletes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        }
    );
    let what = match name {
        Some(n) => format!("the vault '{n}'"),
        None => "every removed vault".into(),
    };
    if !crate::records::confirm_purge(prompt, yes, &what, &message)? {
        return Ok((json!({"purged": 0}), "nothing purged".into()));
    }
    let (mut deleted, mut forgotten) = (Vec::new(), Vec::new());
    for (n, v) in &which {
        if deletable(v) {
            // Its daemon goes first, and the folder once its socket has (§5).
            stop_daemon(&v.path).await?;
            std::fs::remove_dir_all(&v.path)
                .map_err(|e| Error::unavailable(format!("couldn't delete {}: {e}", v.path.display())))?;
            deleted.push(n.clone());
        } else {
            forgotten.push(n.clone());
        }
        saved.removed_vaults.remove(n);
        save(&file, &saved)?;
    }
    let mut text = format!("✓ purged {} vault(s)", which.len());
    if !deleted.is_empty() {
        text.push_str(&format!("\n  deleted: {}", deleted.join(", ")));
    }
    if !forgotten.is_empty() {
        text.push_str(&format!(
            "\n  forgotten, folders kept (Shimmer didn't make them, or they're gone): {}",
            forgotten.join(", ")
        ));
    }
    Ok((json!({"purged": which.len(), "deleted": deleted, "forgotten": forgotten}), text))
}

/// Whether `purge` may delete `v`'s folder: Shimmer made it, it's still a Shimmer folder (or empty),
/// and it isn't a symlink. Shimmer never deletes a folder it didn't make (§5).
fn deletable(v: &Vault) -> bool {
    let real_dir = std::fs::symlink_metadata(&v.path).is_ok_and(|m| m.is_dir());
    v.made && real_dir && (looks_like_shimmer(&v.path) || is_empty(&v.path))
}

/// A Shimmer folder has the store's `.gitignore` and `data/` (§5).
fn looks_like_shimmer(dir: &Path) -> bool {
    dir.join(".gitignore").is_file() && dir.join("data").is_dir()
}

fn is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none())
}

/// The folder the login service starts, if `shimmer startup on` wrote one.
fn login_vault(saved: &Settings) -> Option<PathBuf> {
    let text = std::fs::read_to_string(&saved.startup.as_ref()?.file).ok()?;
    crate::startup::pinned_home(&text)
}

// ---------------------------------------------------------------- copy (§5)

/// What a copy leaves out: derived or tied to this machine (§5). `.staging/` is copied, so the copy's
/// own recovery finishes any save that was committed, and keeps aside one that's damaged (ADR 0040).
fn left_out(name: &str) -> bool {
    matches!(name, "cache" | "logs" | "daemon.sock" | ".daemon.lock") || name.starts_with("index.sqlite")
}

/// What `copy` switched off in the copy, through each feature's own ops.
#[derive(Debug, Default)]
struct SwitchedOff {
    sources: Vec<String>,
    rules: Vec<String>,
    schedules: Vec<String>,
    notify_paused: bool,
    workspaces: Vec<String>,
}

async fn copy(name: &str, new: &str, path: Option<&Path>, live: bool, env: &Env) -> Result<(Value, String)> {
    let (file, saved) = env.load()?;
    let source = path_of(name, &saved, &env.main)?;
    if !looks_like_shimmer(&source) {
        return Err(Error::not_found(format!("vault '{name}' has nothing to copy yet ({})", source.display())));
    }
    check_new_name(new, &saved)?;
    let target = target_path(new, path, &saved, env)?;
    if paths::resolve(&target).starts_with(paths::resolve(&source)) {
        return Err(Error::invalid_params(format!("{} is inside the vault being copied", target.display())));
    }
    if target.exists() && !(target.is_dir() && is_empty(&target)) {
        return Err(Error::invalid_params(format!("{} already exists and isn't empty", target.display())));
    }
    // Built beside the target and renamed into place last, so an interrupted copy never leaves a
    // vault that's registered, or one that isn't switched off yet (Wasay on #312).
    let staging = target.with_file_name(format!(
        "{}.copying",
        target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    if staging.exists() {
        // An earlier copy to this name that was interrupted: only ever something this made.
        if !(looks_like_shimmer(&staging) || is_empty(&staging)) || stop_daemon(&staging).await.is_err() {
            return Err(Error::conflict(format!(
                "{} is in the way and isn't a copy Shimmer left behind; move it",
                staging.display()
            )));
        }
        std::fs::remove_dir_all(&staging).map_err(|e| io_error("remove", &staging, e))?;
    }

    // One writer (CLAUDE.md §2): VAULT's daemon is stopped and its lock held, so none can start
    // there mid-copy. For those moments there is no writer at all.
    stop_daemon(&source).await?;
    {
        // Held until the copy is made, then let go.
        let _lock = hold_lock(&source, name).await?;
        let copied = copy_tree(&source, &staging, true);
        if let Err(e) = copied {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    }

    let off = if live {
        None
    } else {
        match switch_off(&staging).await {
            Ok(off) => Some(off),
            Err(e) => {
                let _ = stop_daemon(&staging).await;
                let _ = std::fs::remove_dir_all(&staging);
                return Err(Error {
                    message: format!("couldn't switch the copy off, so it wasn't made: {}", e.message),
                    ..e
                });
            }
        }
    };

    if target.exists() {
        std::fs::remove_dir(&target).map_err(|e| io_error("replace", &target, e))?;
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_error("make", parent, e))?;
    }
    std::fs::rename(&staging, &target).map_err(|e| io_error("move into place", &target, e))?;
    // Read again: `cli.toml` may have changed while the copy ran.
    let (_, mut saved) = env.load()?;
    saved.vaults.insert(new.to_owned(), Vault { path: target.clone(), made: true });
    save(&file, &saved)?;

    let mut text = format!("✓ copied {name} to {} at {}", paint(Role::Name, new), target.display());
    let data_off = match &off {
        None => {
            text.push_str(
                "
  left on (--live): its schedules, automations and sources run when its daemon starts",
            );
            Value::Null
        }
        Some(off) => {
            text.push_str(&describe_off(new, off));
            json!({"schedules": off.schedules, "rules": off.rules, "sources": off.sources,
                   "notify_paused": off.notify_paused, "workspaces_reset": off.workspaces})
        }
    };
    let data = json!({"name": new, "from": name, "path": target, "made": true, "live": live, "switched_off": data_off});
    Ok((data, text))
}

/// Take `home`'s `.daemon.lock`, so no daemon can start there. A daemon that was just told to stop
/// lets go of it a moment after its socket goes, so this waits a little, as a starting daemon does.
async fn hold_lock(home: &Path, name: &str) -> Result<std::fs::File> {
    let lock = std::fs::File::create(home.join(".daemon.lock")).map_err(|e| io_error("lock", home, e))?;
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Error::conflict(format!("a daemon is running in '{name}' and didn't stop; try again")))
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(io_error("lock", home, e)),
        }
    }
}

/// How long `copy` waits for a stopped daemon's lock.
const LOCK_WAIT: Duration = Duration::from_secs(5);

fn io_error(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::unavailable(format!("couldn't {what} {}: {e}", path.display()))
}

/// Copy `from` into a new folder `to`: files, folders and symlinks (as links, never followed). At
/// the top, [`left_out`] isn't copied.
fn copy_tree(from: &Path, to: &Path, top: bool) -> Result<()> {
    std::fs::create_dir_all(to).map_err(|e| io_error("make", to, e))?;
    for entry in std::fs::read_dir(from).map_err(|e| io_error("read", from, e))? {
        let entry = entry.map_err(|e| io_error("read", from, e))?;
        let name = entry.file_name();
        if top && left_out(&name.to_string_lossy()) {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        let kind = entry.file_type().map_err(|e| io_error("read", &src, e))?;
        if kind.is_symlink() {
            let target = std::fs::read_link(&src).map_err(|e| io_error("read", &src, e))?;
            std::os::unix::fs::symlink(target, &dst).map_err(|e| io_error("copy", &src, e))?;
        } else if kind.is_dir() {
            copy_tree(&src, &dst, false)?;
        } else if kind.is_file() {
            std::fs::copy(&src, &dst).map_err(|e| io_error("copy", &src, e))?;
        }
        // Sockets and other special files aren't data: left out.
    }
    Ok(())
}

/// Start the copy's daemon quiet (§5a), switch off everything that would act by itself, through
/// each feature's own ops (the daemon never edits a module's files), then stop it. An op that isn't
/// in the manifest is skipped: a module that isn't installed has nothing to switch off.
async fn switch_off(home: &Path) -> Result<SwitchedOff> {
    let socket = paths::default_socket(home);
    let exe = crate::daemon_exe()?;
    let mut c = crate::autostart::start_quiet(&exe, home, &socket).await?;
    let manifest = c.call(ops::CORE_MANIFEST, json!({})).await?;
    let has = |op: &str| {
        manifest["modules"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|m| m["commands"].as_array().into_iter().flatten())
            .any(|cmd| cmd["op"] == op)
    };
    // On, and not Shimmer's own: built-in alerts only send notifications, which are paused, and a
    // schedule a module declares for itself (notify's flush) is its plumbing, which a person
    // wouldn't know to turn back on.
    let enabled = |list: &Value, key: &str| -> Vec<String> {
        list[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|x| x["enabled"] == true && x["builtin"] != true && x["source"]["type"] != "module")
            .filter_map(|x| x["id"].as_str().map(str::to_owned))
            .collect()
    };
    let mut off = SwitchedOff::default();
    // Sources first: disabling one also removes its own next-run schedule (#320).
    if has("fetchers.disable") {
        for id in enabled(&c.call("fetchers.list", json!({})).await?, "sources") {
            c.call("fetchers.disable", json!({"id": id})).await?;
            off.sources.push(id);
        }
    }
    if has("automations.disable") {
        for id in enabled(&c.call("automations.list", json!({})).await?, "rules") {
            c.call("automations.disable", json!({"id": id})).await?;
            off.rules.push(id);
        }
    }
    // Every schedule still on, including the one-time ones modules added for you (an automation's
    // delayed action, ADR 0035 §5.6: source "added").
    for id in enabled(&c.call("scheduler.list", json!({})).await?, "triggers") {
        c.call("scheduler.disable", json!({"trigger_id": id})).await?;
        off.schedules.push(id);
    }
    if has("notify.pause") {
        c.call("notify.pause", json!({"all": true})).await?;
        off.notify_paused = true;
    }
    // A copied `active` was the source's launch; `launching` was made `dirty` as it started.
    if has("workspaces.reset") {
        let list = c.call("workspaces.list", json!({})).await?;
        for w in list["workspaces"].as_array().into_iter().flatten() {
            if matches!(w["state"].as_str(), Some("active" | "dirty")) {
                if let Some(id) = w["id"].as_str() {
                    c.call("workspaces.reset", json!({"id": id})).await?;
                    off.workspaces.push(id.to_owned());
                }
            }
        }
    }
    drop(c);
    stop_daemon(home).await?;
    Ok(off)
}

/// The lines after `✓ copied …`: what was switched off, and how to turn each back on.
fn describe_off(new: &str, off: &SwitchedOff) -> String {
    let n = |v: &[String], one: &str, many: &str| match v.len() {
        1 => format!("1 {one}"),
        k => format!("{k} {many}"),
    };
    let mut parts = Vec::new();
    if !off.schedules.is_empty() {
        parts.push(n(&off.schedules, "schedule", "schedules"));
    }
    if !off.rules.is_empty() {
        parts.push(n(&off.rules, "automation", "automations"));
    }
    if !off.sources.is_empty() {
        parts.push(n(&off.sources, "fetcher source", "fetcher sources"));
    }
    let mut text = String::new();
    if parts.is_empty() {
        text.push_str("\n  nothing in it was switched on");
    } else {
        text.push_str(&format!("\n  switched off, so nothing runs twice: {}", parts.join(", ")));
    }
    if off.notify_paused {
        text.push_str("\n  notifications paused");
    }
    if !off.workspaces.is_empty() {
        text.push_str(&format!(
            "\n  {} reset to ready (it was the original's launch)",
            n(&off.workspaces, "workspace", "workspaces")
        ));
    }
    let mut back = Vec::new();
    if !off.schedules.is_empty() {
        back.push("scheduler enable ID");
    }
    if !off.rules.is_empty() {
        back.push("automations enable ID");
    }
    if !off.sources.is_empty() {
        back.push("fetchers enable ID");
    }
    if off.notify_paused {
        back.push("notify resume");
    }
    if !back.is_empty() {
        let hint = format!("→ turn things back on in it with: shimmer --vault {new} {}", back.join(", "));
        text.push_str(&format!("\n  {}", paint(Role::Hint, &hint)));
    }
    text
}

// ---------------------------------------------------------------- daemons

/// How long to wait for another vault's daemon: listing vaults must stay quick.
const DAEMON_WAIT: Duration = Duration::from_millis(500);
/// How long a stopping daemon may take to let go of its socket.
const STOP_WAIT: Duration = Duration::from_secs(10);

fn running(up: bool) -> &'static str {
    if up {
        "running"
    } else {
        "stopped"
    }
}

async fn daemon_up(home: &Path) -> bool {
    let socket = paths::default_socket(home);
    matches!(tokio::time::timeout(DAEMON_WAIT, Client::connect_for(&socket, home)).await, Ok(Ok(_)))
}

/// Stop `home`'s daemon, if one runs, and wait until its socket is gone. `true` if one was stopped.
async fn stop_daemon(home: &Path) -> Result<bool> {
    let socket = paths::default_socket(home);
    let mut client = match Client::connect_for(&socket, home).await {
        Ok(c) => c,
        Err(e) if e.nobody_listening() => return Ok(false),
        Err(e) => return Err(e.into_error(&socket)),
    };
    client.call(ops::CORE_SHUTDOWN, json!({})).await?;
    let deadline = tokio::time::Instant::now() + STOP_WAIT;
    loop {
        match Client::connect_for(&socket, home).await {
            Err(e) if e.nobody_listening() => return Ok(true),
            _ if tokio::time::Instant::now() >= deadline => {
                return Err(Error::unavailable(format!(
                    "asked the daemon for {} to stop, but it still answers after {}s",
                    home.display(),
                    STOP_WAIT.as_secs()
                )))
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

// ---------------------------------------------------------------- sizes

/// Bytes in `dir`'s files, not following symlinks. `None` if it can't be read at all.
fn folder_size(dir: &Path) -> Option<u64> {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    std::fs::read_dir(dir).ok()?;
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match entry.metadata() {
                Ok(m) if m.is_dir() => stack.push(entry.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    Some(total)
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn settings(used: Option<&str>) -> Settings {
        Settings {
            vault: used.map(Into::into),
            vaults: BTreeMap::from([("play".into(), Vault { path: "/v/play".into(), made: true })]),
            removed_vaults: BTreeMap::from([("old".into(), Vault { path: "/v/old".into(), made: true })]),
            ..Settings::default()
        }
    }

    #[test]
    fn the_flag_beats_shimmer_home_beats_use_beats_main() {
        let main = Path::new("/m/shimmer");
        let env = Some(Path::new("/elsewhere"));
        let pick = |flag, env, used| resolve(flag, env, &settings(used), main).unwrap();

        let c = pick(Some("play"), env, Some("play"));
        assert_eq!((c.name.as_deref(), c.home.as_path(), c.why), (Some("play"), Path::new("/v/play"), Why::Flag));
        let c = pick(Some("main"), env, Some("play"));
        assert_eq!((c.home.as_path(), c.why), (main, Why::Flag));
        let c = pick(None, env, Some("play"));
        assert_eq!((c.name, c.home.as_path(), c.why), (None, Path::new("/elsewhere"), Why::Env));
        let c = pick(None, None, Some("play"));
        assert_eq!((c.home.as_path(), c.why), (Path::new("/v/play"), Why::Used));
        let c = pick(None, None, None);
        assert_eq!((c.name.as_deref(), c.home.as_path(), c.why), (Some("main"), main, Why::Main));
        // SHIMMER_HOME pointing at a vault's folder is that vault.
        assert_eq!(pick(None, Some(Path::new("/v/play/")), None).name.as_deref(), Some("play"));
    }

    #[test]
    fn unknown_and_removed_vaults_say_what_to_do() {
        let main = Path::new("/m");
        let e = resolve(Some("nope"), None, &settings(None), main).unwrap_err();
        assert!(e.message.contains("no vault named 'nope'"), "{}", e.message);
        let e = resolve(Some("old"), None, &settings(None), main).unwrap_err();
        assert!(e.message.contains("shimmer vaults restore old"), "{}", e.message);
        let e = resolve(None, None, &settings(Some("gone")), main).unwrap_err();
        assert!(e.message.contains("shimmer vaults use main"), "{}", e.message);
    }

    #[test]
    fn main_is_the_platform_folder_whatever_shimmer_home_says() {
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(main_home_from(p("/h"), None, false), Path::new("/h/.local/share/shimmer"));
        assert_eq!(main_home_from(p("/h"), p("/x"), false), Path::new("/x/shimmer"));
        assert_eq!(main_home_from(p("/h"), p("/x"), true), Path::new("/h/Library/Application Support/shimmer"));
        assert_eq!(
            default_path("play", Path::new("/h/.local/share/shimmer")),
            Path::new("/h/.local/share/shimmer-vaults/play")
        );
    }

    #[test]
    fn the_vault_flag_only_counts_before_the_command() {
        let (flag, rest) = take_vault_flag(&s(&["--json", "--vault", "play", "records", "list"]));
        assert_eq!((flag, rest), (Some(Ok("play".into())), s(&["--json", "records", "list"])));
        let (flag, rest) = take_vault_flag(&s(&["--vault=play", "ping"]));
        assert_eq!((flag, rest), (Some(Ok("play".into())), s(&["ping"])));
        let (flag, rest) = take_vault_flag(&s(&["records", "add", "x", "--vault", "y"]));
        assert_eq!((flag, rest), (None, s(&["records", "add", "x", "--vault", "y"])));
        assert!(matches!(take_vault_flag(&s(&["--vault"])).0, Some(Err(_))));
        assert!(matches!(take_vault_flag(&s(&["--vault=", "ping"])).0, Some(Err(_))));
    }

    fn cli_toml(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join("cli.toml");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn it_reruns_only_when_the_folder_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let play = tmp.path().join("play");
        let toml = cli_toml(tmp.path(), &format!("[vaults.play]\npath = \"{}\"\nmade = true\n", play.display()));
        let plan = |args: &[&str], shimmer, env: Option<&Path>| plan(&s(args), shimmer, env, Some(&toml), &main);

        assert_eq!(plan(&["ping"], true, None), Plan::Stay, "main, and nothing set");
        assert_eq!(
            plan(&["--vault", "play", "ping"], true, None),
            Plan::Rerun { home: play.clone(), args: s(&["ping"]) }
        );
        // The re-run: SHIMMER_HOME is play already, so it carries on (no loop, no marker).
        assert_eq!(plan(&["--vault", "play", "ping"], true, Some(&play)), Plan::Stay);
        assert_eq!(plan(&["ping"], true, Some(&play)), Plan::Stay, "SHIMMER_HOME alone is never changed");
        assert_eq!(
            plan(&["--vault", "main", "ping"], true, Some(&play)),
            Plan::Rerun { home: main.clone(), args: s(&["ping"]) }
        );
        assert_eq!(plan(&["daemon"], true, None), Plan::Stay);
        assert_eq!(
            plan(&["--vault", "play", "vaults", "list"], true, None),
            Plan::Stay,
            "vaults reads cli.toml itself"
        );
        // Through a pack's alias, `daemon` and `vaults` are the alias's arguments.
        assert_eq!(
            plan(&["--vault", "play", "daemon"], false, None),
            Plan::Rerun { home: play.clone(), args: s(&["daemon"]) }
        );
        assert!(
            matches!(plan(&["--vault", "nope", "ping"], true, None), Plan::Fail(m) if m.contains("no vault named 'nope'"))
        );

        let toml = cli_toml(
            tmp.path(),
            &format!("vault = \"play\"\n[vaults.play]\npath = \"{}\"\nmade = true\n", play.display()),
        );
        let plan = |args: &[&str], env: Option<&Path>| super::plan(&s(args), true, env, Some(&toml), &main);
        assert_eq!(plan(&["ping"], None), Plan::Rerun { home: play.clone(), args: s(&["ping"]) }, "the used vault");
        assert_eq!(plan(&["ping"], Some(&main)), Plan::Stay, "SHIMMER_HOME comes before use");
    }

    #[test]
    fn a_broken_cli_toml_only_stops_a_named_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = cli_toml(tmp.path(), "vault = 3");
        let main = tmp.path().join("main");
        assert_eq!(plan(&s(&["ping"]), true, None, Some(&toml), &main), Plan::Stay);
        assert!(matches!(plan(&s(&["--vault", "x", "ping"]), true, None, Some(&toml), &main), Plan::Fail(_)));
    }

    #[test]
    fn a_copy_leaves_out_only_whats_derived_or_tied_to_the_machine() {
        for out in
            ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm", "cache", "logs", "daemon.sock", ".daemon.lock"]
        {
            assert!(left_out(out), "{out}");
        }
        for kept in ["data", "events", ".staging", "config.toml", ".gitignore", "packs"] {
            assert!(!left_out(kept), "{kept}");
        }
    }

    #[test]
    fn commands_parse() {
        let p = |w: &[&str]| parse(s(w));
        assert_eq!(p(&[]), Ok(VaultsCmd::List { removed: false }));
        assert_eq!(p(&["list", "--removed"]), Ok(VaultsCmd::List { removed: true }));
        assert_eq!(
            p(&["new", "play", "--path", "/x"]),
            Ok(VaultsCmd::New { name: "play".into(), path: Some("/x".into()) })
        );
        assert_eq!(p(&["rename", "a", "b"]), Ok(VaultsCmd::Rename { name: "a".into(), new: "b".into() }));
        assert_eq!(p(&["purge"]), Ok(VaultsCmd::Purge { name: None, yes: false }));
        assert_eq!(p(&["purge", "old", "--yes"]), Ok(VaultsCmd::Purge { name: Some("old".into()), yes: true }));
        assert_eq!(
            p(&["copy", "main", "play", "--live"]),
            Ok(VaultsCmd::Copy { name: "main".into(), new: "play".into(), path: None, live: true })
        );
        assert!(p(&["copy", "main"]).unwrap_err().contains("usage: shimmer vaults copy VAULT NEW"));
        assert!(p(&["use"]).unwrap_err().contains("usage: shimmer vaults use VAULT"));
        assert!(p(&["list", "--yes"]).is_err());
        assert!(p(&["bogus"]).is_err());
    }

    struct Answer(bool);

    impl Prompt for Answer {
        fn interactive(&self) -> bool {
            true
        }
        fn confirm(&mut self, _question: &str) -> bool {
            self.0
        }
    }

    fn env(tmp: &Path) -> Env {
        Env { cli_toml: Some(tmp.join("cli.toml")), main: tmp.join("data/shimmer"), env_home: None, flag: None }
    }

    async fn go(cmd: &[&str], env: &Env) -> Result<String> {
        run(&parse(s(cmd)).unwrap(), false, env, &mut Answer(true)).await
    }

    #[tokio::test]
    async fn new_use_rename_remove_restore_purge() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        go(&["new", "play"], &env).await.unwrap();
        let play = tmp.path().join("data/shimmer-vaults/play");
        assert!(play.is_dir(), "beside main, not inside it");
        assert_eq!(go(&["new", "play"], &env).await.unwrap_err().code, shimmer_core::ErrorCode::Conflict);
        assert!(go(&["new", "main"], &env).await.is_err());
        assert!(go(&["new", "Bad"], &env).await.is_err());

        go(&["use", "play"], &env).await.unwrap();
        assert!(go(&["remove", "play"], &env).await.unwrap_err().message.contains("in use"));
        go(&["rename", "play", "sandbox"], &env).await.unwrap();
        let saved = settings::load(env.cli_toml.as_deref().unwrap()).unwrap();
        assert_eq!(saved.vault.as_deref(), Some("sandbox"), "use follows a rename");
        go(&["use", "main"], &env).await.unwrap();

        // Pretend the daemon made it a Shimmer folder.
        std::fs::create_dir_all(play.join("data")).unwrap();
        std::fs::write(play.join(".gitignore"), "index.sqlite\n").unwrap();
        go(&["remove", "sandbox"], &env).await.unwrap();
        assert!(play.is_dir(), "remove keeps the files");
        assert!(go(&["restore", "nope"], &env).await.is_err());
        go(&["restore", "sandbox"], &env).await.unwrap();
        go(&["remove", "sandbox"], &env).await.unwrap();
        let out = go(&["purge", "sandbox"], &env).await.unwrap();
        assert!(out.contains("deleted: sandbox"), "{out}");
        assert!(!play.exists());
        assert!(go(&["purge"], &env).await.unwrap().contains("nothing to purge"));
        for refused in [&["remove", "main"][..], &["rename", "main", "x"]] {
            assert!(go(refused, &env).await.is_err(), "{refused:?}");
        }
    }

    #[tokio::test]
    async fn new_refuses_a_busy_folder_and_adopts_a_shimmer_one() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        let busy = tmp.path().join("busy");
        std::fs::create_dir_all(&busy).unwrap();
        std::fs::write(busy.join("notes.txt"), "mine").unwrap();
        let e = go(&["new", "x", "--path", busy.to_str().unwrap()], &env).await.unwrap_err();
        assert!(e.message.contains("isn't a Shimmer folder"), "{}", e.message);

        let inside = env.main.join("nested");
        let e = go(&["new", "x", "--path", inside.to_str().unwrap()], &env).await.unwrap_err();
        assert!(e.message.contains("inside your main folder"), "{}", e.message);

        let theirs = tmp.path().join("theirs");
        std::fs::create_dir_all(theirs.join("data")).unwrap();
        std::fs::write(theirs.join(".gitignore"), "").unwrap();
        go(&["new", "theirs", "--path", theirs.to_str().unwrap()], &env).await.unwrap();
        go(&["remove", "theirs"], &env).await.unwrap();
        let out = go(&["purge", "--yes"], &env).await.unwrap();
        assert!(out.contains("forgotten"), "{out}");
        assert!(theirs.join("data").is_dir(), "an adopted vault is never deleted");
    }

    #[tokio::test]
    async fn purge_asks_and_no_keeps_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        go(&["new", "play"], &env).await.unwrap();
        go(&["remove", "play"], &env).await.unwrap();
        let out = run(&VaultsCmd::Purge { name: None, yes: false }, false, &env, &mut Answer(false)).await.unwrap();
        assert_eq!(out, "nothing purged");
        assert!(tmp.path().join("data/shimmer-vaults/play").is_dir());
        let saved = settings::load(env.cli_toml.as_deref().unwrap()).unwrap();
        assert!(saved.removed_vaults.contains_key("play"));
    }

    #[tokio::test]
    async fn status_names_the_reason_as_given() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = env(tmp.path());
        go(&["new", "play"], &env).await.unwrap();
        assert!(go(&["status"], &env).await.unwrap().contains("The default"));
        go(&["use", "play"], &env).await.unwrap();
        assert!(go(&["status"], &env).await.unwrap().contains("Chosen with 'shimmer vaults use'"));
        env.env_home = Some(env.main.clone());
        let out = go(&["status"], &env).await.unwrap();
        assert!(out.contains("SHIMMER_HOME is set (instead of 'play'"), "{out}");
        env.flag = Some("play".into());
        assert!(go(&["status"], &env).await.unwrap().contains("Because --vault named it"));
    }

    #[tokio::test]
    async fn remove_never_replaces_a_removed_vault_of_the_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        go(&["new", "play"], &env).await.unwrap();
        go(&["remove", "play"], &env).await.unwrap();
        // `new` refuses the name too, so the only way to a second "play" is a hand-edited cli.toml.
        assert!(go(&["new", "play"], &env).await.unwrap_err().message.contains("purge it first"));
        let path = env.cli_toml.as_deref().unwrap();
        let mut saved = settings::load(path).unwrap();
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        saved.vaults.insert("play".into(), Vault { path: other, made: true });
        settings::save(path, &saved).unwrap();
        let e = go(&["remove", "play"], &env).await.unwrap_err();
        assert!(e.message.contains("purge it first"), "{}", e.message);
        let saved = settings::load(path).unwrap();
        assert_eq!(saved.removed_vaults["play"].path, tmp.path().join("data/shimmer-vaults/play"), "the first kept");
    }

    #[tokio::test]
    async fn use_refuses_a_missing_folder_and_warns_about_shimmer_home() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = env(tmp.path());
        go(&["new", "play"], &env).await.unwrap();
        std::fs::remove_dir(tmp.path().join("data/shimmer-vaults/play")).unwrap();
        assert!(go(&["use", "play"], &env).await.unwrap_err().message.contains("isn't there"));
        env.env_home = Some("/somewhere".into());
        assert!(go(&["use", "main"], &env).await.unwrap().contains("SHIMMER_HOME is set"));
    }
}
