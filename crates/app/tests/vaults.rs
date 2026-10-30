//! ADR 0041: vaults, end to end with the real binary. Nothing here sets `SHIMMER_HOME` or
//! `SHIMMER_SOCKET` except where a test says so: the vault decides, as it does for a person.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use common::{stderr, stdout, LEETCODE};
use tempfile::TempDir;

/// A machine of its own: `HOME`, the data and config folders and the runtime folder are all temp.
struct Machine {
    dir: TempDir,
}

impl Machine {
    fn new() -> Self {
        let m = Self { dir: TempDir::new().unwrap() };
        m.fill(&m.main());
        m
    }

    /// The main vault: the platform's default folder under this machine's `HOME`.
    fn main(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.dir.path().join("home/Library/Application Support/shimmer")
        } else {
            self.dir.path().join("data/shimmer")
        }
    }

    fn vault(&self, name: &str) -> PathBuf {
        self.main().parent().unwrap().join("shimmer-vaults").join(name)
    }

    /// A LeetCode collection, and no desktop popups (ADR 0032 §2).
    fn fill(&self, home: &Path) {
        std::fs::create_dir_all(home.join("data/records/collections")).unwrap();
        std::fs::write(home.join("config.toml"), "[desktop]\nshow = false\n").unwrap();
        std::fs::write(home.join("data/records/collections/leetcode.toml"), LEETCODE).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_shimmer"));
        cmd.args(args)
            .env("HOME", self.dir.path().join("home"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_RUNTIME_DIR", self.dir.path().join("rt"))
            .env_remove("SHIMMER_HOME")
            .env_remove("SHIMMER_SOCKET");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(o.status.success(), "shimmer {args:?} failed: {}{}", stdout(&o), stderr(&o));
        stdout(&o)
    }

    /// Record ids in `home`'s LeetCode collection, read from the files.
    fn records(&self, home: &Path) -> Vec<String> {
        let dir = home.join("data/records/items/leetcode");
        let mut ids: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        ids
    }

    fn socket(&self, home: &Path) -> PathBuf {
        let rt = cfg!(target_os = "linux").then(|| self.dir.path().join("rt"));
        shimmer_proto::paths::default_socket_in(home, rt.as_deref(), shimmer_proto::paths::owner_uid())
    }

    fn stop(&self, home: &Path) {
        let _ = self.command(&["shutdown"]).env("SHIMMER_HOME", home).output();
        let socket = self.socket(home);
        let deadline = Instant::now() + Duration::from_secs(10);
        while socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Never leave a daemon running behind a failed assertion.
impl Drop for Machine {
    fn drop(&mut self) {
        let mut homes = vec![self.main()];
        if let Ok(dir) = std::fs::read_dir(self.main().parent().unwrap().join("shimmer-vaults")) {
            homes.extend(dir.flatten().map(|e| e.path()));
        }
        for home in homes {
            if self.socket(&home).exists() {
                self.stop(&home);
            }
        }
    }
}

#[test]
fn a_vault_keeps_its_own_records_and_the_order_of_choice_holds() {
    let m = Machine::new();
    let play = m.vault("play");
    assert!(m.ok(&["vaults", "new", "play"]).contains("✓ vault play"));
    assert!(play.is_dir(), "beside main: {play:?}");
    m.fill(&play);

    // --vault: one command, in play only.
    m.ok(&["--vault", "play", "records", "add", "leetcode", "--title", "In Play"]);
    assert_eq!(m.records(&play).len(), 1);
    assert!(m.records(&m.main()).is_empty(), "nothing in main");

    // A plain command is main's.
    m.ok(&["records", "add", "leetcode", "--title", "In Main"]);
    assert_eq!((m.records(&m.main()).len(), m.records(&play).len()), (1, 1));

    // `use`: plain commands go to play, and both daemons run at once.
    m.ok(&["vaults", "use", "play"]);
    m.ok(&["records", "add", "leetcode", "--title", "Also In Play"]);
    assert_eq!((m.records(&m.main()).len(), m.records(&play).len()), (1, 2));
    assert!(m.socket(&play).exists() && m.socket(&m.main()).exists());
    let status = m.ok(&["vaults", "status"]);
    assert!(status.contains("in use: play") && status.contains("vaults use"), "{status}");
    assert!(status.contains("daemons running: main, play"), "{status}");

    // SHIMMER_HOME comes before `use`; --vault before SHIMMER_HOME.
    let main = m.main();
    let o = m.command(&["records", "add", "leetcode", "--title", "Env"]).env("SHIMMER_HOME", &main).output().unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(m.records(&main).len(), 2, "SHIMMER_HOME beat use");
    let o = m
        .command(&["--vault", "play", "records", "add", "leetcode", "--title", "Flag"])
        .env("SHIMMER_HOME", &main)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!((m.records(&main).len(), m.records(&play).len()), (2, 3), "--vault beat SHIMMER_HOME");

    // `vaults list --json` marks the one in use.
    let list: serde_json::Value = serde_json::from_str(&m.ok(&["vaults", "list", "--json"])).unwrap();
    assert_eq!(list["in_use"], "play");
    assert_eq!(list["vaults"].as_array().unwrap().len(), 2);

    // An unknown vault is an error, never a silent fallback to main.
    let o = m.run(&["--vault", "nope", "ping"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("no vault named 'nope'"), "{}", stderr(&o));
}

#[test]
fn a_vault_is_forgotten_kept_brought_back_and_purged() {
    let m = Machine::new();
    let play = m.vault("play");
    m.ok(&["vaults", "new", "play"]);
    m.fill(&play);
    m.ok(&["--vault", "play", "records", "add", "leetcode", "--title", "X"]);
    assert!(m.socket(&play).exists(), "its daemon runs");

    m.ok(&["vaults", "remove", "play"]);
    assert!(play.join("data").is_dir(), "files kept");
    assert!(!m.socket(&play).exists(), "its daemon was stopped");
    assert!(m.ok(&["vaults", "list", "--removed"]).contains("play"));
    assert!(!m.run(&["--vault", "play", "ping"]).status.success());

    m.ok(&["vaults", "restore", "play"]);
    m.ok(&["vaults", "remove", "play"]);
    // Asks first: with nobody to answer, --yes is needed.
    let o = m.run(&["vaults", "purge"]);
    assert!(!o.status.success() && stderr(&o).contains("--yes"), "{}", stderr(&o));
    assert!(m.ok(&["vaults", "purge", "--yes"]).contains("deleted: play"));
    assert!(!play.exists());
    assert!(m.records(&m.main()).is_empty(), "main untouched");
}

#[test]
fn completion_offers_vault_names() {
    let m = Machine::new();
    m.ok(&["vaults", "new", "play"]);
    let out = m.ok(&["__complete", "shimmer", "--vault", ""]);
    assert!(out.contains("main") && out.contains("play"), "{out}");
}

const RULE: &str = r#"
[rule]
label = "Tell me"
on = "records.item.completed"

[[do]]
op = "records.list"
params = { collection = "leetcode" }
"#;

const SITE: &str = "[workspace]\nlabel = \"Site\"\n\n[[step]]\nname = \"setup\"\nmode = \"supervised\"\ntimeout_s = 30\n\n[cleanup]\ntimeout_s = 30\n";

/// `--json` output of `args` in `vault`.
fn json_in(m: &Machine, vault: &str, args: &[&str]) -> serde_json::Value {
    let mut all = vec!["--vault", vault];
    all.extend_from_slice(args);
    all.push("--json");
    serde_json::from_str(&m.ok(&all)).unwrap()
}

/// `(id, enabled)` of every schedule a person or a rule made, and of notify's own flush.
fn schedules(m: &Machine, vault: &str) -> (Vec<bool>, Option<bool>) {
    let list = json_in(m, vault, &["scheduler", "list"]);
    let triggers = list["triggers"].as_array().unwrap();
    let mine = triggers.iter().filter(|t| t["source"]["type"] != "module").map(|t| t["enabled"] == true).collect();
    let flush = triggers.iter().find(|t| t["id"] == "notify.flush").map(|t| t["enabled"] == true);
    (mine, flush)
}

/// ADR 0041 §5: a copy has the data, leaves out what's derived or tied to the machine, and starts
/// with everything that acts by itself switched off, so nothing runs twice.
#[test]
fn a_copy_has_the_data_and_starts_switched_off() {
    let m = Machine::new();
    let main = m.main();
    m.ok(&["records", "add", "leetcode", "--title", "Two Sum"]);
    let schedule =
        r#"{"schedule":{"every":3600},"op":"records.list","params":{"collection":"leetcode"},"catch_up":"skip"}"#;
    m.ok(&["call", "scheduler.add", schedule]);
    let rule = m.dir.path().join("rule.toml");
    std::fs::write(&rule, RULE).unwrap();
    m.ok(&["automations", "new", "told", "--file", rule.to_str().unwrap()]);
    // An active workspace whose cleanup leaves a mark: in the copy it must never run.
    let mark = m.dir.path().join("cleanup-ran");
    let site = main.join("data/workspaces/site");
    std::fs::create_dir_all(site.join("steps")).unwrap();
    std::fs::write(site.join("workspace.toml"), SITE).unwrap();
    std::fs::write(site.join("steps/01-setup.sh"), "true\n").unwrap();
    std::fs::write(site.join("cleanup.sh"), format!("touch '{}'\n", mark.display())).unwrap();
    std::fs::write(site.join("state.toml"), "state = \"active\"\n").unwrap();
    std::fs::create_dir_all(main.join("logs")).unwrap();
    std::fs::write(main.join("logs/marker.txt"), "main's own log").unwrap();
    // What an interrupted copy to this name leaves: removed by the next one.
    let leftover = m.vault("play").with_file_name("play.copying");
    std::fs::create_dir_all(leftover.join("data")).unwrap();
    std::fs::write(leftover.join(".gitignore"), "").unwrap();

    let out = m.ok(&["vaults", "copy", "main", "play"]);
    assert!(out.contains("✓ copied main to play"), "{out}");
    assert!(out.contains("1 schedule, 1 automation") && out.contains("notifications paused"), "{out}");
    assert!(out.contains("1 workspace reset to ready"), "{out}");
    assert!(!leftover.exists(), "the leftover .copying folder is gone");
    let play = m.vault("play");
    assert_eq!(m.records(&play).len(), 1, "the data came");
    assert!(!play.join("logs/marker.txt").exists(), "logs stay with the machine");

    // Off in the copy, as they were in main; Shimmer's own plumbing left alone.
    assert_eq!(schedules(&m, "play"), (vec![false], Some(true)));
    assert_eq!(schedules(&m, "main"), (vec![true], Some(true)));
    let rules = json_in(&m, "play", &["automations", "list"]);
    let rules = rules["rules"].as_array().unwrap();
    assert!(rules.iter().any(|r| r["id"] == "told" && r["enabled"] == false), "{rules:?}");
    assert!(rules.iter().filter(|r| r["builtin"] == true).all(|r| r["enabled"] == true), "built-in alerts left on");
    assert!(json_in(&m, "play", &["notify", "status"])["paused"].is_object(), "paused in the copy");
    assert!(json_in(&m, "main", &["notify", "status"])["paused"].is_null(), "not in main");
    assert_eq!(json_in(&m, "play", &["workspaces", "show", "site"])["state"], "ready");
    assert_eq!(json_in(&m, "main", &["workspaces", "show", "site"])["state"], "active");
    assert!(!mark.exists(), "the copy never ran the original's cleanup");

    // Two vaults from here on: a change in one isn't in the other.
    m.ok(&["--vault", "play", "records", "add", "leetcode", "--title", "Only In Play"]);
    assert_eq!((m.records(&main).len(), m.records(&play).len()), (1, 2));

    // --live leaves things as they were.
    let out = m.ok(&["vaults", "copy", "main", "live", "--live"]);
    assert!(out.contains("left on (--live)"), "{out}");
    assert_eq!(schedules(&m, "live"), (vec![true], Some(true)));
}

#[test]
fn copy_refuses_what_it_shouldnt_touch() {
    let m = Machine::new();
    m.ok(&["vaults", "new", "empty"]);
    let o = m.run(&["vaults", "copy", "empty", "x"]);
    assert!(stderr(&o).contains("nothing to copy yet"), "{}", stderr(&o));
    m.ok(&["ping"]);
    let o = m.run(&["vaults", "copy", "main", "empty"]);
    assert!(stderr(&o).contains("already a vault named 'empty'"), "{}", stderr(&o));
    let busy = m.dir.path().join("busy");
    std::fs::create_dir_all(&busy).unwrap();
    std::fs::write(busy.join("notes.txt"), "mine").unwrap();
    let o = m.run(&["vaults", "copy", "main", "x", "--path", busy.to_str().unwrap()]);
    assert!(stderr(&o).contains("isn't empty"), "{}", stderr(&o));
    assert_eq!(std::fs::read_to_string(busy.join("notes.txt")).unwrap(), "mine");
}
