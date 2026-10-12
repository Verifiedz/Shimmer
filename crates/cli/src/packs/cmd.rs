//! `shimmer packs …` (ADR 0013 §11): list, show, check and switch command packs, and manage the
//! bare commands they add. None of it talks to the daemon: packs live entirely in the client.

use std::path::{Path, PathBuf};

use shimmer_core::{Error, Result};

use super::active::{find, FindError};
use super::builtin::{builtin, BUILTIN};
use super::link::{Linker, Report};
use super::pack::{check_folder, Pack, NO_PACK, TARGETS};
use super::settings::{self, Settings};
use crate::render::table;

pub const USAGE: &str = "usage: shimmer packs <command>

commands:
  list                     every pack, built in or yours, and which one is active
  show NAME                a pack's aliases and the command each one runs
  use NAME [--no-link]     turn a pack on. Its aliases then work on their own
                           (ikuzo deep-work) as well as after shimmer; --no-link: only
                           after shimmer (shimmer ikuzo deep-work)
  use none                 turn packs off and remove the commands they added
  link                     add the active pack's commands again, e.g. after moving shimmer
  unlink                   remove them (aliases still work as 'shimmer <alias>')
  check NAME|FOLDER        check a pack and list every problem

An alias stands for shimmer plus a whole command. In pack.toml it names one of these:
  core.ping  core.shutdown  core.manifest
  records.collections  records.add  records.list  records.get  records.update
  records.complete  records.reopen  records.rename  records.remove  records.restore
  records.templates  records.create_collection  records.trash  records.purge
  records.import  records.export  records.check  records.rename_field
  records.rename_collection  records.remove_collection  records.restore_collection
  records.peek  records.copy_collection  records.edit
  workspaces.list  workspaces.status  workspaces.activate  workspaces.cleanup
  workspaces.force_relaunch  workspaces.reset  workspaces.stop  workspaces.remove
  workspaces.restore  workspaces.rename  workspaces.copy  workspaces.reconfigure
  workspaces.edit  workspaces.templates  workspaces.peek  workspaces.create
  queue.list  queue.task  queue.cancel  queue.reorder
  scheduler.list  scheduler.get  scheduler.add  scheduler.pause  scheduler.resume  scheduler.remove

Your own packs go in packs/<name>/pack.toml in your Shimmer folder (see docs/packs/README.md).
The active pack is kept in ~/.config/shimmer/cli.toml; aliases are linked into ~/.local/bin.";

#[derive(Clone, Debug, PartialEq)]
pub enum PacksCmd {
    Help,
    List,
    Show { name: String },
    Use { name: String, link: bool },
    Link,
    Unlink,
    Check { target: String },
}

/// `rest` is what follows `shimmer packs`.
pub fn parse(rest: Vec<String>) -> std::result::Result<PacksCmd, String> {
    let mut words = rest.into_iter();
    let Some(verb) = words.next() else { return Ok(PacksCmd::Help) };
    let (flags, args): (Vec<String>, Vec<String>) = words.partition(|w| w.starts_with('-') && w.len() > 1);
    let no_link = flags.iter().any(|f| f == "--no-link");
    if let Some(bad) = flags.iter().find(|f| !(verb == "use" && *f == "--no-link")) {
        return Err(format!("unknown option '{bad}' for 'packs {verb}'"));
    }
    let one = |what: &str| match args.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("'packs {verb}' needs {what}")),
        _ => Err(format!("'packs {verb}' takes one {what}")),
    };
    let none = || if args.is_empty() { Ok(()) } else { Err(format!("'packs {verb}' takes no arguments")) };
    match verb.as_str() {
        "help" => none().map(|()| PacksCmd::Help),
        "list" => none().map(|()| PacksCmd::List),
        "link" => none().map(|()| PacksCmd::Link),
        "unlink" => none().map(|()| PacksCmd::Unlink),
        "show" => one("a pack name").map(|name| PacksCmd::Show { name }),
        "check" => one("a pack name or folder").map(|target| PacksCmd::Check { target }),
        "use" => one("a pack name, or 'none'").map(|name| PacksCmd::Use { name, link: !no_link }),
        other => Err(format!("unknown packs command '{other}'")),
    }
}

