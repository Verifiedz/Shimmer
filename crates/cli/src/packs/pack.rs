//! A command pack's `pack.toml` (ADR 0013 §1) and the checks every pack must pass (§6, §7).
//!
//! Checking reports every problem in a pack, not just the first, so someone writing one can fix
//! it in one go. Each problem names the file it is in.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use shimmer_core::ids::is_valid_name;
use toml::{Table, Value};

/// `pack.toml` larger than this is rejected unread.
pub const MAX_PACK_TOML: u64 = 64 * 1024;
/// Most aliases one pack may define.
pub const MAX_ALIASES: usize = 64;
/// Longest pack id or alias.
pub const MAX_NAME: usize = 32;
/// Longest pack label, in characters.
pub const MAX_LABEL: usize = 40;
/// Largest animation file.
pub const MAX_ANIM: u64 = 256 * 1024;

/// The pack id that means "no pack". No pack may use it.
pub const NO_PACK: &str = "none";

/// Words an alias may never be: the CLI's own command words, words held for commands we expect
/// (so a pack that is valid today can't break when they arrive), and the binary's own name.
pub const RESERVED: &[&str] = &[
    "ping",
    "manifest",
    "shutdown",
    "call",
    "daemon",
    "mockd",
    "records",
    "workspaces",
    "packs",
    "help",
    "queue",
    "scheduler",
    "fetchers",
    "notify",
    "calendar",
    "tui",
    "version",
    "shimmer",
];

/// One command a pack can name: the canonical op an alias points at, and the command words it
/// runs (ADR 0013 §6). `daemon`, `mockd`, `call` and `packs` are deliberately absent.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub op: &'static str,
    pub words: &'static [&'static str],
}

pub const TARGETS: &[Target] = &[
    Target { op: "core.ping", words: &["ping"] },
    Target { op: "core.shutdown", words: &["shutdown"] },
    Target { op: "core.manifest", words: &["manifest"] },
    Target { op: "records.collections", words: &["records", "collections"] },
    Target { op: "records.add", words: &["records", "add"] },
    Target { op: "records.list", words: &["records", "list"] },
    Target { op: "records.get", words: &["records", "get"] },
    Target { op: "records.update", words: &["records", "update"] },
    Target { op: "records.complete", words: &["records", "complete"] },
    Target { op: "records.reopen", words: &["records", "reopen"] },
    Target { op: "records.rename", words: &["records", "rename"] },
    Target { op: "records.remove", words: &["records", "remove"] },
    Target { op: "records.restore", words: &["records", "restore"] },
    // ADR 0029: every command, the rare and destructive ones too (they still ask first).
    Target { op: "records.templates", words: &["records", "templates"] },
    Target { op: "records.create_collection", words: &["records", "new"] },
    Target { op: "records.trash", words: &["records", "trash"] },
    Target { op: "records.purge", words: &["records", "purge"] },
    Target { op: "records.import", words: &["records", "import"] },
    Target { op: "records.export", words: &["records", "export"] },
    Target { op: "records.check", words: &["records", "check"] },
    Target { op: "records.rename_field", words: &["records", "rename-field"] },
    Target { op: "records.rename_collection", words: &["records", "rename-collection"] },
    Target { op: "records.remove_collection", words: &["records", "remove-collection"] },
    Target { op: "records.restore_collection", words: &["records", "restore-collection"] },
    Target { op: "workspaces.list", words: &["workspaces", "list"] },
    Target { op: "workspaces.status", words: &["workspaces", "status"] },
    Target { op: "workspaces.activate", words: &["workspaces", "activate"] },
    Target { op: "workspaces.cleanup", words: &["workspaces", "cleanup"] },
    Target { op: "workspaces.force_relaunch", words: &["workspaces", "force-relaunch"] },
    Target { op: "workspaces.reset", words: &["workspaces", "reset"] },
    Target { op: "workspaces.stop", words: &["workspaces", "stop"] },
    Target { op: "workspaces.remove", words: &["workspaces", "remove"] },
    Target { op: "workspaces.restore", words: &["workspaces", "restore"] },
    Target { op: "workspaces.rename", words: &["workspaces", "rename"] },
    Target { op: "workspaces.copy", words: &["workspaces", "copy"] },
    Target { op: "workspaces.reconfigure", words: &["workspaces", "reconfigure"] },
    Target { op: "workspaces.edit", words: &["workspaces", "edit"] },
    Target { op: "workspaces.templates", words: &["workspaces", "templates"] },
    Target { op: "workspaces.peek", words: &["workspaces", "peek"] },
    Target { op: "workspaces.create", words: &["workspaces", "new"] },
    // ADR 0015 §5: the queue and scheduler commands. A user's pack may leave these out; the
    // built-in packs name all of them.
    Target { op: "queue.list", words: &["queue", "list"] },
    Target { op: "queue.task", words: &["queue", "show"] },
    Target { op: "queue.cancel", words: &["queue", "cancel"] },
    Target { op: "queue.reorder", words: &["queue", "move"] },
    Target { op: "scheduler.list", words: &["scheduler", "list"] },
    Target { op: "scheduler.get", words: &["scheduler", "show"] },
    Target { op: "scheduler.add", words: &["scheduler", "add"] },
    Target { op: "scheduler.pause", words: &["scheduler", "pause"] },
    Target { op: "scheduler.resume", words: &["scheduler", "resume"] },
    Target { op: "scheduler.remove", words: &["scheduler", "remove"] },
];

