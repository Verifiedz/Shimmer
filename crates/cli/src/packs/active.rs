//! Which pack is active, and turning an alias into the command it stands for (ADR 0013 §3, §4,
//! §8, §9).
//!
//! Finding the pack never fails a command: every problem becomes a warning and the command runs
//! with canonical names (§8). Only the first command word is ever rewritten, and it is rewritten
//! before anything is parsed, so errors and help always use canonical names (CLAUDE.md §12
//! rule 16) and the daemon never sees an alias.

use std::path::Path;

use super::builtin::builtin;
use super::pack::{check_folder, name_problem, Pack, NO_PACK, TARGETS};
use super::settings;

/// The command line with the pack flags taken off, and what they said.
#[derive(Debug, PartialEq)]
pub struct Front {
    /// The arguments for the normal parser: `--pack` and `--canonical` removed.
    pub args: Vec<String>,
    /// `--pack NAME`, if given before the command word.
    pub pack: Option<String>,
    /// `--canonical`: help without the active pack's aliases.
    pub canonical: bool,
}

/// Flags `shimmer` itself takes before the command word, and whether each takes a value.
const GLOBAL_FLAGS: &[(&str, bool)] = &[
    ("--socket", true),
    ("--json", false),
    ("-h", false),
    ("--help", false),
    ("--pack", true),
    ("--canonical", false),
];

/// Take `--pack NAME` and `--canonical` off the front of the command line. They only count
/// before the command word, so `shimmer records add … --pack x` still reaches a records field
/// called `pack`.
pub fn take_front_flags(args: Vec<String>) -> Result<Front, String> {
    let mut front = Front { args: Vec::with_capacity(args.len()), pack: None, canonical: false };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = split_flag(&arg);
        match (flag, GLOBAL_FLAGS.iter().find(|(f, _)| *f == flag)) {
            ("--pack", _) => {
                let value = inline.map(str::to_string).or_else(|| args.next()).filter(|v| !v.is_empty());
                front.pack = Some(value.ok_or("--pack needs a value: a pack name, or 'none'")?);
            }
            ("--canonical", _) => front.canonical = true,
            (_, Some((_, true))) => {
                front.args.push(arg.clone());
                if inline.is_none() {
                    front.args.extend(args.next());
                }
            }
            (_, Some((_, false))) => front.args.push(arg),
            // The command word, or something the normal parser will judge: stop here.
            _ => {
                front.args.push(arg);
                front.args.extend(args);
                break;
            }
        }
    }
    Ok(front)
}

/// Replace the command word with the words of the command it stands for, if it is an alias in
/// `pack`. Everything else on the line stays exactly as typed:
/// `ikuzo deep-work --wait` → `workspaces activate deep-work --wait`.
pub fn rewrite(mut args: Vec<String>, pack: Option<&Pack>) -> Vec<String> {
    let (Some(pack), Some(i)) = (pack, command_word(&args)) else { return args };
    if let Some(target) = pack.aliases.get(&args[i]) {
        args.splice(i..=i, target.words.iter().map(|w| w.to_string()));
    }
    args
}

/// Where the command word is, after the global flags. `None` if there isn't one, or if an
/// unknown flag comes first (the normal parser reports that).
pub fn command_word(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let (flag, inline) = split_flag(arg);
        match GLOBAL_FLAGS.iter().find(|(f, _)| *f == flag) {
            Some((_, true)) if inline.is_none() => i += 2,
            Some(_) => i += 1,
            None if arg.starts_with('-') && arg.len() > 1 => return None,
            None => return Some(i),
        }
    }
    None
}

/// `--flag=value` → (`--flag`, `Some("value")`); anything else → (itself, `None`).
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
        _ => (arg, None),
    }
}

/// The active pack, and anything worth warning about while finding it.
#[derive(Debug, Default)]
pub struct Active {
    pub pack: Option<Pack>,
    pub warnings: Vec<String>,
}

/// The active pack for this run: `--pack` if given, else `cli.toml`'s choice, else none.
pub fn load(flag: Option<&str>) -> Active {
    resolve(flag, settings::path().as_deref(), &shimmer_proto::paths::shimmer_home())
}