/// Everything the commands read from outside: spelled out so tests can point it at temp folders.
pub struct Env {
    /// `$SHIMMER_HOME`, whose `packs/` holds people's own packs.
    pub home: PathBuf,
    /// Where `cli.toml` lives; `None` if neither `XDG_CONFIG_HOME` nor `HOME` is set.
    pub cli_toml: Option<PathBuf>,
    /// The link folder when `cli.toml` doesn't name one.
    pub default_link_dir: Option<PathBuf>,
    /// The real `shimmer` binary, symlinks resolved: what links point at.
    pub exe: std::io::Result<PathBuf>,
    pub path_dirs: Vec<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            home: shimmer_proto::paths::shimmer_home(),
            cli_toml: settings::path(),
            default_link_dir: settings::default_link_dir(),
            exe: std::env::current_exe().and_then(|p| p.canonicalize()),
            path_dirs: std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default(),
        }
    }
}

pub fn run(cmd: &PacksCmd, env: &Env) -> Result<String> {
    match cmd {
        PacksCmd::Help => Ok(USAGE.into()),
        PacksCmd::List => Ok(list(env)),
        PacksCmd::Show { name } => show(name, env),
        PacksCmd::Check { target } => check(target, env),
        PacksCmd::Use { name, link } => use_pack(name, *link, env),
        PacksCmd::Link => relink(env, true),
        PacksCmd::Unlink => relink(env, false),
    }
}

// ---------------------------------------------------------------- list, show, check

fn list(env: &Env) -> String {
    let active = env.cli_toml.as_deref().and_then(|p| settings::load(p).ok()).and_then(|s| s.pack);
    let folders = folder_packs(&env.home);
    let mut rows = Vec::new();
    let mut row = |id: &str, label: &str, aliases: String, from: String| {
        let mark = if active.as_deref() == Some(id) { "*" } else { "" };
        rows.push(vec![mark.into(), id.into(), label.into(), aliases, from]);
    };
    for (id, _) in BUILTIN {
        if folders.iter().any(|(f, _)| f == id) {
            continue; // listed below, as the folder that replaces it
        }
        if let Some(pack) = builtin(id) {
            row(id, &pack.label, pack.aliases.len().to_string(), "built in".into());
        }
    }
    for (id, checked) in &folders {
        let replaces = if builtin(id).is_some() { ", replaces the built-in" } else { "" };
        match checked {
            Ok(pack) => row(id, &pack.label, pack.aliases.len().to_string(), format!("packs/{id}{replaces}")),
            Err(problems) => row(id, "-", "-".into(), format!("invalid: {}", first_problem(problems))),
        }
    }
    let mut out = table(&["".into(), "PACK".into(), "LABEL".into(), "ALIASES".into(), "FROM".into()], &rows);
    out.push('\n');
    out.push_str(&match active {
        Some(id) => format!("\n* active: {id}. See its aliases with: shimmer packs show {id}"),
        None => "\nNo pack is active. Turn one on with: shimmer packs use NAME".into(),
    });
    out
}