/// The command `op` names, if a pack may alias it.
pub fn target(op: &str) -> Option<&'static Target> {
    TARGETS.iter().find(|t| t.op == op)
}

/// A pack that passed every check.
#[derive(Debug, PartialEq)]
pub struct Pack {
    pub id: String,
    pub label: String,
    /// Alias -> the command it runs, sorted by alias.
    pub aliases: BTreeMap<String, &'static Target>,
    /// Op -> its animation file, relative to the pack's folder (ADR 0013 §12).
    pub anim: BTreeMap<String, String>,
}

/// Check a `pack.toml`'s text. `file` names it in every problem, e.g. `my-pack/pack.toml`.
/// Animation files are only checked by [`check_folder`], which can see them.
pub fn parse(file: &str, text: &str) -> Result<Pack, Vec<String>> {
    let (pack, problems) = read(file, text);
    if problems.is_empty() {
        Ok(pack)
    } else {
        Err(problems)
    }
}

/// Check the pack in folder `dir`: its `pack.toml`, that its id matches the folder's name, and
/// that its animation files exist and stay inside the folder.
pub fn check_folder(dir: &Path) -> Result<Pack, Vec<String>> {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let file = format!("{name}/pack.toml");
    let text = match read_capped(&dir.join("pack.toml"), MAX_PACK_TOML) {
        Ok(text) => text,
        Err(e) => return Err(vec![format!("{file}: {e}")]),
    };
    let (pack, mut problems) = read(&file, &text);
    if !pack.id.is_empty() && pack.id != name {
        problems.push(format!("{file}: id '{}' must match the folder name '{name}'", pack.id));
    }
    for (op, rel) in &pack.anim {
        if let Err(e) = check_anim_file(dir, rel) {
            problems.push(format!("{file}: [anim] {op}: '{rel}' {e}"));
        }
    }
    if problems.is_empty() {
        Ok(pack)
    } else {
        Err(problems)
    }
}

/// Every check that needs only the text. Returns what was valid, plus every problem found.
fn read(file: &str, text: &str) -> (Pack, Vec<String>) {
    let mut pack = Pack { id: String::new(), label: String::new(), aliases: BTreeMap::new(), anim: BTreeMap::new() };
    let mut problems = Vec::new();
    let mut problem = |msg: String| problems.push(format!("{file}: {msg}"));

    if text.len() as u64 > MAX_PACK_TOML {
        problem(format!("larger than {} KiB", MAX_PACK_TOML / 1024));
        return (pack, problems);
    }
    let table: Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(e) => {
            problem(format!("not valid TOML: {}", e.message()));
            return (pack, problems);
        }
    };

    for (key, value) in &table {
        match (key.as_str(), value) {
            ("pack", Value::Table(t)) => read_pack(t, &mut pack, &mut problem),
            ("alias", Value::Table(t)) => read_aliases(t, &mut pack, &mut problem),
            ("anim", Value::Table(t)) => read_anim(t, &mut pack, &mut problem),
            ("pack" | "alias" | "anim", _) => problem(format!("'{key}' must be a table: [{key}]")),
            _ => problem(format!("unknown section or key '{key}'")),
        }
    }
    if !table.contains_key("pack") {
        problem("missing [pack] with an id and a label".into());
    }
    if !table.contains_key("alias") {
        problem("missing [alias]: a pack needs at least one alias".into());
    }
    (pack, problems)
}