/// [`load`] with its inputs spelled out, for tests.
pub fn resolve(flag: Option<&str>, cli_toml: Option<&Path>, home: &Path) -> Active {
    let mut active = Active::default();
    let name = match flag {
        Some(name) => Some(name.to_string()),
        None => cli_toml.and_then(|path| chosen_in(path, &mut active.warnings)),
    };
    match name.as_deref() {
        None | Some(NO_PACK) => {}
        Some(name) => match find(name, home) {
            Ok(pack) => active.pack = Some(pack),
            Err(e) => active.warnings.push(format!("{e}; using no aliases")),
        },
    }
    active
}

/// The pack `cli.toml` chose, warning about anything wrong with the file. A missing file just
/// means nothing was chosen.
fn chosen_in(path: &Path, warnings: &mut Vec<String>) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warnings.push(format!("can't read {}: {e}; using no aliases", path.display()));
            return None;
        }
    };
    match settings::parse(&path.display().to_string(), &text) {
        Ok(loaded) => {
            warnings.extend(loaded.warnings);
            loaded.settings.pack
        }
        Err(problems) => {
            warnings.push(format!("{}{}; using no aliases", problems[0], more(problems.len())));
            None
        }
    }
}

/// Why a pack couldn't be found or used.
#[derive(Debug, PartialEq)]
pub enum FindError {
    /// The name itself isn't a valid pack id.
    BadName { name: String, why: String },
    /// No folder and no built-in has that name.
    Unknown(String),
    /// The folder pack fails its checks: every problem.
    Invalid { name: String, problems: Vec<String> },
}

impl std::fmt::Display for FindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName { name, why } => write!(f, "no pack '{name}': a pack name {why}"),
            Self::Unknown(name) => write!(f, "no pack named '{name}' (see 'shimmer packs list')"),
            Self::Invalid { name, problems } => write!(
                f,
                "pack '{name}' not loaded ({} problem{}; see 'shimmer packs check {name}')",
                problems.len(),
                if problems.len() == 1 { "" } else { "s" }
            ),
        }
    }
}

/// Pack `name`: the folder `$SHIMMER_HOME/packs/<name>/` if there is one, which replaces a
/// built-in of the same name, else the built-in (ADR 0013 §4).
pub fn find(name: &str, home: &Path) -> Result<Pack, FindError> {
    if let Some(why) = name_problem(name) {
        return Err(FindError::BadName { name: name.into(), why });
    }
    let dir = home.join("packs").join(name);
    if dir.is_dir() {
        return check_folder(&dir).map_err(|problems| FindError::Invalid { name: name.into(), problems });
    }
    builtin(name).ok_or_else(|| FindError::Unknown(name.into()))
}

/// The section `shimmer --help` adds for the active pack: each alias and the command it runs,
/// in the order of the help above it.
pub fn help_section(pack: &Pack) -> String {
    let width = pack.aliases.keys().map(String::len).max().unwrap_or(0);
    let mut out = format!("aliases ({} pack; 'shimmer --help --canonical' hides these):\n", pack.id);
    for target in TARGETS {
        for (alias, _) in pack.aliases.iter().filter(|(_, t)| t.op == target.op) {
            out.push_str(&format!("  {alias:<width$}  shimmer {}\n", target.words.join(" ")));
        }
    }
    out.push_str("Run an alias as 'shimmer <alias> …', or on its own after 'shimmer packs use'.");
    out
}