/// Every folder in `$SHIMMER_HOME/packs/`, sorted, each checked.
fn folder_packs(home: &Path) -> Vec<(String, std::result::Result<Pack, Vec<String>>)> {
    let Ok(entries) = std::fs::read_dir(home.join("packs")) else { return Vec::new() };
    let mut out: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .map(|name| {
            let checked = check_folder(&home.join("packs").join(&name));
            (name, checked)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn show(name: &str, env: &Env) -> Result<String> {
    let pack = find(name, &env.home).map_err(find_error)?;
    let from = if env.home.join("packs").join(name).is_dir() { format!("packs/{name}") } else { "built in".into() };
    let rows: Vec<Vec<String>> = TARGETS
        .iter()
        .flat_map(|t| {
            pack.aliases
                .iter()
                .filter(move |(_, a)| a.op == t.op)
                .map(move |(alias, _)| vec![alias.clone(), format!("shimmer {}", t.words.join(" "))])
        })
        .collect();
    Ok(format!(
        "{}  ({}, {from})\n\n{}\n\nTurn it on with: shimmer packs use {}",
        pack.id,
        pack.label,
        table(&["ALIAS".into(), "RUNS".into()], &rows),
        pack.id
    ))
}

fn check(target: &str, env: &Env) -> Result<String> {
    let looks_like_path = target.contains('/') || target.starts_with('.');
    let dir = if looks_like_path { PathBuf::from(target) } else { env.home.join("packs").join(target) };
    if dir.is_dir() {
        let what = if looks_like_path { dir.display().to_string() } else { target.to_string() };
        let pack = check_folder(&dir).map_err(|problems| invalid(&what, &problems))?;
        return Ok(format!("✓ {}: {}, no problems", pack.id, aliases(&pack)));
    }
    match (looks_like_path, builtin(target)) {
        (false, Some(pack)) => Ok(format!("✓ {}: built in, {}, no problems", pack.id, aliases(&pack))),
        _ => Err(Error::not_found(format!("no pack folder at {} and no built-in pack '{target}'", dir.display()))),
    }
}

// ---------------------------------------------------------------- use, link, unlink

fn use_pack(name: &str, link: bool, env: &Env) -> Result<String> {
    let (path, mut saved) = load_settings(env)?;
    let pack = if name == NO_PACK { None } else { Some(find(name, &env.home).map_err(find_error)?) };
    let want: Vec<String> = match &pack {
        Some(pack) if link => pack.aliases.keys().cloned().collect(),
        _ => Vec::new(),
    };
    let report = sync_links(env, &saved, &want)?;
    saved.pack = pack.as_ref().map(|p| p.id.clone());
    save(&path, &mut saved, env, &report)?;

    let mut out = match &pack {
        Some(p) => format!("✓ active pack: {} ({})", p.id, p.label),
        None => "✓ packs off: plain shimmer commands only".into(),
    };
    out.push_str(&describe(&report, &saved, env));
    if let Some(pack) = &pack {
        out.push_str(&try_it(pack, &report));
    }
    Ok(out)
}

/// `packs link` (`add` true) or `packs unlink` for the active pack.
fn relink(env: &Env, add: bool) -> Result<String> {
    let (path, mut saved) = load_settings(env)?;
    let want: Vec<String> = match (&saved.pack, add) {
        (Some(id), true) => find(id, &env.home).map_err(find_error)?.aliases.keys().cloned().collect(),
        (None, true) => {
            return Err(Error::invalid_params("no pack is active; turn one on with: shimmer packs use NAME"));
        }
        (_, false) => Vec::new(),
    };
    let report = sync_links(env, &saved, &want)?;
    save(&path, &mut saved, env, &report)?;
    let mut out = if add {
        format!("✓ commands for {}", saved.pack.as_deref().unwrap_or("-"))
    } else {
        "✓ commands removed".into()
    };
    out.push_str(&describe(&report, &saved, env));
    Ok(out)
}

fn sync_links(env: &Env, saved: &Settings, want: &[String]) -> Result<Report> {
    let dir = link_dir(saved, env)?;
    if want.is_empty() && saved.linked.is_empty() {
        return Ok(Report { dir_on_path: true, ..Report::default() });
    }
    let exe = env
        .exe
        .as_deref()
        .map_err(|e| Error::unavailable(format!("can't find the shimmer program to link to: {e}")))?;
    Ok(Linker { dir: &dir, exe, path_dirs: &env.path_dirs }.sync(want, &saved.linked))
}

/// Save the new `linked` list. If that fails, put the link folder back as it was (remove the
/// links just added, re-create the ones just removed), so a switch that can't be recorded changes
/// nothing: `cli.toml` never forgets a link it made, never claims one it didn't, and the previous
/// pack's commands still work (#70 review).
fn save(path: &Path, saved: &mut Settings, env: &Env, report: &Report) -> Result<()> {
    let before = std::mem::replace(&mut saved.linked, report.linked.clone());
    settings::save(path, saved).map_err(|e| {
        if let (Ok(dir), Ok(exe)) = (link_dir(saved, env), env.exe.as_deref()) {
            Linker { dir: &dir, exe, path_dirs: &env.path_dirs }.undo(report);
        }
        saved.linked = before;
        Error::unavailable(format!("couldn't save {}: {e}", path.display()))
    })
}

fn load_settings(env: &Env) -> Result<(PathBuf, Settings)> {
    let path = env
        .cli_toml
        .clone()
        .ok_or_else(|| Error::unavailable("can't find your config folder: neither XDG_CONFIG_HOME nor HOME is set"))?;
    let saved = settings::load(&path).map_err(|problems| {
        Error::invalid_params(format!("{}\nFix or delete {} and try again.", problems.join("\n"), path.display()))
    })?;
    Ok((path, saved))
}

fn link_dir(saved: &Settings, env: &Env) -> Result<PathBuf> {
    saved.link_dir.clone().or_else(|| env.default_link_dir.clone()).ok_or_else(|| {
        Error::unavailable("can't find a folder for the commands: HOME is not set and cli.toml has no link_dir")
    })
}

/// The lines after "✓ …": what was linked, removed, skipped or failed, and a `PATH` reminder.
fn describe(r: &Report, saved: &Settings, env: &Env) -> String {
    let dir = link_dir(saved, env).map(|d| d.display().to_string()).unwrap_or_default();
    let mut out = String::new();
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    if !r.added.is_empty() {
        out.push_str(&format!("\n  linked {} command{} into {dir}", r.added.len(), plural(r.added.len())));
    }
    if !r.removed.is_empty() {
        out.push_str(&format!("\n  removed {} command{} from {dir}", r.removed.len(), plural(r.removed.len())));
    }
    for (name, why) in &r.skipped {
        out.push_str(&format!("\n  skipped {name}: it {why}; use 'shimmer {name}' instead"));
    }
    for (name, why) in &r.failed {
        out.push_str(&format!("\n  {name}: {why}"));
    }
    if !r.linked.is_empty() && !r.dir_on_path {
        out.push_str(&format!(
            "\n  note: {dir} isn't on your PATH, so the commands won't run on their own until you add it"
        ));
    }
    out
}

/// A first command to try with the new pack: its ping alias.
fn try_it(pack: &Pack, r: &Report) -> String {
    let Some((alias, _)) = pack.aliases.iter().find(|(_, t)| t.op == "core.ping") else { return String::new() };
    if r.linked.contains(alias) && r.dir_on_path {
        format!("\ntry it: {alias}")
    } else {
        format!("\ntry it: shimmer {alias}")
    }
}

fn aliases(pack: &Pack) -> String {
    match pack.aliases.len() {
        1 => "1 alias".into(),
        n => format!("{n} aliases"),
    }
}

fn find_error(e: FindError) -> Error {
    match e {
        FindError::Unknown(_) => Error::not_found(e.to_string()),
        FindError::BadName { .. } => Error::invalid_params(e.to_string()),
        FindError::Invalid { ref name, ref problems } => invalid(name, problems),
    }
}

fn invalid(what: &str, problems: &[String]) -> Error {
    let n = problems.len();
    Error::invalid_params(format!(
        "pack '{what}' has {n} problem{}:\n  {}",
        if n == 1 { "" } else { "s" },
        problems.join("\n  ")
    ))
}

fn first_problem(problems: &[String]) -> String {
    let first = problems.first().map(String::as_str).unwrap_or("");
    let first = first.split_once(": ").map_or(first, |(_, why)| why);
    if problems.len() > 1 {
        format!("{first} (+{} more)", problems.len() - 1)
    } else {
        first.into()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn p(s: &[&str]) -> std::result::Result<PacksCmd, String> {
        parse(s.iter().map(|w| w.to_string()).collect())
    }

    #[test]
    fn commands_parse() {
        assert_eq!(p(&[]), Ok(PacksCmd::Help));
        assert_eq!(p(&["list"]), Ok(PacksCmd::List));
        assert_eq!(p(&["use", "short"]), Ok(PacksCmd::Use { name: "short".into(), link: true }));
        assert_eq!(p(&["use", "short", "--no-link"]), Ok(PacksCmd::Use { name: "short".into(), link: false }));
        assert_eq!(p(&["check", "./mine"]), Ok(PacksCmd::Check { target: "./mine".into() }));
        assert!(p(&["use"]).unwrap_err().contains("needs a pack name"));
        assert!(p(&["show", "a", "b"]).unwrap_err().contains("takes one"));
        assert!(p(&["list", "x"]).unwrap_err().contains("takes no arguments"));
        assert!(p(&["link", "--no-link"]).unwrap_err().contains("unknown option '--no-link'"));
        assert!(p(&["frob"]).unwrap_err().contains("unknown packs command"));
    }

    #[test]
    fn usage_lists_exactly_the_commands_a_pack_can_name() {
        // Both ways: nothing missing, and nothing stale or misspelled.
        let list = USAGE.split("names one of these:").nth(1).expect("the op list in packs help");
        let list = list.split("\n\n").next().unwrap();
        let listed: Vec<&str> = list.split_whitespace().collect();
        let targets: Vec<&str> = TARGETS.iter().map(|t| t.op).collect();
        assert_eq!(listed, targets, "packs help's op list and TARGETS differ");
    }

    #[test]
    fn every_command_in_the_help_can_be_aliased() {
        // A command added to records, workspaces, queue or scheduler must get a target too, or a
        // pack can't name it. Read from each one's own help, its "commands:" section.
        for (top, usage) in [
            ("records", crate::records::USAGE),
            ("workspaces", crate::workspaces::USAGE),
            ("queue", crate::queue::USAGE),
            ("scheduler", crate::scheduler::USAGE),
        ] {
            let commands = usage.split("commands:\n").nth(1).unwrap().split("\n\n").next().unwrap();
            let words: BTreeSet<&str> = commands
                .lines()
                .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
                .filter_map(|l| l.split_whitespace().next())
                .collect();
            assert!(!words.is_empty(), "no commands found in {top}'s help");
            for word in words {
                let named = TARGETS.iter().any(|t| t.words == [top, word]);
                assert!(named, "'shimmer {top} {word}' has no target in packs/pack.rs, so no pack can alias it");
            }
        }
    }

    struct T {
        _tmp: tempfile::TempDir,
        env: Env,
        bin: PathBuf,
    }

    fn t() -> T {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (bin, exe) = (root.join("bin"), root.join("shimmer"));
        std::fs::write(&exe, "binary").unwrap();
        std::fs::create_dir_all(root.join("home/packs")).unwrap();
        let env = Env {
            home: root.join("home"),
            cli_toml: Some(root.join("config/shimmer/cli.toml")),
            default_link_dir: Some(bin.clone()),
            exe: Ok(exe),
            path_dirs: vec![bin.clone()],
        };
        T { _tmp: tmp, env, bin }
    }

    fn run_ok(t: &T, cmd: PacksCmd) -> String {
        run(&cmd, &t.env).unwrap_or_else(|e| panic!("{cmd:?}: {e}"))
    }

    fn saved(t: &T) -> Settings {
        settings::load(t.env.cli_toml.as_ref().unwrap()).unwrap()
    }

    fn links(t: &T) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&t.bin)
            .map(|d| d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn use_turns_a_pack_on_links_it_and_switching_swaps_the_links() {
        let t = t();
        let out = run_ok(&t, PacksCmd::Use { name: "short".into(), link: true });
        assert!(out.starts_with("✓ active pack: short (Short)\n  linked 53 commands into"), "{out}");
        assert!(out.ends_with("try it: up"), "{out}");
        assert_eq!(saved(&t).pack.as_deref(), Some("short"));
        assert_eq!(links(&t).len(), 53);
        assert!(links(&t).contains(&"wgo".to_string()));

        let out = run_ok(&t, PacksCmd::Use { name: "ship-it".into(), link: true });
        assert!(out.contains("linked 53") && out.contains("removed 53"), "{out}");
        assert!(links(&t).contains(&"deploy".to_string()) && !links(&t).contains(&"wgo".to_string()));
        assert_eq!(saved(&t).linked.len(), 53);

        let out = run_ok(&t, PacksCmd::Use { name: "none".into(), link: true });
        assert!(out.starts_with("✓ packs off") && out.contains("removed 53"), "{out}");
        assert_eq!((saved(&t), links(&t)), (Settings::default(), vec![]));
    }

    #[test]
    fn no_link_switches_without_commands_and_clears_old_ones() {
        let t = t();
        run_ok(&t, PacksCmd::Use { name: "short".into(), link: true });
        let out = run_ok(&t, PacksCmd::Use { name: "starship".into(), link: false });
        assert!(out.contains("removed 53") && out.ends_with("try it: shimmer comms"), "{out}");
        assert_eq!((saved(&t).pack.as_deref(), links(&t)), (Some("starship"), vec![]));
        // `packs link` adds them later; `unlink` takes them away and keeps the pack.
        assert!(run_ok(&t, PacksCmd::Link).contains("linked 53"));
        assert!(run_ok(&t, PacksCmd::Unlink).contains("removed 53"));
        assert_eq!((saved(&t).pack.as_deref(), links(&t)), (Some("starship"), vec![]));
    }

    #[test]
    fn use_refuses_an_unknown_or_broken_pack_and_changes_nothing() {
        let t = t();
        run_ok(&t, PacksCmd::Use { name: "short".into(), link: true });
        let e = run(&PacksCmd::Use { name: "nope".into(), link: true }, &t.env).unwrap_err();
        assert!(e.message.contains("no pack named 'nope'"), "{e}");

        let dir = t.env.home.join("packs/mine");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pack.toml"),
            "[pack]\nid = \"mine\"\nlabel = \"M\"\n[alias]\n\"ping\" = \"core.ping\"\n",
        )
        .unwrap();
        let e = run(&PacksCmd::Use { name: "mine".into(), link: true }, &t.env).unwrap_err();
        assert!(e.message.contains("has 1 problem") && e.message.contains("is a shimmer command word"), "{e}");
        assert_eq!((saved(&t).pack.as_deref(), links(&t).len()), (Some("short"), 53), "nothing changed");
    }

    #[test]
    fn a_switch_that_cant_be_saved_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let t = t();
        run_ok(&t, PacksCmd::Use { name: "short".into(), link: true });
        let before = links(&t);
        // cli.toml can still be read, but its folder can't be written: the save fails.
        let config = t.env.cli_toml.as_ref().unwrap().parent().unwrap().to_path_buf();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o555)).unwrap();
        let e = run(&PacksCmd::Use { name: "starship".into(), link: true }, &t.env).unwrap_err();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(e.message.starts_with("couldn't save"), "{e}");
        assert_eq!(links(&t), before, "the old pack's links are back, the new pack's are gone");
        assert_eq!(saved(&t).pack.as_deref(), Some("short"));
        assert_eq!(saved(&t).linked.len(), 53);
    }

    #[test]
    fn a_broken_cli_toml_is_never_overwritten() {
        let t = t();
        let path = t.env.cli_toml.clone().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "pack = 3\n").unwrap();
        let e = run(&PacksCmd::Use { name: "short".into(), link: true }, &t.env).unwrap_err();
        assert!(e.message.contains("Fix or delete"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "pack = 3\n");
    }

    #[test]
    fn link_needs_an_active_pack() {
        let t = t();
        let e = run(&PacksCmd::Link, &t.env).unwrap_err();
        assert!(e.message.contains("no pack is active"), "{e}");
        assert!(run_ok(&t, PacksCmd::Unlink).starts_with("✓ commands removed"), "unlink with nothing is fine");
    }

    #[test]
    fn list_show_and_check() {
        let t = t();
        let dir = t.env.home.join("packs");
        for (id, alias) in [("short", "zz"), ("mine", "hello")] {
            std::fs::create_dir_all(dir.join(id)).unwrap();
            let toml = format!("[pack]\nid = \"{id}\"\nlabel = \"Mine\"\n[alias]\n\"{alias}\" = \"core.ping\"\n");
            std::fs::write(dir.join(id).join("pack.toml"), toml).unwrap();
        }
        std::fs::create_dir_all(dir.join("broken")).unwrap();
        run_ok(&t, PacksCmd::Use { name: "mine".into(), link: false });

        let out = run_ok(&t, PacksCmd::List);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0].split_whitespace().collect::<Vec<_>>(), ["PACK", "LABEL", "ALIASES", "FROM"]);
        assert!(out.contains("anime-tropes") && out.contains("built in"), "{out}");
        assert!(out.contains("packs/short, replaces the built-in"), "{out}");
        assert!(lines.iter().any(|l| l.starts_with("*  mine")), "{out}");
        assert!(out.contains("invalid: doesn't exist"), "{out}");
        assert!(out.ends_with("* active: mine. See its aliases with: shimmer packs show mine"), "{out}");

        let out = run_ok(&t, PacksCmd::Show { name: "anime-tropes".into() });
        assert!(out.starts_with("anime-tropes  (Anime Tropes, built in)"), "{out}");
        assert!(
            out.contains("ikuzo         shimmer workspaces activate")
                && out.contains("mada-mada     shimmer queue list"),
            "{out}"
        );

        assert_eq!(run_ok(&t, PacksCmd::Check { target: "mine".into() }), "✓ mine: 1 alias, no problems");
        assert!(run_ok(&t, PacksCmd::Check { target: "starship".into() }).contains("built in"));
        let e = run(&PacksCmd::Check { target: "broken".into() }, &t.env).unwrap_err();
        assert!(e.message.contains("has 1 problem") && e.message.contains("pack.toml: doesn't exist"), "{e}");
        let e = run(&PacksCmd::Check { target: "./nowhere".into() }, &t.env).unwrap_err();
        assert!(e.message.contains("no pack folder at ./nowhere"), "{e}");
    }
}
