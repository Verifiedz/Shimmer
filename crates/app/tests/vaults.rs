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
    // Main synced with git: a copy must never carry its history, or its remote.
    std::fs::create_dir_all(main.join(".git")).unwrap();
    std::fs::write(main.join(".git/config"), "[remote \"origin\"]\n\turl = git@example.test:me/backup.git\n").unwrap();
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
    assert!(!play.join(".git").exists(), "no git history, so no push to main's backup");

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

const LATER: &str = r#"
[rule]
label = "Revisit it later"
on = "records.item.completed"
when = { collection = "leetcode" }

[[do]]
op = "records.update"
params = { collection = "leetcode", id = "{id}", fields = { url = "https://x.test/{id}" } }
after = "60d"
"#;

/// How many events in `home`'s log have this topic, and (for queue tasks) aren't notify's own.
fn logged(home: &Path, topic: &str) -> usize {
    let mut n = 0;
    for entry in std::fs::read_dir(home.join("events")).into_iter().flatten().flatten() {
        for line in std::fs::read_to_string(entry.path()).unwrap_or_default().lines() {
            let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let notify_own = ev["payload"]["op"].as_str().is_some_and(|op| op.starts_with("notify."));
            if ev["topic"] == topic && !notify_own {
                n += 1;
            }
        }
    }
    n
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// ADR 0041's test list, the rest of it: what was in flight when the copy was made. A save waiting
/// in `.staging/` is finished in the copy and a damaged one stays aside; a fetcher source and an
/// automation's delayed action are off; a half-launched workspace is ready; and the copy's next
/// ordinary start fires no rule over the copied log and runs no task.
#[cfg(unix)]
#[test]
fn a_copy_settles_what_was_in_flight_and_its_next_start_runs_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let m = Machine::new();
    let main = m.main();
    m.ok(&["records", "add", "leetcode/two-sum", "--title", "Two Sum"]);
    // A delayed action waiting: completing the record has automations add a one-time schedule.
    let rule = m.dir.path().join("later.toml");
    std::fs::write(&rule, LATER).unwrap();
    m.ok(&["automations", "new", "later", "--file", rule.to_str().unwrap()]);
    m.ok(&["records", "complete", "leetcode/two-sum"]);
    let added = || {
        let list = json_in(&m, "main", &["scheduler", "list"]);
        list["triggers"].as_array().unwrap().iter().filter(|t| t["source"]["type"] == "added").count()
    };
    until("the delayed action is scheduled", || added() == 1);
    // A fetcher source, on (its address answers nothing: no network here).
    m.ok(&["fetchers", "new", "blog", "--from", "rss", "--set", "URL=http://127.0.0.1:9/feed", "--yes"]);
    m.ok(&["fetchers", "enable", "blog"]); // a template's source starts off
                                           // A workspace a launch was cut off in.
    let half = main.join("data/workspaces/half");
    std::fs::create_dir_all(half.join("steps")).unwrap();
    std::fs::write(half.join("workspace.toml"), SITE.replace("Site", "Half")).unwrap();
    std::fs::write(half.join("steps/01-setup.sh"), "true\n").unwrap();
    std::fs::write(half.join("cleanup.sh"), "true\n").unwrap();
    std::fs::write(half.join("state.toml"), "state = \"launching\"\n").unwrap();
    // A save waiting (its folder can't be written), and a damaged one an earlier start kept aside.
    let items = main.join("data/records/items/leetcode");
    std::fs::set_permissions(&items, std::fs::Permissions::from_mode(0o555)).unwrap();
    let root = std::fs::File::create(items.join("probe")).is_ok(); // root ignores permissions
    if !root {
        m.ok(&["records", "update", "leetcode/two-sum", "--title", "Two Sum, waiting"]);
    }
    let broken = main.join(".staging/broken/01jabcdefghjkmnpqrstvwxyz0");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("manifest.json"), "{ half written").unwrap();

    let out = m.ok(&["vaults", "copy", "main", "play"]);
    std::fs::set_permissions(&items, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(out.contains("1 fetcher source"), "{out}");
    assert!(out.contains("1 workspace reset to ready"), "{out}");
    let play = m.vault("play");

    if !root {
        let file = std::fs::read_to_string(play.join("data/records/items/leetcode/two-sum.toml")).unwrap();
        assert!(file.contains("Two Sum, waiting"), "the waiting save was finished in the copy: {file}");
    }
    assert!(play.join(".staging/broken/01jabcdefghjkmnpqrstvwxyz0/manifest.json").exists(), "kept aside");
    let sources = json_in(&m, "play", &["fetchers", "list"]);
    assert_eq!(sources["sources"][0]["enabled"], false, "{sources}");
    assert_eq!(json_in(&m, "main", &["fetchers", "list"])["sources"][0]["enabled"], true);
    let list = json_in(&m, "play", &["scheduler", "list"]);
    let delayed: Vec<_> =
        list["triggers"].as_array().unwrap().iter().filter(|t| t["source"]["type"] == "added").collect();
    assert!(delayed.len() == 1 && delayed[0]["enabled"] == false, "the delayed action is off: {delayed:?}");
    assert_eq!(json_in(&m, "play", &["workspaces", "show", "half"])["state"], "ready");

    // The copy's next start is ordinary, and does nothing by itself.
    let (fired, ran) = (logged(&play, "automations.rule.fired"), logged(&play, "queue.task.started"));
    m.ok(&["--vault", "play", "shutdown"]);
    m.ok(&["--vault", "play", "ping"]);
    std::thread::sleep(Duration::from_secs(2));
    m.ok(&["--vault", "play", "ping"]);
    assert_eq!(logged(&play, "automations.rule.fired"), fired, "no rule fired over the copied log");
    assert_eq!(logged(&play, "queue.task.started"), ran, "no task ran");
    m.ok(&["--vault", "play", "shutdown"]);
    m.ok(&["shutdown"]);
}

