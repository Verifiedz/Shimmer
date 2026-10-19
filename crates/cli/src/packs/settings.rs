//! The CLI's own settings file, `cli.toml` (ADR 0013 §3): which pack is active, where bare
//! commands are linked, which links Shimmer made, and the login service `shimmer startup on`
//! wrote (ADR 0034 §6). It lives outside `$SHIMMER_HOME`, which
//! belongs to the daemon (CLAUDE.md §1.6, §7), and only the CLI reads or writes it.
//!
//! A broken `cli.toml` must never break a command, so problems here are reported and the caller
//! carries on with no pack (ADR 0013 §8). An unknown key is only a warning, so a file written by
//! a newer Shimmer still loads in an older one.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use super::pack::{name_problem, NO_PACK};

/// `cli.toml` larger than this is rejected unread.
pub const MAX_CLI_TOML: usize = 16 * 1024;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Settings {
    /// The active pack's id. `None`: no pack.
    pub pack: Option<String>,
    /// Where bare commands are linked. `None`: the default, `~/.local/bin`.
    pub link_dir: Option<PathBuf>,
    /// The links Shimmer made: the only names it will ever remove from `link_dir`.
    pub linked: Vec<String>,
    /// The login service `shimmer startup on` wrote: the only one `off` will remove.
    pub startup: Option<Startup>,
    /// Told once that `shimmer startup on` exists, when a schedule was made (ADR 0034 §7).
    pub told_startup: bool,
}

/// `startup = { file = "…", target = "…" }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Startup {
    /// The service file it wrote: a systemd unit or a launchd plist.
    pub file: PathBuf,
    /// When it starts: `graphical-session.target` or `default.target` (systemd), `login` (launchd).
    pub target: String,
}

/// Settings read from `cli.toml`, plus warnings to show for keys this version doesn't know.
#[derive(Debug, Default, PartialEq)]
pub struct Loaded {
    pub settings: Settings,
    pub warnings: Vec<String>,
}

/// `$XDG_CONFIG_HOME/shimmer/cli.toml`, else `~/.config/shimmer/cli.toml`, on Linux and macOS.
/// `None` when neither variable is set.
pub fn path() -> Option<PathBuf> {
    path_from(std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME"))
}

fn path_from(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match xdg_config_home.filter(|p| !p.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(home.filter(|p| !p.is_empty())?).join(".config"),
    };
    Some(base.join("shimmer/cli.toml"))
}

/// Where bare commands go when `cli.toml` doesn't say: `~/.local/bin`.
pub fn default_link_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".local/bin"))
}

/// The settings in `path`: the defaults if there is no file yet, or every problem with it.
pub fn load(path: &Path) -> Result<Settings, Vec<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&path.display().to_string(), &text).map(|l| l.settings),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(vec![format!("{}: can't be read: {e}", path.display())]),
    }
}

/// Save `settings` to `path`, all or nothing (CLAUDE.md §7.1): a temp file in the same folder,
/// `fsync`, then `rename`, so a crash leaves the old file or the new one. Keys this version
/// doesn't know are kept, so saving never loses a newer Shimmer's settings.
pub fn save(path: &Path, settings: &Settings) -> std::io::Result<()> {
    let mut table: Table =
        std::fs::read_to_string(path).ok().and_then(|text| toml::from_str(&text).ok()).unwrap_or_default();
    let _ = match &settings.pack {
        Some(id) => table.insert("pack".into(), Value::String(id.clone())),
        None => table.remove("pack"),
    };
    let _ = match &settings.link_dir {
        Some(dir) => table.insert("link_dir".into(), Value::String(dir.display().to_string())),
        None => table.remove("link_dir"),
    };
    let _ = if settings.linked.is_empty() {
        table.remove("linked")
    } else {
        table.insert("linked".into(), Value::Array(settings.linked.iter().cloned().map(Value::String).collect()))
    };
    let _ = match &settings.startup {
        Some(s) => {
            let mut t = Table::new();
            t.insert("file".into(), Value::String(s.file.display().to_string()));
            t.insert("target".into(), Value::String(s.target.clone()));
            table.insert("startup".into(), Value::Table(t))
        }
        None => table.remove("startup"),
    };
    let _ = if settings.told_startup {
        table.insert("told_startup".into(), Value::Boolean(true))
    } else {
        table.remove("told_startup")
    };
    let text = toml::to_string(&table).map_err(std::io::Error::other)?;

    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".cli.toml.{}.tmp", std::process::id()));
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(text.as_bytes())?;
        f.sync_all()
    });
    match written.and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Check `cli.toml`'s text. `file` names it in every problem and warning.