fn read_pack(t: &Table, pack: &mut Pack, problem: &mut impl FnMut(String)) {
    for key in t.keys().filter(|k| !matches!(k.as_str(), "id" | "label")) {
        problem(format!("unknown key 'pack.{key}'"));
    }
    match t.get("id") {
        None => problem("[pack] id is missing".into()),
        Some(Value::String(id)) => match name_problem(id) {
            Some(why) => problem(format!("[pack] id '{id}' {why}")),
            None if id == NO_PACK => problem(format!("[pack] id '{NO_PACK}' is reserved (it means no pack)")),
            None => pack.id = id.clone(),
        },
        Some(_) => problem("[pack] id must be a string".into()),
    }
    match t.get("label") {
        None => problem("[pack] label is missing".into()),
        Some(Value::String(label)) => match label_problem(label) {
            Some(why) => problem(format!("[pack] label {why}")),
            None => pack.label = label.trim().to_string(),
        },
        Some(_) => problem("[pack] label must be a string".into()),
    }
}

fn read_aliases(t: &Table, pack: &mut Pack, problem: &mut impl FnMut(String)) {
    if t.is_empty() {
        problem("[alias] is empty: a pack needs at least one alias".into());
    }
    if t.len() > MAX_ALIASES {
        problem(format!("[alias] has {} entries; the most a pack may have is {MAX_ALIASES}", t.len()));
    }
    for (alias, value) in t {
        let mut ok = true;
        if let Some(why) = name_problem(alias) {
            problem(format!("alias '{alias}' {why}"));
            ok = false;
        } else if RESERVED.contains(&alias.as_str()) {
            problem(format!("alias '{alias}' is a shimmer command word; aliases can't replace those"));
            ok = false;
        }
        let Value::String(op) = value else {
            problem(format!("alias '{alias}' must name an op as a string, e.g. \"workspaces.activate\""));
            continue;
        };
        match target(op) {
            Some(target) if ok => {
                pack.aliases.insert(alias.clone(), target);
            }
            Some(_) => {}
            None => problem(format!(
                "alias '{alias}' points at '{op}', which isn't a command a pack can name; {SEE_TARGETS}"
            )),
        }
    }
}

fn read_anim(t: &Table, pack: &mut Pack, problem: &mut impl FnMut(String)) {
    for (op, value) in t {
        if target(op).is_none() {
            problem(format!("[anim] '{op}' isn't a command a pack can name; {SEE_TARGETS}"));
            continue;
        }
        let Value::String(rel) = value else {
            problem(format!("[anim] {op} must be a path string, e.g. \"anim/start.txt\""));
            continue;
        };
        match relative_path_problem(rel) {
            Some(why) => problem(format!("[anim] {op}: '{rel}' {why}")),
            None => {
                pack.anim.insert(op.clone(), rel.clone());
            }
        }
    }
}

/// Why `s` can't be a pack id or alias, if it can't.
pub fn name_problem(s: &str) -> Option<String> {
    if !is_valid_name(s) {
        Some("must start with a lowercase letter and use only a-z, 0-9, '-' and '_'".into())
    } else if s.len() > MAX_NAME {
        Some(format!("is longer than {MAX_NAME} characters"))
    } else {
        None
    }
}

fn label_problem(label: &str) -> Option<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        Some("must not be empty".into())
    } else if trimmed.chars().any(char::is_control) {
        Some("must be one line of plain text".into())
    } else if trimmed.chars().count() > MAX_LABEL {
        Some(format!("is longer than {MAX_LABEL} characters"))
    } else {
        None
    }
}