/// `--live` leaves things on, but never replays the original's run: a firing main left unfinished
/// isn't queued again in the copy (it would send that reminder a second time).
#[test]
fn a_live_copy_never_runs_what_the_original_left_unfinished() {
    let m = Machine::new();
    let main = m.main();
    let schedule =
        r#"{"schedule":{"every":3600},"op":"records.list","params":{"collection":"leetcode"},"catch_up":"skip"}"#;
    let added: serde_json::Value = serde_json::from_str(&m.ok(&["call", "scheduler.add", schedule])).unwrap();
    let id = added["trigger_id"].as_str().unwrap().to_owned();
    m.ok(&["shutdown"]);
    // What a stop mid-firing leaves: a firing on disk that never got to run.
    let file = main.join(format!("data/scheduler/triggers/{id}.json"));
    let mut trigger: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    trigger["unfinished"] = serde_json::json!(["2026-01-01T00:00:00Z"]);
    std::fs::write(&file, serde_json::to_vec_pretty(&trigger).unwrap()).unwrap();

    let out = m.ok(&["vaults", "copy", "main", "live", "--live"]);
    assert!(out.contains("left on (--live)"), "{out}");
    let live = m.vault("live");
    let ran = logged(&live, "queue.task.started");
    m.ok(&["--vault", "live", "ping"]);
    std::thread::sleep(Duration::from_secs(2));
    m.ok(&["--vault", "live", "ping"]);
    assert_eq!(logged(&live, "queue.task.started"), ran, "the original's unfinished firing didn't run in the copy");
    let list = json_in(&m, "live", &["scheduler", "list"]);
    let t = list["triggers"].as_array().unwrap().iter().find(|t| t["id"] == id.as_str()).cloned().unwrap();
    assert_eq!((t["enabled"].clone(), t["waiting"].clone()), (serde_json::json!(true), serde_json::json!(0)), "{t}");
    m.ok(&["--vault", "live", "shutdown"]);
}