pub fn parse(file: &str, text: &str) -> Result<Loaded, Vec<String>> {
    if text.len() > MAX_CLI_TOML {
        return Err(vec![format!("{file}: larger than {} KiB", MAX_CLI_TOML / 1024)]);
    }
    let table: Table = toml::from_str(text).map_err(|e| vec![format!("{file}: not valid TOML: {}", e.message())])?;

    let mut loaded = Loaded::default();
    let mut problems = Vec::new();
    for (key, value) in &table {
        match (key.as_str(), value) {
            ("pack", Value::String(id)) if id == NO_PACK => {}
            ("pack", Value::String(id)) => match name_problem(id) {
                Some(why) => problems.push(format!("{file}: pack '{id}' {why}")),
                None => loaded.settings.pack = Some(id.clone()),
            },
            ("link_dir", Value::String(dir)) => {
                let dir = PathBuf::from(dir);
                if dir.is_absolute() {
                    loaded.settings.link_dir = Some(dir);
                } else {
                    problems.push(format!("{file}: link_dir '{}' must be an absolute path", dir.display()));
                }
            }
            ("linked", Value::Array(names)) => {
                for name in names {
                    // These names are deleted from link_dir later, so each must be one plain
                    // file name: the alias charset rules out '/', '..' and anything odd.
                    match name.as_str() {
                        Some(n) if name_problem(n).is_none() => loaded.settings.linked.push(n.to_string()),
                        _ => problems.push(format!("{file}: linked entry {name} isn't a valid alias name")),
                    }
                }
            }
            ("startup", Value::Table(t)) => {
                match (t.get("file").and_then(Value::as_str), t.get("target").and_then(Value::as_str)) {
                    // `off` removes this file, so it must be a full path (and `off` checks it's its own).
                    (Some(f), Some(target)) if Path::new(f).is_absolute() => {
                        loaded.settings.startup = Some(Startup { file: PathBuf::from(f), target: target.to_owned() })
                    }
                    _ => problems.push(format!("{file}: startup must be {{ file = \"<full path>\", target = \"…\" }}")),
                }
            }
            ("told_startup", Value::Boolean(b)) => loaded.settings.told_startup = *b,
            ("startup", _) => {
                problems.push(format!("{file}: startup must be {{ file = \"<full path>\", target = \"…\" }}"))
            }
            ("told_startup", _) => problems.push(format!("{file}: told_startup must be true or false")),
            ("pack" | "link_dir", _) => problems.push(format!("{file}: {key} must be a string")),
            ("linked", _) => problems.push(format!("{file}: linked must be a list of names")),
            _ => loaded.warnings.push(format!("{file}: unknown key '{key}' ignored")),
        }
    }
    if problems.is_empty() {
        Ok(loaded)
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Loaded {
        parse("cli.toml", text).unwrap()
    }

    fn problems(text: &str) -> Vec<String> {
        parse("cli.toml", text).unwrap_err()
    }

    #[test]
    fn a_full_file_parses() {
        let loaded = ok("pack = \"anime-tropes\"\nlink_dir = \"/home/me/bin\"\nlinked = [\"ikuzo\", \"nani\"]\n");
        assert_eq!(
            loaded.settings,
            Settings {
                pack: Some("anime-tropes".into()),
                link_dir: Some("/home/me/bin".into()),
                linked: vec!["ikuzo".into(), "nani".into()],
                ..Settings::default()
            }
        );
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn an_empty_file_or_none_means_no_pack() {
        assert_eq!(ok("").settings, Settings::default());
        assert_eq!(ok("pack = \"none\"").settings.pack, None);
    }

    #[test]
    fn unknown_keys_warn_but_still_load() {
        let loaded = ok("pack = \"short\"\ntheme = \"dark\"\n");
        assert_eq!(loaded.settings.pack.as_deref(), Some("short"));
        assert_eq!(loaded.warnings, ["cli.toml: unknown key 'theme' ignored"]);
    }

    #[test]
    fn wrong_values_are_problems() {
        let mut p = problems("pack = 3\nlink_dir = \"bin\"\nlinked = \"ikuzo\"\n");
        p.sort();
        assert_eq!(
            p,
            [
                "cli.toml: link_dir 'bin' must be an absolute path",
                "cli.toml: linked must be a list of names",
                "cli.toml: pack must be a string",
            ]
        );
        assert!(problems("pack = \"Not Valid\"")[0].contains("pack 'Not Valid' must start"));
        assert!(problems("[pack\n")[0].starts_with("cli.toml: not valid TOML"));
        assert!(problems(&"#".repeat(MAX_CLI_TOML + 1))[0].contains("larger than 16 KiB"));
    }

    #[test]
    fn linked_names_can_never_reach_outside_the_link_folder() {
        for bad in ["\"../evil\"", "\"/etc/passwd\"", "\"a/b\"", "\"\"", "3"] {
            let p = problems(&format!("linked = [{bad}]"));
            assert!(p[0].contains("isn't a valid alias name"), "{bad}: {p:?}");
        }
    }

    #[test]
    fn save_then_load_round_trips_and_keeps_unknown_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shimmer/cli.toml");
        assert_eq!(load(&path).unwrap(), Settings::default(), "no file yet");

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "theme = \"dark\"\npack = \"old\"\n").unwrap();
        let s = Settings {
            pack: Some("short".into()),
            link_dir: Some("/opt/bin".into()),
            linked: vec!["up".into(), "down".into()],
            startup: Some(Startup {
                file: "/home/me/.config/systemd/user/shimmer.service".into(),
                target: "default.target".into(),
            }),
            told_startup: true,
        };
        save(&path, &s).unwrap();
        assert_eq!(load(&path).unwrap(), s);
        assert!(std::fs::read_to_string(&path).unwrap().contains("theme = \"dark\""), "unknown key kept");

        save(&path, &Settings::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "theme = \"dark\"", "cleared keys are removed");
        let leftovers: Vec<_> =
            std::fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, ["cli.toml"], "no temp file left behind");
    }

    #[test]
    fn the_startup_service_is_recorded_by_full_path() {
        let loaded = ok("told_startup = true\n[startup]\nfile = \"/home/me/x.service\"\ntarget = \"default.target\"\n");
        assert_eq!(
            loaded.settings.startup,
            Some(Startup { file: "/home/me/x.service".into(), target: "default.target".into() })
        );
        assert!(loaded.settings.told_startup);
        for bad in ["startup = 3", "[startup]\nfile = \"x.service\"\ntarget = \"t\"", "[startup]\nfile = \"/x\""] {
            assert!(problems(bad)[0].contains("startup must be"), "{bad}");
        }
        assert!(problems("told_startup = \"yes\"")[0].contains("true or false"));
    }

    #[test]
    fn a_broken_file_is_reported_by_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cli.toml");
        std::fs::write(&path, "pack = 3").unwrap();
        assert!(load(&path).unwrap_err()[0].ends_with("pack must be a string"));
    }

    #[test]
    fn the_file_lives_in_the_config_folder() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(path_from(os("/x"), os("/home/me")), Some("/x/shimmer/cli.toml".into()));
        assert_eq!(path_from(None, os("/home/me")), Some("/home/me/.config/shimmer/cli.toml".into()));
        assert_eq!(path_from(os(""), os("/home/me")), Some("/home/me/.config/shimmer/cli.toml".into()));
        assert_eq!(path_from(None, None), None);
    }
}