/// An animation path must stay inside the pack's folder: relative, no `..`, nothing odd.
fn relative_path_problem(rel: &str) -> Option<&'static str> {
    let path = Path::new(rel);
    if rel.is_empty() {
        Some("is empty")
    } else if path.is_absolute() || rel.starts_with('/') || rel.starts_with('\\') {
        Some("must be relative to the pack's folder")
    } else if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        Some("must stay inside the pack's folder (no '..' or '.')")
    } else {
        None
    }
}

/// The checks only the filesystem can answer. `rel` already passed [`relative_path_problem`];
/// resolving it here also catches a symlink that leads out of the folder.
fn check_anim_file(dir: &Path, rel: &str) -> Result<(), String> {
    let full = dir.join(rel).canonicalize().map_err(|_| "doesn't exist".to_string())?;
    let root = dir.canonicalize().map_err(|e| format!("can't be checked: {e}"))?;
    if !full.starts_with(&root) {
        return Err("leads outside the pack's folder".into());
    }
    read_capped(&full, MAX_ANIM).map(|_| ())
}

/// The text of `path`, if it is a regular file of at most `max` bytes and valid UTF-8.
fn read_capped(path: &Path, max: u64) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "doesn't exist".to_string(),
        _ => format!("can't be read: {e}"),
    })?;
    if !meta.is_file() {
        return Err("is not a file".into());
    }
    if meta.len() > max {
        return Err(format!("is larger than {} KiB", max / 1024));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("can't be read: {e}"))?;
    String::from_utf8(bytes).map_err(|_| "is not UTF-8 text".into())
}

/// Where to find the commands a pack can name ([`TARGETS`]), for problems about them.
const SEE_TARGETS: &str = "'shimmer packs --help' lists the ones that can";

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[pack]
id = "my-pack"
label = "My Pack"

