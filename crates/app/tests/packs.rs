//! Command packs end to end (ADR 0013, issue #35), through the real binary: choosing a pack,
//! aliases after `shimmer`, aliases on their own through the links `shimmer packs use` makes,
//! and everything that must never break a command or touch someone else's files. Each test has
//! its own `$SHIMMER_HOME`, config folder, `HOME` and `PATH`, so nothing outside it is touched.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{stderr, stdout, Home};

/// The folder `shimmer packs use` links into: `$HOME/.local/bin`, inside the test's temp folder.
fn bin(home: &Home) -> PathBuf {
    home.dir.path().join("fakehome/.local/bin")
}

/// Run `program args…` with the test's environment. `PATH` is the test's own link folder and
/// nothing else: never the real `PATH`, nor the system's folders. A developer who has used a
/// pack has its aliases linked in `~/.local/bin`, and the linker rightly refuses to shadow a
/// command that's already on `PATH`, so the counts below would depend on the machine (#97).
fn run(home: &Home, program: &Path, args: &[&str]) -> Output {
    let path = bin(home);
    Command::new(program)
        .args(args)
        .env("SHIMMER_HOME", home.dir.path().join("home"))
        .env("SHIMMER_SOCKET", home.socket())
        .env("XDG_CONFIG_HOME", home.dir.path().join("config"))
        .env("HOME", home.dir.path().join("fakehome"))
        .env("PATH", path)
        .output()
        .unwrap()
}

fn shimmer(home: &Home, args: &[&str]) -> Output {
    run(home, Path::new(env!("CARGO_BIN_EXE_shimmer")), args)
}

/// Run an alias on its own, through its link: `ikuzo deep-work`.
fn bare(home: &Home, alias: &str, args: &[&str]) -> Output {
    run(home, &bin(home).join(alias), args)
}

fn ok(o: Output) -> String {
    assert!(o.status.success(), "exit {:?}\nstdout: {}\nstderr: {}", o.status.code(), stdout(&o), stderr(&o));
    stdout(&o)
}