/// Whether `name` could be an alias this binary was started as (ADR 0013 §10): one Shimmer
/// linked (`cli.toml` `linked`), or one in any built-in or folder pack. Anything else, such as a
/// renamed binary, runs as plain `shimmer`.
pub fn is_known_alias(name: &str, cli_toml: Option<&Path>, home: &Path) -> bool {
    if name_problem(name).is_some() {
        return false;
    }
    let linked = cli_toml.and_then(|p| settings::load(p).ok()).is_some_and(|s| s.linked.iter().any(|l| l == name));
    let in_builtin =
        super::builtin::BUILTIN.iter().filter_map(|(id, _)| builtin(id)).any(|p| p.aliases.contains_key(name));
    let in_folder = || {
        std::fs::read_dir(home.join("packs"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| check_folder(&e.path()).is_ok_and(|p| p.aliases.contains_key(name)))
    };
    linked || in_builtin || in_folder()
}

fn more(n: usize) -> String {
    if n > 1 {
        format!(" (and {} more)", n - 1)
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    fn short() -> Pack {
        builtin("short").unwrap()
    }

    fn rewritten(s: &[&str]) -> Vec<String> {
        rewrite(args(s), Some(&short()))
    }

    #[test]
    fn an_alias_becomes_the_whole_command_and_the_rest_is_untouched() {
        assert_eq!(
            rewritten(&["wgo", "deep-work", "--wait"]),
            args(&["workspaces", "activate", "deep-work", "--wait"])
        );
        assert_eq!(rewritten(&["up"]), args(&["ping"]));
        assert_eq!(rewritten(&["rdone", "leetcode/two-sum"]), args(&["records", "complete", "leetcode/two-sum"]));
        assert_eq!(rewritten(&["wforce", "x"]), args(&["workspaces", "force-relaunch", "x"]));
    }

    #[test]
    fn global_flags_before_the_alias_are_kept_and_skipped() {
        assert_eq!(
            rewritten(&["--json", "--socket", "/s", "wst", "x"]),
            args(&["--json", "--socket", "/s", "workspaces", "status", "x"])
        );
        assert_eq!(rewritten(&["--socket=/s", "up"]), args(&["--socket=/s", "ping"]));
        assert_eq!(rewritten(&["-h", "wgo"]), args(&["-h", "workspaces", "activate"]));
    }

    #[test]
    fn only_the_command_word_is_ever_rewritten() {
        // An argument that happens to be an alias stays an argument.
        assert_eq!(rewritten(&["workspaces", "activate", "wgo"]), args(&["workspaces", "activate", "wgo"]));
        assert_eq!(rewritten(&["records", "add", "leetcode", "up"]), args(&["records", "add", "leetcode", "up"]));
        // Canonical words, unknown words and an unknown flag first are left alone.
        for line in [&["ping"][..], &["nope"], &["--bogus", "up"], &[]] {
            assert_eq!(rewritten(line), args(line));
        }
        assert_eq!(rewrite(args(&["up"]), None), args(&["up"]), "no pack, no rewrite");
    }

    #[test]
    fn pack_and_canonical_come_off_the_front_only() {
        let f = take_front_flags(args(&["--pack", "short", "--json", "up"])).unwrap();
        assert_eq!(f, Front { args: args(&["--json", "up"]), pack: Some("short".into()), canonical: false });
        let f = take_front_flags(args(&["--pack=none", "--help", "--canonical"])).unwrap();
        assert_eq!(f, Front { args: args(&["--help"]), pack: Some("none".into()), canonical: true });
        let f = take_front_flags(args(&["--socket", "/s", "--pack", "starship", "comms"])).unwrap();
        assert_eq!((f.args, f.pack.as_deref()), (args(&["--socket", "/s", "comms"]), Some("starship")));
        // After the command word they belong to the command.
        let f = take_front_flags(args(&["records", "add", "c", "x", "--pack", "red"])).unwrap();
        assert_eq!((f.args.len(), f.pack), (6, None));
        assert!(take_front_flags(args(&["--pack"])).unwrap_err().contains("needs a value"));
        assert!(take_front_flags(args(&["--pack="])).unwrap_err().contains("needs a value"));
    }

    // ---------------------------------------------------------------- choosing the pack

    struct Dirs {
        _tmp: tempfile::TempDir,
        cli_toml: std::path::PathBuf,
        home: std::path::PathBuf,
    }

    fn dirs() -> Dirs {
        let tmp = tempfile::tempdir().unwrap();
        let (cli_toml, home) = (tmp.path().join("cli.toml"), tmp.path().join("home"));
        std::fs::create_dir_all(home.join("packs")).unwrap();
        Dirs { _tmp: tmp, cli_toml, home }
    }

    fn active(d: &Dirs, flag: Option<&str>) -> (Option<String>, Vec<String>) {
        let a = resolve(flag, Some(&d.cli_toml), &d.home);
        (a.pack.map(|p| p.id), a.warnings)
    }

    fn folder_pack(d: &Dirs, id: &str, alias: &str) {
        let dir = d.home.join("packs").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let toml = format!("[pack]\nid = \"{id}\"\nlabel = \"Mine\"\n\n[alias]\n\"{alias}\" = \"core.ping\"\n");
        std::fs::write(dir.join("pack.toml"), toml).unwrap();
    }

    #[test]
    fn no_cli_toml_means_no_pack_and_no_warning() {
        assert_eq!(active(&dirs(), None), (None, vec![]));
    }

    #[test]
    fn cli_toml_chooses_and_the_flag_overrides() {
        let d = dirs();
        std::fs::write(&d.cli_toml, "pack = \"short\"\n").unwrap();
        assert_eq!(active(&d, None), (Some("short".into()), vec![]));
        assert_eq!(active(&d, Some("starship")), (Some("starship".into()), vec![]));
        assert_eq!(active(&d, Some("none")), (None, vec![]));
    }

    #[test]
    fn a_folder_pack_loads_and_replaces_a_built_in_of_the_same_name() {
        let d = dirs();
        folder_pack(&d, "mine", "hello");
        folder_pack(&d, "short", "zz");
        let a = resolve(Some("mine"), None, &d.home);
        assert!(a.pack.unwrap().aliases.contains_key("hello"));
        let a = resolve(Some("short"), None, &d.home);
        assert_eq!(a.pack.unwrap().aliases.keys().collect::<Vec<_>>(), ["zz"]);
    }

    #[test]
    fn every_problem_becomes_one_warning_and_no_pack() {
        let d = dirs();
        let (pack, w) = active(&d, Some("nope"));
        assert_eq!(
            (pack, w),
            (None, vec!["no pack named 'nope' (see 'shimmer packs list'); using no aliases".to_string()])
        );

        let (pack, w) = active(&d, Some("Bad Name"));
        assert!(pack.is_none() && w[0].starts_with("no pack 'Bad Name': a pack name must start"), "{w:?}");

        let broken = d.home.join("packs/broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("pack.toml"), "[pack]\nid = \"broken\"\n[alias]\n\"ping\" = \"x\"\n").unwrap();
        let (pack, w) = active(&d, Some("broken"));
        assert!(pack.is_none());
        assert_eq!(w, ["pack 'broken' not loaded (3 problems; see 'shimmer packs check broken'); using no aliases"]);

        std::fs::write(&d.cli_toml, "pack = 3\nlink_dir = \"rel\"\n").unwrap();
        let (pack, w) = active(&d, None);
        assert!(pack.is_none());
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].ends_with("(and 1 more); using no aliases"), "{w:?}");
    }

    #[test]
    fn unknown_cli_toml_keys_warn_but_the_pack_still_loads() {
        let d = dirs();
        std::fs::write(&d.cli_toml, "pack = \"short\"\ncolour = \"red\"\n").unwrap();
        let (pack, w) = active(&d, None);
        assert_eq!(pack.as_deref(), Some("short"));
        assert!(w[0].ends_with("unknown key 'colour' ignored"), "{w:?}");
    }

    #[test]
    fn known_aliases_are_linked_built_in_or_in_a_folder_pack() {
        let d = dirs();
        folder_pack(&d, "mine", "hello");
        std::fs::write(&d.cli_toml, "linked = [\"old-one\"]\n").unwrap();
        for name in ["ikuzo", "wgo", "hello", "old-one"] {
            assert!(is_known_alias(name, Some(&d.cli_toml), &d.home), "{name}");
        }
        for name in ["shimmer-dev", "nope", "Bad Name", ""] {
            assert!(!is_known_alias(name, Some(&d.cli_toml), &d.home), "{name}");
        }
    }

    #[test]
    fn help_lists_aliases_in_command_order() {
        let help = help_section(&short());
        let lines: Vec<&str> = help.lines().collect();
        assert_eq!(lines[0], "aliases (short pack; 'shimmer --help --canonical' hides these):");
        assert_eq!(lines[1].trim_end(), "  up        shimmer ping");
        assert_eq!(lines[33].trim_end(), "  wreset    shimmer workspaces reset");
        assert_eq!(lines[53].trim_end(), "  sdel      shimmer scheduler remove");
        assert_eq!(lines.len(), 55);
    }
}