[alias]
"go-time" = "workspaces.activate"
"finished" = "records.complete"
"#;

    fn problems(text: &str) -> Vec<String> {
        parse("my-pack/pack.toml", text).expect_err("should fail")
    }

    fn has(problems: &[String], part: &str) {
        assert!(problems.iter().any(|p| p.contains(part)), "no problem containing {part:?} in {problems:#?}");
    }

    fn with_alias(line: &str) -> String {
        format!("[pack]\nid = \"my-pack\"\nlabel = \"My Pack\"\n\n[alias]\n{line}\n")
    }

    #[test]
    fn a_good_pack_parses() {
        let pack = parse("my-pack/pack.toml", GOOD).unwrap();
        assert_eq!((pack.id.as_str(), pack.label.as_str()), ("my-pack", "My Pack"));
        assert_eq!(pack.aliases["go-time"].words, ["workspaces", "activate"]);
        assert_eq!(pack.aliases["finished"].op, "records.complete");
        assert!(pack.anim.is_empty());
    }

    #[test]
    fn every_target_is_an_op_with_its_command_words() {
        assert_eq!(TARGETS.len(), 50);
        // Where the CLI's word differs from the op's verb (ADR 0015 §1).
        let renamed = [
            ("queue.task", "show"),
            ("queue.reorder", "move"),
            ("records.create_collection", "new"),
            ("workspaces.create", "new"),
            ("scheduler.get", "show"),
        ];
        for t in TARGETS {
            let (module, verb) = t.op.split_once('.').unwrap();
            let verb = renamed.iter().find(|(op, _)| *op == t.op).map_or(verb, |(_, word)| word);
            let words: Vec<String> = t.words.iter().map(|w| w.replace('-', "_")).collect();
            match module {
                "core" => assert_eq!(words, [verb]),
                _ => assert_eq!(words, [module, verb]),
            }
        }
        assert_eq!(target("workspaces.force_relaunch").unwrap().words, ["workspaces", "force-relaunch"]);
        assert_eq!(target("queue.reorder").unwrap().words, ["queue", "move"]);
        for plumbing in ["core.daemon", "core.subscribe", "queue.promote", "scheduler.show", "packs.use"] {
            assert!(target(plumbing).is_none(), "{plumbing} must not be aliasable");
        }
    }

    #[test]
    fn every_problem_is_reported_not_just_the_first() {
        let text = r#"
[pack]
id = "Bad Id"
label = ""
colour = "red"

[alias]
"ping" = "core.ping"
"x y" = "records.add"
"go" = "daemon"

[extras]
"#;
        let p = problems(text);
        for part in [
            "[pack] id 'Bad Id'",
            "[pack] label must not be empty",
            "unknown key 'pack.colour'",
            "alias 'ping' is a shimmer command word",
            "alias 'x y' must start",
            "alias 'go' points at 'daemon'",
            "unknown section or key 'extras'",
        ] {
            has(&p, part);
        }
        assert!(p.iter().all(|m| m.starts_with("my-pack/pack.toml: ")), "{p:#?}");
    }

    #[test]
    fn invalid_toml_is_one_problem_naming_the_file() {
        let p = problems("[pack\nid = 1");
        assert_eq!(p.len(), 1, "{p:#?}");
        has(&p, "my-pack/pack.toml: not valid TOML");
    }

    #[test]
    fn a_repeated_alias_is_rejected() {
        has(&problems(&with_alias("\"a\" = \"core.ping\"\n\"a\" = \"core.shutdown\"")), "not valid TOML");
    }

    #[test]
    fn pack_and_alias_sections_are_required() {
        let p = problems("[anim]\n");
        has(&p, "missing [pack]");
        has(&p, "missing [alias]");
        has(&problems("[pack]\nid = \"my-pack\"\nlabel = \"x\"\n[alias]\n"), "[alias] is empty");
        has(&problems("pack = 1\nalias = 2\n"), "'pack' must be a table");
    }

    #[test]
    fn id_and_label_are_checked() {
        let pack =
            |id: &str, label: &str| format!("[pack]\nid = {id}\nlabel = {label}\n[alias]\n\"a\" = \"core.ping\"\n");
        has(&problems(&pack("\"none\"", "\"x\"")), "id 'none' is reserved");
        has(&problems(&pack("\"1pack\"", "\"x\"")), "must start with a lowercase letter");
        has(&problems(&pack(&format!("\"{}\"", "a".repeat(33)), "\"x\"")), "longer than 32");
        has(&problems(&pack("7", "\"x\"")), "id must be a string");
        has(&problems(&pack("\"p\"", &format!("\"{}\"", "x".repeat(41)))), "label is longer than 40");
        has(&problems(&pack("\"p\"", "\"two\\nlines\"")), "one line of plain text");
        has(&problems("[pack]\nlabel = \"x\"\n[alias]\n\"a\" = \"core.ping\"\n"), "[pack] id is missing");
        assert_eq!(parse("p", &pack("\"p\"", "\"  Padded  \"")).unwrap().label, "Padded");
    }

    #[test]
    fn aliases_are_checked() {
        for word in RESERVED {
            has(&problems(&with_alias(&format!("\"{word}\" = \"core.ping\""))), "is a shimmer command word");
        }
        has(&problems(&with_alias("\"-x\" = \"core.ping\"")), "must start with a lowercase letter");
        has(&problems(&with_alias("\"Caps\" = \"core.ping\"")), "must start with a lowercase letter");
        has(&problems(&with_alias(&format!("\"{}\" = \"core.ping\"", "a".repeat(33)))), "longer than 32");
        has(&problems(&with_alias("\"a\" = 1")), "must name an op as a string");
        has(&problems(&with_alias("\"a\" = \"records.nope\"")), "isn't a command a pack can name");
        has(&problems(&with_alias("\"a\" = \"workspaces activate\"")), "isn't a command a pack can name");
    }

    #[test]
    fn several_aliases_may_name_one_command() {
        let pack = parse("p", &with_alias("\"go\" = \"core.ping\"\n\"hi\" = \"core.ping\"")).unwrap();
        assert_eq!(pack.aliases.len(), 2);
    }

    #[test]
    fn too_many_aliases_or_too_big_a_file_is_rejected() {
        let many: String = (0..=MAX_ALIASES).map(|i| format!("\"a{i}\" = \"core.ping\"\n")).collect();
        has(&problems(&with_alias(&many)), "the most a pack may have is 64");
        let huge = format!("{GOOD}# {}\n", "x".repeat(MAX_PACK_TOML as usize));
        has(&problems(&huge), "larger than 64 KiB");
    }

    #[test]
    fn anim_entries_are_checked_without_the_filesystem() {
        let anim = |line: &str| format!("{GOOD}\n[anim]\n{line}\n");
        let pack = parse("p", &anim("\"workspaces.activate\" = \"anim/go.txt\"")).unwrap();
        assert_eq!(pack.anim["workspaces.activate"], "anim/go.txt");
        has(&problems(&anim("\"core.daemon\" = \"a.txt\"")), "[anim] 'core.daemon' isn't a command");
        has(&problems(&anim("\"core.ping\" = \"/etc/passwd\"")), "must be relative");
        has(&problems(&anim("\"core.ping\" = \"../x.txt\"")), "must stay inside");
        has(&problems(&anim("\"core.ping\" = \"anim/../../x\"")), "must stay inside");
        has(&problems(&anim("\"core.ping\" = \"\"")), "is empty");
        has(&problems(&anim("\"core.ping\" = 3")), "must be a path string");
    }

    // ---------------------------------------------------------------- check_folder

    fn folder(name: &str, toml: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(dir.join("anim")).unwrap();
        std::fs::write(dir.join("pack.toml"), toml).unwrap();
        (tmp, dir)
    }

    #[test]
    fn a_folder_pack_with_its_animation_passes() {
        let (_tmp, dir) = folder("my-pack", &format!("{GOOD}\n[anim]\n\"workspaces.activate\" = \"anim/go.txt\"\n"));
        std::fs::write(dir.join("anim/go.txt"), "frame 1\n").unwrap();
        assert_eq!(check_folder(&dir).unwrap().id, "my-pack");
    }

    #[test]
    fn the_id_must_match_the_folder() {
        let (_tmp, dir) = folder("other-name", GOOD);
        let p = check_folder(&dir).unwrap_err();
        has(&p, "other-name/pack.toml: id 'my-pack' must match the folder name 'other-name'");
    }

    #[test]
    fn a_missing_or_unreadable_pack_toml_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("empty");
        std::fs::create_dir(&dir).unwrap();
        has(&check_folder(&dir).unwrap_err(), "empty/pack.toml: doesn't exist");

        let (_tmp, dir) = folder("my-pack", GOOD);
        std::fs::remove_file(dir.join("pack.toml")).unwrap();
        std::fs::create_dir(dir.join("pack.toml")).unwrap();
        has(&check_folder(&dir).unwrap_err(), "is not a file");

        let (_tmp, dir) = folder("my-pack", GOOD);
        std::fs::write(dir.join("pack.toml"), [0xff, 0xfe, b'\n']).unwrap();
        has(&check_folder(&dir).unwrap_err(), "is not UTF-8 text");
    }

    #[test]
    fn animation_files_must_exist_be_small_text_and_stay_inside() {
        let anim = format!("{GOOD}\n[anim]\n\"core.ping\" = \"anim/a.txt\"\n\"core.shutdown\" = \"anim/b.txt\"\n\"records.add\" = \"anim/c.txt\"\n\"records.get\" = \"anim\"\n");
        let (_tmp, dir) = folder("my-pack", &anim);
        std::fs::write(dir.join("anim/b.txt"), vec![b'x'; MAX_ANIM as usize + 1]).unwrap();
        std::fs::write(dir.join("anim/c.txt"), [0xff, 0xfe]).unwrap();
        let p = check_folder(&dir).unwrap_err();
        has(&p, "[anim] core.ping: 'anim/a.txt' doesn't exist");
        has(&p, "[anim] core.shutdown: 'anim/b.txt' is larger than 256 KiB");
        has(&p, "[anim] records.add: 'anim/c.txt' is not UTF-8 text");
        has(&p, "[anim] records.get: 'anim' is not a file");
    }

    #[cfg(unix)]
    #[test]
    fn an_animation_symlink_leading_outside_the_folder_is_rejected() {
        let (tmp, dir) = folder("my-pack", &format!("{GOOD}\n[anim]\n\"core.ping\" = \"anim/sneaky.txt\"\n"));
        std::fs::write(tmp.path().join("secret.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("secret.txt"), dir.join("anim/sneaky.txt")).unwrap();
        has(&check_folder(&dir).unwrap_err(), "'anim/sneaky.txt' leads outside the pack's folder");
    }
}