fn links(home: &Home) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(bin(home))
        .map(|d| d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

fn folder_pack(home: &Home, id: &str, toml: &str) {
    let dir = home.dir.path().join("home/packs").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pack.toml"), toml).unwrap();
}

#[test]
fn no_pack_by_default_and_packs_need_no_daemon() {
    let home = Home::new();
    let list = ok(shimmer(&home, &["packs", "list"]));
    for id in ["anime-tropes", "ship-it", "starship", "short"] {
        assert!(list.contains(id), "{list}");
    }
    assert!(list.contains("No pack is active"), "{list}");
    assert!(!home.socket().exists(), "packs commands never start the daemon");

    let o = shimmer(&home, &["ikuzo"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("unknown command 'ikuzo'"), "{}", stderr(&o));
}

#[cfg(unix)]
#[test]
fn use_links_aliases_that_run_shimmer_and_the_whole_command() {
    let home = Home::with_leetcode();
    let out = ok(shimmer(&home, &["packs", "use", "short"]));
    assert!(out.starts_with("✓ active pack: short (Short)\n  linked 53 commands into"), "{out}");
    assert_eq!(links(&home).len(), 53);

    // On their own, through the links: ping starts the daemon; a record goes in and is completed.
    assert!(ok(bare(&home, "up", &[])).starts_with("pong"));
    assert_eq!(
        ok(bare(&home, "radd", &["leetcode", "two-sum", "--title", "Two Sum"])).trim(),
        "added leetcode/two-sum"
    );
    assert!(ok(bare(&home, "rdone", &["leetcode/two-sum"])).starts_with("✓ leetcode/two-sum done"));
    // After shimmer too, and the real names still work alongside.
    let rec = ok(shimmer(&home, &["rget", "leetcode/two-sum", "--json"]));
    assert!(rec.contains("\"status\": \"done\""), "{rec}");
    assert!(ok(shimmer(&home, &["records", "list", "leetcode"])).contains("two-sum"));

    // Errors name the real command, never the alias.
    let o = bare(&home, "wgo", &[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).starts_with("shimmer: usage: shimmer workspaces activate NAME"), "{}", stderr(&o));
}

#[cfg(unix)]
#[test]
fn switching_swaps_the_links_and_none_removes_them() {
    let home = Home::new();
    ok(shimmer(&home, &["packs", "use", "anime-tropes"]));
    assert!(links(&home).contains(&"ikuzo".to_string()));

    let out = ok(shimmer(&home, &["packs", "use", "ship-it"]));
    assert!(out.contains("linked 53") && out.contains("removed 53"), "{out}");
    assert!(links(&home).contains(&"deploy".to_string()) && !links(&home).contains(&"ikuzo".to_string()));

    ok(shimmer(&home, &["packs", "use", "starship", "--no-link"]));
    assert_eq!(links(&home), Vec::<String>::new());
    assert!(ok(shimmer(&home, &["comms"])).starts_with("pong"), "aliases still work after shimmer");

    ok(shimmer(&home, &["packs", "use", "none"]));
    assert_eq!(shimmer(&home, &["comms"]).status.code(), Some(2), "packs off: comms is unknown again");
    assert!(ok(shimmer(&home, &["ping"])).starts_with("pong"));
}

#[cfg(unix)]
#[test]
fn nothing_of_the_users_is_ever_overwritten_or_removed() {
    let home = Home::new();
    std::fs::create_dir_all(bin(&home)).unwrap();
    std::fs::write(bin(&home).join("deploy"), "my own deploy script").unwrap();

    let out = ok(shimmer(&home, &["packs", "use", "ship-it"]));
    assert!(out.contains("linked 52") && out.contains("skipped deploy: it already exists"), "{out}");
    ok(shimmer(&home, &["packs", "use", "none"]));
    assert_eq!(links(&home), ["deploy"]);
    assert_eq!(std::fs::read_to_string(bin(&home).join("deploy")).unwrap(), "my own deploy script");
}

#[cfg(unix)]
#[test]
fn a_link_from_another_pack_explains_itself_and_never_starts_the_daemon() {
    let home = Home::new();
    ok(shimmer(&home, &["packs", "use", "starship"]));
    // A leftover link from a pack that's no longer on (made by hand here).
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_shimmer"), bin(&home).join("wgo")).unwrap();
    let o = bare(&home, "wgo", &["deep-work"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(stderr(&o).trim(), "wgo: isn't an alias in your active pack (starship). Run 'shimmer packs link'.");

    // An alias's arguments are its own, even "daemon": engage daemon activates a workspace called
    // daemon, it doesn't start the daemon in the foreground.
    let o = bare(&home, "engage", &["daemon", "--wait"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("no workspace 'daemon'"), "{}", stderr(&o));
}

#[test]
fn pack_problems_are_one_warning_and_commands_still_run() {
    let home = Home::new();
    folder_pack(&home, "broken", "[pack]\nid = \"broken\"\nlabel = \"B\"\n[alias]\n\"ping\" = \"core.ping\"\n");

    let o = shimmer(&home, &["--pack", "broken", "ping", "--json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("\"pong\": true"), "stdout stays clean JSON: {}", stdout(&o));
    assert_eq!(
        stderr(&o).trim(),
        "shimmer: pack 'broken' not loaded (1 problem; see 'shimmer packs check broken'); using no aliases"
    );
    let o = shimmer(&home, &["--pack", "nope", "ping"]);
    assert!(o.status.success() && stderr(&o).contains("no pack named 'nope'"), "{}", stderr(&o));

    // packs use refuses it, with every problem, and switches nothing.
    let o = shimmer(&home, &["packs", "use", "broken"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("alias 'ping' is a shimmer command word"), "{}", stderr(&o));
    assert!(ok(shimmer(&home, &["packs", "list"])).contains("No pack is active"));

    // Made active by hand, it warns on every command except `shimmer packs …`, which says what's
    // wrong itself rather than pointing at itself.
    let config = home.dir.path().join("config/shimmer");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("cli.toml"), "pack = \"broken\"\n").unwrap();
    assert!(stderr(&shimmer(&home, &["ping"])).contains("pack 'broken' not loaded"));
    for args in [&["packs", "list"][..], &["--json", "packs", "show", "short"]] {
        let o = shimmer(&home, args);
        assert!(o.status.success() && stderr(&o).is_empty(), "{args:?}: {}", stderr(&o));
    }
    let o = shimmer(&home, &["packs", "check", "broken"]);
    assert_eq!(stderr(&o).lines().count(), 2, "only the check's own answer: {}", stderr(&o));
}

#[test]
fn a_folder_pack_works_and_help_shows_its_aliases() {
    let home = Home::new();
    folder_pack(&home, "mine", "[pack]\nid = \"mine\"\nlabel = \"Mine\"\n[alias]\n\"hello\" = \"core.ping\"\n");
    assert_eq!(ok(shimmer(&home, &["packs", "check", "mine"])).trim(), "✓ mine: 1 alias, no problems");
    ok(shimmer(&home, &["packs", "use", "mine", "--no-link"]));
    assert!(ok(shimmer(&home, &["hello"])).starts_with("pong"));

    let help = ok(shimmer(&home, &["--help"]));
    assert!(help.contains("aliases (mine pack;") && help.contains("hello  shimmer ping"), "{help}");
    let canonical = ok(shimmer(&home, &["--help", "--canonical"]));
    assert!(!canonical.contains("aliases ("), "{canonical}");
}

#[test]
fn the_example_pack_in_the_docs_passes_its_checks() {
    let home = Home::new();
    let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/packs/my-first-pack");
    let out = ok(shimmer(&home, &["packs", "check", example]));
    assert_eq!(out.trim(), "✓ my-first-pack: 7 aliases, no problems");
}
