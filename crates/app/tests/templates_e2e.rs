//! Every built-in workspace template end to end (ADR 0025), through the real binary and the real
//! daemon: create (and peek), activate, activate again, stop, a failed launch and its cleanup,
//! remove. After every stop and cleanup, **no process the workspace started may be left**.
//!
//! The sandbox is complete: the daemon (and so every script) gets a fake `HOME`, its own `TMPDIR`
//! and a `PATH` that is an allowlist: links to basic system tools (sh, git, curl, python3…) and
//! stand-ins for everything a template drives (editors, terminals, browsers, sudo, gh, docker,
//! npm, updaters…). Nothing real is updated, deleted, pruned or opened. The stand-ins behave like
//! the real programs where it matters: a terminal or a browser window stays running until it is
//! closed, an editor's command returns at once, and every call is written to `calls.log`.
//!
//! Orphans are found by their environment: every process a workspace starts inherits
//! `SHIMMER_WORKSPACE_ID` and this sandbox's `SHIMMER_HOME`, which the daemon itself doesn't have.
//! Linux only (`/proc`).
#![cfg(target_os = "linux")]

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_shimmer");

/// The system tools a template may use for real. Anything not here (and not a stand-in) is
/// missing in the sandbox, so a test can never run a real updater, prune or window.
const SYSTEM_TOOLS: &[&str] = &[
    "sh", "dash", "bash", "cat", "grep", "awk", "sed", "mkdir", "rm", "rmdir", "ls", "sleep", "date", "cut", "tr",
    "head", "tail", "find", "du", "df", "basename", "dirname", "env", "printf", "ps", "sort", "uniq", "wc", "id",
    "paste", "touch", "mv", "cp", "chmod", "ln", "readlink", "tee", "xargs", "stat", "kill", "true", "false", "test",
    "expr", "uname", "getconf", "mktemp", "nohup", "timeout", "seq", "git", "python3", "node", "curl", "ss", "nc",
];

/// Stand-ins, as (name, script). `$SANDBOX` is replaced with the sandbox's path.
const STAND_INS: &[(&str, &str)] = &[
    // Editors: the command hands the folder to the editor and returns.
    ("code", "#!/bin/sh\necho \"code $*\" >>$SANDBOX/calls.log\n"),
    ("xdg-open", "#!/bin/sh\necho \"xdg-open $*\" >>$SANDBOX/calls.log\n"),
    // A terminal: `kitty --directory DIR SHELL -c INNER` runs INNER and stays open (the shell
    // below stays running, as an interactive one would) until it's closed.
    ("kitty", "#!/bin/sh\necho \"kitty $*\" >>$SANDBOX/calls.log\nshift 2\nexec \"$@\"\n"),
    // SHELL: `-c CMD` runs CMD; with no arguments it's an open window, until it's closed.
    ("fakeshell", "#!/bin/sh\nif [ \"$1\" = -c ]; then exec /bin/sh -c \"$2\"; fi\nexec sleep 600\n"),
    // A browser window: stays open until it's closed.
    ("firefox", "#!/bin/sh\necho \"firefox $*\" >>$SANDBOX/calls.log\nexec sleep 600\n"),
    ("notify-send", "#!/bin/sh\necho \"notify-send $*\" >>$SANDBOX/calls.log\n"),
    ("sudo", "#!/bin/sh\n[ \"$1\" = -n ] && shift\necho \"sudo $*\" >>$SANDBOX/calls.log\n"),
    ("ssh", "#!/bin/sh\necho \"ssh $*\" >>$SANDBOX/calls.log\n"),
    // npm: installs into node_modules what package.json lists, without a network.
    (
        "npm",
        "#!/bin/sh\necho \"npm $*\" >>$SANDBOX/calls.log\ncase \"$1\" in\n  prefix) echo \"$HOME/.npm-global\" ;;\n  \
         install|ci) mkdir -p node_modules ;;\nesac\n",
    ),
    ("rustup", "#!/bin/sh\necho \"rustup $*\" >>$SANDBOX/calls.log\n"),
    ("pipx", "#!/bin/sh\necho \"pipx $*\" >>$SANDBOX/calls.log\n"),
    ("uv", "#!/bin/sh\necho \"uv $*\" >>$SANDBOX/calls.log\nexit 3\n"),
    // ollama: `serve` is one long-running process, as the real one is.
    (
        "ollama",
        "#!/bin/sh\necho \"ollama $*\" >>$SANDBOX/calls.log\ncase \"$1\" in\n  list) [ -f $SANDBOX/ollama.up ] ;;\n  \
         serve) touch $SANDBOX/ollama.up; exec sleep 600 ;;\nesac\n",
    ),
];

struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let sb = Self { dir };
        for d in ["bin", "home", "tmp", "shimmer", "config", "state"] {
            std::fs::create_dir_all(sb.path(d)).unwrap();
        }
        // Notify's popups are logged, never shown (ADR 0032 §2).
        std::fs::write(sb.path("shimmer/config.toml"), "[desktop]\nshow = false\n").unwrap();
        for tool in SYSTEM_TOOLS {
            if let Some(real) = which(tool) {
                symlink(real, sb.path("bin").join(tool)).unwrap();
            }
        }
        symlink(BIN, sb.path("bin/shimmer")).unwrap();
        for (name, script) in STAND_INS {
            sb.stand_in(name, script);
        }
        std::fs::write(sb.path("calls.log"), "").unwrap();
        sb
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// The fake home folder, as scripts see it.
    fn home(&self) -> PathBuf {
        self.path("home")
    }

    /// Add or replace a stand-in program.
    fn stand_in(&self, name: &str, script: &str) {
        let path = self.path("bin").join(name);
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, script.replace("$SANDBOX", &self.dir.path().display().to_string())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `shimmer ARGS…` with only the sandbox's environment; the daemon it starts gets the same,
    /// and every script inherits it.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env_clear()
            .env("PATH", self.path("bin"))
            .env("HOME", self.home())
            .env("TMPDIR", self.path("tmp"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("SHELL", self.path("bin/fakeshell"))
            .env("SHIMMER_HOME", self.path("shimmer"))
            .env("SHIMMER_SOCKET", self.path("d.sock"))
            .env("LANG", "C.UTF-8")
            .output()
            .unwrap()
    }

    /// `shimmer ARGS…`, which must succeed; its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        let (out, err) = (text(&o.stdout), text(&o.stderr));
        assert!(o.status.success(), "shimmer {args:?} failed:\n{err}\n{out}\n--- calls:\n{}", self.calls());
        out
    }

    /// `shimmer ARGS…`, which must fail; its stderr.
    fn fails(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(!o.status.success(), "shimmer {args:?} should have failed:\n{}", text(&o.stdout));
        text(&o.stderr)
    }

    /// Make workspace ID from TEMPLATE with these answers, after peeking at it.
    fn create(&self, id: &str, template: &str, answers: &[(&str, &str)]) {
        let peek = self.ok(&["workspaces", "peek", template]);
        assert!(peek.contains("Good to know") && peek.contains("When you activate it"), "{peek}");
        // --scripts shows every step's script and cleanup's: as many as it has steps.
        let steps = peek.lines().find_map(|l| l.strip_prefix("When you activate it (")).expect("a step count");
        let steps: usize = steps.split(' ').next().unwrap().parse().unwrap();
        let scripts = self.ok(&["workspaces", "peek", template, "--scripts"]);
        assert_eq!(scripts.matches("\n── step ").count(), steps, "{scripts}");
        assert!(scripts.contains("· cleanup.sh ──\n"), "{scripts}");
        let helper = self.ok(&["workspaces", "peek", template, "--file", "lib/shimmer-open.sh"]);
        assert!(helper.starts_with("# Shared helpers for workspace templates"), "{helper}");
        let mut args = vec!["workspaces".to_owned(), "new".into(), id.into(), "--from".into(), template.into()];
        for (q, a) in answers {
            args.push("--set".into());
            args.push(format!("{q}={a}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.ok(&args);
    }

    fn activate(&self, id: &str) -> String {
        let out = self.ok(&["workspaces", "activate", id, "--wait"]);
        assert_eq!(self.state(id), "active", "{out}");
        out
    }

    /// Stop, then nothing of the workspace may be left running. Returns what stop said.
    fn stop(&self, id: &str) -> String {
        let out = self.ok(&["workspaces", "stop", id, "--wait"]);
        assert_eq!(self.state(id), "ready", "{out}");
        self.no_orphans(id, "after stop");
        out
    }

    /// A dirty workspace's cleanup, then nothing left running.
    fn cleanup(&self, id: &str) -> String {
        let out = self.ok(&["workspaces", "cleanup", id, "--wait"]);
        assert_eq!(self.state(id), "ready", "{out}");
        self.no_orphans(id, "after cleanup");
        out
    }

    /// Remove it: gone from the list, and nothing of it running.
    fn remove(&self, id: &str) {
        self.ok(&["workspaces", "remove", id]);
        let list = self.ok(&["workspaces", "list"]);
        assert!(!list.lines().any(|l| l.starts_with(&format!("{id} "))), "{list}");
        self.no_orphans(id, "after remove");
    }

    fn state(&self, id: &str) -> String {
        let o = self.run(&["--json", "workspaces", "status", id]);
        let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
        v["state"].as_str().unwrap_or_default().to_owned()
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.path("calls.log")).unwrap_or_default()
    }

    /// Every live process workspace ID started, as (pid, command line).
    fn processes(&self, id: &str) -> Vec<(u32, String)> {
        let home = format!("SHIMMER_HOME={}", self.path("shimmer").display());
        let ws = format!("SHIMMER_WORKSPACE_ID={id}");
        let mut out = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
            let Ok(env) = std::fs::read(entry.path().join("environ")) else { continue };
            let vars: Vec<String> = env.split(|b| *b == 0).map(|v| String::from_utf8_lossy(v).into_owned()).collect();
            if !(vars.contains(&home) && vars.contains(&ws)) {
                continue;
            }
            // A zombie has exited: only its parent's bookkeeping is left.
            let stat = std::fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
            if stat.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')) {
                continue;
            }
            let cmd = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
            out.push((pid, String::from_utf8_lossy(&cmd).replace('\0', " ").trim().to_owned()));
        }
        out
    }

    /// Nothing workspace ID started is still running (allowing a few seconds to exit).
    fn no_orphans(&self, id: &str, when: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let left = self.processes(id);
            if left.is_empty() {
                return;
            }
            if Instant::now() > deadline {
                kill_all(&left);
                panic!("{id}: processes left running {when}:\n{left:#?}\n--- calls:\n{}", self.calls());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Wait until `what` is true, or fail with `why`.
    fn wait_for(&self, why: &str, what: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !what() {
            assert!(Instant::now() < deadline, "timed out waiting for: {why}\n--- calls:\n{}", self.calls());
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Never leave a daemon, or anything a test's workspaces started, behind a failed assertion.
impl Drop for Sandbox {
    fn drop(&mut self) {
        if self.path("d.sock").exists() {
            let _ = self.run(&["shutdown"]);
        }
        let home = format!("SHIMMER_HOME={}", self.path("shimmer").display());
        let mut left = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
            let Ok(env) = std::fs::read(entry.path().join("environ")) else { continue };
            if env.split(|b| *b == 0).any(|v| v == home.as_bytes()) {
                left.push((pid, String::new()));
            }
        }
        kill_all(&left);
    }
}

fn kill_all(procs: &[(u32, String)]) {
    for (pid, _) in procs {
        let _ = Command::new("kill").arg("-9").arg(pid.to_string()).output();
    }
}

fn which(tool: &str) -> Option<PathBuf> {
    ["/usr/local/bin", "/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(tool)).find(|p| p.exists())
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Does anything answer at URL?
fn up(url: &str) -> bool {
    Command::new("curl").args(["-s", "-o", "/dev/null", "--max-time", "1", url]).status().is_ok_and(|s| s.success())
}

/// Run git in DIR, quietly, as a test user.
fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", text(&o.stderr));
}

/// A git repo at DIR with FILES committed, pushed to a bare remote next to it.
fn repo(sb: &Sandbox, dir: &Path, files: &[(&str, &str)]) {
    let remote = sb.path(&format!("remotes/{}.git", dir.file_name().unwrap().to_string_lossy()));
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    for (path, contents) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "init"]);
    git(dir, &["remote", "add", "origin", &remote.display().to_string()]);
    git(dir, &["push", "-q", "-u", "origin", "main"]);
}

// ---------------------------------------------------------------- smoke-test

#[test]
fn smoke_test_launches_stops_fails_and_cleans_up_without_leftovers() {
    let sb = Sandbox::new();
    sb.create("smoke", "smoke-test", &[]);
    // The last step is detached: activate returns once it's started, while its shell is still
    // getting to `exec sleep` (a mkdir, a cat). So wait for it to settle as one sleep, rather than
    // counting at once (#224).
    let one_sleep = || {
        let p = sb.processes("smoke");
        (p.len() == 1 && p[0].1.starts_with("sleep")).then(|| p[0].0)
    };
    sb.activate("smoke");
    sb.wait_for("its background step runs, as one sleep", || one_sleep().is_some());
    let pid = one_sleep().unwrap();
    sb.activate("smoke");
    // The second shell sees the first is running and exits. Settled, and still settled a moment
    // later, it must be the same sleep: a second one would show as two, or as a new pid.
    sb.wait_for("activating again leaves only the first sleep", || one_sleep() == Some(pid));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(one_sleep(), Some(pid), "activating again doesn't start a second one");
    sb.stop("smoke");

    sb.ok(&["workspaces", "edit", "smoke", "--path"]);
    let toml = sb.path("shimmer/data/workspaces/smoke/workspace.toml");
    let text = std::fs::read_to_string(&toml).unwrap().replace("FAIL_AT = \"none\"", "FAIL_AT = \"check\"");
    std::fs::write(&toml, text).unwrap();
    let err = sb.fails(&["workspaces", "activate", "smoke", "--wait"]);
    assert!(err.contains("FAIL_AT"), "the failed step says why: {err}");
    assert_eq!(sb.state("smoke"), "dirty");
    sb.cleanup("smoke");
    sb.remove("smoke");
}

/// How many live processes of workspace ID run a command starting with START (not the shells
/// that started them, whose command lines contain it too).
fn count(sb: &Sandbox, id: &str, start: &str) -> usize {
    sb.processes(id).iter().filter(|(_, cmd)| cmd.starts_with(start)).count()
}

/// Exactly N processes of workspace ID run a command starting with START, and still N a second
/// later. Detached steps (windows, servers) are still starting when activate returns, and a
/// duplicate could appear a moment late, so one look isn't enough either way.
fn settles(sb: &Sandbox, id: &str, start: &str, n: usize, why: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut since: Option<Instant> = None;
    loop {
        let now = count(sb, id, start);
        if now == n {
            let at = *since.get_or_insert_with(Instant::now);
            if at.elapsed() >= Duration::from_secs(1) {
                return;
            }
        } else {
            since = None;
        }
        assert!(
            Instant::now() < deadline,
            "{why}: {now} processes starting '{start}', not {n}:\n{:#?}",
            sb.processes(id)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------- web-project

#[test]
fn web_project_runs_its_server_services_and_windows_and_stop_leaves_nothing() {
    let sb = Sandbox::new();
    let site = sb.home().join("code/site");
    // Services that outlive their command (like `docker compose up -d`), stopped by their own
    // stop command.
    repo(&sb, &site, &[("index.html", "hi"), (".gitignore", "services.pid\n")]);
    let answers = [
        ("PROJECT_DIR", site.to_str().unwrap()),
        ("CODE_EDITOR", "vscode"),
        ("DEV_COMMAND", "python3 -m http.server 18201 --bind 127.0.0.1"),
        ("LOCAL_URL", "http://127.0.0.1:18201"),
        (
            "SERVICES_COMMAND",
            "nohup python3 -m http.server 18211 --bind 127.0.0.1 >/dev/null 2>&1 & echo $! > services.pid",
        ),
        ("SERVICES_STOP_COMMAND", "kill $(cat services.pid)"),
        ("REPO_PAGE", "none"),
        ("LINKS", "https://example.com/docs"),
        ("BROWSER_WINDOW", "separate"),
        ("BROWSER_APP", "firefox"),
        ("TERMINAL_APP", "kitty"),
    ];
    sb.create("site", "web-project", &answers);
    sb.activate("site");
    assert!(up("http://127.0.0.1:18201"), "the dev server answers");
    assert!(up("http://127.0.0.1:18211"), "the services run");
    let calls = sb.calls();
    assert!(calls.contains(&format!("code {}", site.display())), "{calls}");
    assert!(
        calls.contains("firefox --new-instance --profile") && calls.contains("https://example.com/docs"),
        "{calls}"
    );
    settles(&sb, "site", "python3 -m http.server 18201", 1, "running");
    settles(&sb, "site", "sleep 600", 2, "a browser window and a terminal");

    // Again, while active: nothing new.
    let again = sb.activate("site");
    settles(&sb, "site", "python3 -m http.server 18201", 1, &again);
    settles(&sb, "site", "python3 -m http.server 18211", 1, &again);
    settles(&sb, "site", "sleep 600", 2, &format!("no second window: {again}"));

    let stop = sb.stop("site");
    assert!(stop.contains("stopped the dev server"), "{stop}");
    assert!(stop.contains("stopped the services"), "{stop}");
    assert!(stop.contains("closed: the kitty terminal"), "{stop}");
    assert!(stop.contains("closed: the firefox window"), "{stop}");
    assert!(stop.contains("left open, close it yourself: the vscode window"), "{stop}");
    assert!(!up("http://127.0.0.1:18201") && !up("http://127.0.0.1:18211"), "ports freed");

    // A dev server that crashes: the launch goes dirty with its log, and cleanup stops the
    // services that had started before it.
    let toml = sb.path("shimmer/data/workspaces/site/workspace.toml");
    let text = std::fs::read_to_string(&toml).unwrap().replace(
        "python3 -m http.server 18201 --bind 127.0.0.1",
        "python3 -c 'import sys; print(\\\"boom\\\"); sys.exit(3)'",
    );
    std::fs::write(&toml, text).unwrap();
    let err = sb.fails(&["workspaces", "activate", "site", "--wait"]);
    assert!(err.contains("the dev server stopped") && err.contains("boom"), "{err}");
    sb.cleanup("site");
    assert!(!up("http://127.0.0.1:18211"), "cleanup stopped the services");
    sb.remove("site");
}

// ---------------------------------------------------------------- monorepo

#[test]
fn monorepo_runs_every_app_and_its_logs_window_and_stop_leaves_nothing() {
    let sb = Sandbox::new();
    let mono = sb.home().join("code/acme");
    repo(&sb, &mono, &[("package.json", r#"{"name": "acme", "private": true, "workspaces": ["apps/*"]}"#)]);
    let answers = [
        ("PROJECT_DIR", mono.to_str().unwrap()),
        ("APPS", "18301 18302"),
        ("DEV_COMMAND", "python3 -m http.server {app} --bind 127.0.0.1"),
        ("LOCAL_URLS", "http://127.0.0.1:18301 http://127.0.0.1:18302"),
        ("INSTALL_COMMAND", "true"),
        ("REPO_PAGE", "none"),
        ("TERMINAL_APP", "kitty"),
        ("LOGS_TERMINAL", "yes"),
    ];
    sb.create("acme", "monorepo", &answers);
    sb.activate("acme");
    assert!(up("http://127.0.0.1:18301") && up("http://127.0.0.1:18302"), "both apps answer");
    // The logs terminal; a second one only with TERMINAL_COMMAND.
    settles(&sb, "acme", "tail ", 1, "the logs terminal");

    settles(&sb, "acme", "python3 -m http.server", 2, "both apps");
    let again = sb.activate("acme");
    settles(&sb, "acme", "python3 -m http.server", 2, &format!("activating again starts no app twice: {again}"));
    settles(&sb, "acme", "tail ", 1, &format!("nor a second logs terminal: {again}"));

    let stop = sb.stop("acme");
    assert!(!up("http://127.0.0.1:18301") && !up("http://127.0.0.1:18302"), "{stop}");

    // One app that crashes: dirty at once, with that app's log; cleanup stops the other.
    let toml = sb.path("shimmer/data/workspaces/acme/workspace.toml");
    let text = std::fs::read_to_string(&toml).unwrap().replace("APPS = \"18301 18302\"", "APPS = \"18301 nope\"");
    std::fs::write(&toml, text).unwrap();
    let err = sb.fails(&["workspaces", "activate", "acme", "--wait"]);
    assert!(err.contains("nope"), "{err}");
    sb.cleanup("acme");
    sb.remove("acme");
}

// ---------------------------------------------------------------- scratch

#[test]
fn scratch_makes_a_folder_opens_it_and_stop_keeps_the_folder() {
    let sb = Sandbox::new();
    sb.create("try", "scratch", &[("LANGUAGE", "python"), ("CODE_EDITOR", "vscode"), ("TERMINAL_APP", "kitty")]);
    let out = sb.activate("try");
    let folder = out.lines().find_map(|l| l.trim().strip_prefix("folder: ")).expect("says where").to_owned();
    assert!(Path::new(&folder).join("main.py").exists(), "{out}");
    assert!(folder.starts_with(&sb.home().join("scratch").display().to_string()), "{folder}");
    settles(&sb, "try", "sleep 600", 1, "its terminal");

    let stop = sb.stop("try");
    assert!(stop.contains("closed: the kitty terminal") && stop.contains(&format!("kept: {folder}")), "{stop}");
    assert!(Path::new(&folder).join("main.py").exists(), "the folder is kept");

    // A language it doesn't know fails at the check, before anything is made.
    let toml = sb.path("shimmer/data/workspaces/try/workspace.toml");
    let text = std::fs::read_to_string(&toml).unwrap().replace("LANGUAGE = \"python\"", "LANGUAGE = \"klingon\"");
    std::fs::write(&toml, text).unwrap();
    let err = sb.fails(&["workspaces", "activate", "try", "--wait"]);
    assert!(err.contains("LANGUAGE 'klingon'"), "{err}");
    sb.cleanup("try");
    sb.remove("try");
}

/// Change one answer in workspace ID's workspace.toml, as `shimmer workspaces edit` would.
fn set_answer(sb: &Sandbox, id: &str, name: &str, value: &str) {
    let toml = sb.path(&format!("shimmer/data/workspaces/{id}/workspace.toml"));
    let text = std::fs::read_to_string(&toml).unwrap();
    let mut done = false;
    let lines: Vec<String> = text
        .lines()
        .map(|l| {
            if l.starts_with(&format!("{name} = ")) {
                done = true;
                format!("{name} = {}", serde_json::Value::String(value.into()))
            } else {
                l.to_owned()
            }
        })
        .collect();
    assert!(done, "{id} has no answer {name}");
    std::fs::write(&toml, lines.join("\n") + "\n").unwrap();
}

/// Make PATH's modification time DAYS ago, and everything inside it.
fn age(path: &Path, days: u32) {
    let o = Command::new("sh")
        .arg("-c")
        .arg(format!("find \"$1\" -exec touch -d '{days} days ago' {{}} +"))
        .arg("sh")
        .arg(path)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
}

// ---------------------------------------------------------------- vm

#[test]
fn vm_starts_and_stops_a_custom_vm_once_and_only_one_it_started() {
    let sb = Sandbox::new();
    // A "VM" that runs on its own after the start command returns, as a real one does, and
    // answers on its "SSH" port.
    let answers = [
        ("VM_SOFTWARE", "custom"),
        ("VM_NAME", "test-vm"),
        (
            "START_COMMAND",
            "nohup python3 -m http.server 18422 --bind 127.0.0.1 >/dev/null 2>&1 & echo $! > \"$HOME/vm.pid\"",
        ),
        ("STOP_COMMAND", "kill $(cat \"$HOME/vm.pid\")"),
        ("VM_HOST", "127.0.0.1"),
        ("SSH_PORT", "18422"),
        ("TERMINAL_APP", "kitty"),
        ("ON_STOP", "shutdown"),
    ];
    sb.create("box", "vm", &answers);
    let out = sb.activate("box");
    assert!(up("http://127.0.0.1:18422"), "{out}");
    // The terminal step is detached: activate returns once it's started, so wait for its call.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sb.calls().contains("ssh -p 18422 127.0.0.1") {
        assert!(Instant::now() < deadline, "the terminal runs ssh: {}", sb.calls());
        std::thread::sleep(Duration::from_millis(50));
    }
    settles(&sb, "box", "python3 -m http.server 18422", 1, "running");

    let again = sb.activate("box");
    settles(&sb, "box", "python3 -m http.server 18422", 1, &format!("started once: {again}"));
    settles(&sb, "box", "sleep 600", 1, "one terminal");

    let stop = sb.stop("box");
    assert!(stop.contains("shutdown: test-vm") && stop.contains("closed: the kitty terminal"), "{stop}");
    assert!(!up("http://127.0.0.1:18422"), "the VM is stopped");

    // A start that fails: dirty; cleanup leaves the VM alone, since this workspace never
    // started it, rather than failing on its stop command.
    set_answer(&sb, "box", "START_COMMAND", "echo no such vm >&2; false");
    let err = sb.fails(&["workspaces", "activate", "box", "--wait"]);
    assert!(err.contains("no such vm"), "{err}");
    let cleanup = sb.cleanup("box");
    assert!(cleanup.contains("didn't start it"), "{cleanup}");
    sb.remove("box");
}

// ---------------------------------------------------------------- update-everything

#[test]
fn update_everything_runs_what_is_installed_with_sudo_in_a_window_and_leaves_nothing() {
    let sb = Sandbox::new();
    // The system's package manager, which only the stand-in sudo is ever allowed to "run".
    sb.stand_in("apt-get", "#!/bin/sh\necho \"REAL apt-get RAN\" >>$SANDBOX/calls.log\nexit 1\n");
    // Preview first: what update would run, and nothing runs.
    sb.create(
        "look",
        "update-everything",
        &[("MODE", "preview"), ("SYSTEM_UPDATES", "terminal"), ("TERMINAL_APP", "kitty")],
    );
    let preview = sb.activate("look");
    assert!(preview.contains("MODE = preview: nothing ran") && preview.contains("rustup: rustup update"), "{preview}");
    assert!(preview.contains("system (apt-get, with sudo)"), "{preview}");
    let calls = sb.calls();
    for ran in ["sudo", "kitty", "rustup update", "pipx upgrade", "uv tool upgrade", "npm update"] {
        assert!(!calls.contains(ran), "preview ran {ran}: {calls}");
    }
    sb.stop("look");
    sb.remove("look");

    let answers = [
        ("MODE", "update"),
        ("SYSTEM_UPDATES", "terminal"),
        ("TERMINAL_APP", "kitty"),
        ("SKIP", "pipx"),
        ("EXTRA_COMMANDS", "echo dotfiles pulled"),
    ];
    sb.create("upd", "update-everything", &answers);
    let out = sb.activate("upd");
    for line in
        ["updated system (apt-get)", "updated rustup", "updated echo", "skipped pipx (in SKIP)", "failed uv (exit 3)"]
    {
        assert!(out.contains(line), "{line}: {out}");
    }
    let calls = sb.calls();
    assert!(calls.contains("sudo apt-get update") && !calls.contains("REAL apt-get"), "{calls}");
    assert!(calls.contains("rustup update") && !calls.contains("pipx upgrade-all"), "{calls}");

    sb.activate("upd");
    sb.stop("upd");

    // On a schedule with nobody there: sudo that wants a password is reported, not waited on.
    sb.stand_in("sudo", "#!/bin/sh\necho 'sudo: a password is required' >&2\nexit 1\n");
    set_answer(&sb, "upd", "SYSTEM_UPDATES", "passwordless");
    let out = sb.activate("upd");
    assert!(out.contains("sudo wants a password"), "{out}");
    sb.stop("upd");
    sb.remove("upd");
}

// ---------------------------------------------------------------- free-disk

#[test]
fn free_disk_previews_then_removes_only_old_rebuildable_folders() {
    let sb = Sandbox::new();
    // Whether it deletes is the person's call: with no MODE, nothing is made.
    let err = sb.fails(&["workspaces", "new", "free", "--from", "free-disk"]);
    assert!(err.contains("MODE"), "{err}");
    let code = sb.home().join("code");
    let file = |p: &str, text: &str| {
        let p = code.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    file("old-rust/Cargo.toml", "[package]\n");
    file("old-rust/target/CACHEDIR.TAG", "Signature: 8a477f597d28d172789f06886806bc55\n");
    file("old-rust/target/debug/big", &"x".repeat(200_000));
    file("not-cargo/Cargo.toml", "[package]\n");
    file("not-cargo/target/mine.txt", "a folder called target that Cargo didn't make");
    file("old-node/package.json", "{}");
    file("old-node/node_modules/x/index.js", &"x".repeat(100_000));
    file("loose/node_modules/x/index.js", "no package.json next to it");
    file(".hidden/old/Cargo.toml", "[package]\n");
    file(".hidden/old/target/CACHEDIR.TAG", "x");
    age(&code, 90);
    file("new-rust/Cargo.toml", "[package]\n");
    file("new-rust/target/CACHEDIR.TAG", "x");

    sb.create("free", "free-disk", &[("MODE", "preview"), ("CODE_DIRS", "~/code")]);
    let preview = sb.activate("free");
    assert!(preview.contains("would free") && preview.contains("old-rust/target"), "{preview}");
    assert!(code.join("old-rust/target").exists(), "preview removes nothing");

    set_answer(&sb, "free", "MODE", "clean");
    let out = sb.activate("free");
    assert!(out.contains("old-rust/target") && out.contains("old-node/node_modules"), "{out}");
    assert!(!code.join("old-rust/target").exists() && !code.join("old-node/node_modules").exists());
    for kept in
        ["not-cargo/target", "loose/node_modules", ".hidden/old/target", "new-rust/target", "old-rust/Cargo.toml"]
    {
        assert!(code.join(kept).exists(), "{kept} must be kept: {out}");
    }
    sb.stop("free");
    sb.remove("free");
}

// ---------------------------------------------------------------- health

#[test]
fn health_reports_warnings_first_and_notifies_only_then() {
    let sb = Sandbox::new();
    sb.create("hc", "health", &[("DISK_WARN_PERCENT", "1"), ("NOTIFY", "on-warning"), ("BIGGEST_FOLDERS", "2")]);
    let out = sb.activate("hc");
    let first = out.lines().nth(1).unwrap_or_default();
    assert!(first.trim_start().starts_with("! disk"), "warnings first: {out}");
    assert!(out.contains("✓ workspaces: none dirty"), "it asks the daemon: {out}");
    assert!(sb.calls().contains("notify-send Shimmer health"), "{}", sb.calls());

    set_answer(&sb, "hc", "SKIP", "disk");
    std::fs::write(sb.path("calls.log"), "").unwrap();
    let out = sb.activate("hc");
    assert!(!out.contains("disk /") && !sb.calls().contains("notify-send"), "{out}");
    sb.stop("hc");
    sb.remove("hc");
}

// ---------------------------------------------------------------- github-inbox

#[test]
fn github_inbox_lists_what_waits_on_you_through_gh_and_opens_reviews() {
    let sb = Sandbox::new();
    // gh, as it answers after its own --jq: one line per item.
    sb.stand_in(
        "gh",
        "#!/bin/sh\necho \"gh $1 $2\" >>$SANDBOX/calls.log\ncase \"$1 $2\" in\n  \"auth status\") exit 0 ;;\n  \
         \"api graphql\") printf 'review\\tme/app#7\\tAdd login\\tby sam, waiting 3 days\\thttps://github.com/me/app/pull/7\\t3\\n\
         mine\\tme/app#9\\tFix nav\\tready to merge\\thttps://github.com/me/app/pull/9\\t\\n' ;;\n  \
         \"api notifications?per_page=50\") echo 4 ;;\nesac\n",
    );
    sb.create("gh", "github-inbox", &[("OPEN", "reviews"), ("NOTIFY", "when-reviews")]);
    let out = sb.activate("gh");
    assert!(out.contains("Waiting for your review (1):") && out.contains("! me/app#7  Add login"), "{out}");
    assert!(out.contains("me/app#9  Fix nav — ready to merge") && out.contains("Notifications: 4 unread"), "{out}");
    let calls = sb.calls();
    assert!(calls.contains("xdg-open https://github.com/me/app/pull/7") && !calls.contains("pull/9"), "{calls}");
    assert!(calls.contains("notify-send GitHub: 1 PR waiting"), "{calls}");
    sb.stop("gh");

    // Not logged in: stops at the check, saying how to fix it.
    sb.stand_in("gh", "#!/bin/sh\nexit 1\n");
    let err = sb.fails(&["workspaces", "activate", "gh", "--wait"]);
    assert!(err.contains("gh auth login"), "{err}");
    sb.cleanup("gh");
    sb.remove("gh");
}

// ---------------------------------------------------------------- offline-prep

#[test]
fn offline_prep_gets_projects_ready_and_back_online_pushes_only_when_asked() {
    let sb = Sandbox::new();
    let site = sb.home().join("code/site");
    repo(
        &sb,
        &site,
        &[
            ("package.json", r#"{"name": "site"}"#),
            (".gitignore", "node_modules\n"),
            (".env", "DB=postgres://u:p@db.example.com/x\n"),
        ],
    );
    let lib = sb.home().join("code/lib");
    repo(&sb, &lib, &[("Cargo.toml", "[package]\nname = \"lib\"\n")]);
    let extra = sb.path("remotes/extra.git");
    let seed = sb.path("seed/extra");
    repo(&sb, &seed, &[("README", "hi")]);
    let _ = extra;
    // A page to save, served by the test itself.
    std::fs::write(sb.path("page.html"), "<h1>docs</h1>").unwrap();
    let mut server = Command::new("python3")
        .args(["-m", "http.server", "18555", "--bind", "127.0.0.1"])
        .current_dir(sb.dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    sb.wait_for("the page server", || up("http://127.0.0.1:18555/page.html"));

    let answers = [
        ("PROJECTS", "~/code/site ~/code/lib"),
        ("UPDATE_REPOS", "pull"),
        ("CLONE_REPOS", &format!("file://{}", sb.path("remotes/extra.git").display())),
        ("SAVE_PAGES", "http://127.0.0.1:18555/page.html"),
        ("GITHUB_SNAPSHOT", "no"),
        ("AI_MODEL", "tiny-model"),
        ("PUSH_ON_RETURN", "yes"),
    ];
    let answers: Vec<(&str, &str)> = answers.iter().map(|(q, a)| (*q, *a)).collect();
    sb.create("trip", "offline-prep", &answers);
    let out = sb.activate("trip");
    let _ = server.kill();
    let _ = server.wait();
    assert!(out.contains("site: works offline"), "{out}");
    assert!(out.contains("lib: uses rust, but its tool isn't installed here"), "{out}");
    assert!(out.contains("db.example.com"), "remote services are named: {out}");
    assert!(sb.home().join("code/extra/README").exists(), "cloned: {out}");
    assert!(sb.home().join("offline/index.html").exists() && sb.home().join("offline/pages").exists(), "{out}");
    assert!(sb.calls().contains("ollama pull tiny-model"), "{}", sb.calls());
    assert!(sb.processes("trip").is_empty(), "ollama serve, started for the download, is stopped");

    // Back online: a commit made offline is pushed, because PUSH_ON_RETURN = yes.
    std::fs::write(site.join("notes.md"), "written on the plane").unwrap();
    git(&site, &["add", "notes.md"]);
    git(&site, &["commit", "-qm", "offline work"]);
    let stop = sb.stop("trip");
    assert!(stop.contains("site (main): pushed 1 commit"), "{stop}");
    sb.remove("trip");
}

// ---------------------------------------------------------------- reconfigure

#[test]
fn reconfigure_changes_answers_only_while_ready_and_the_next_activate_uses_them() {
    let sb = Sandbox::new();
    sb.create("try", "scratch", &[("LANGUAGE", "python"), ("CODE_EDITOR", "vscode"), ("TERMINAL_APP", "none")]);
    let out = sb.ok(&["workspaces", "reconfigure", "try", "--set", "LANGUAGE=rust"]);
    assert_eq!(
        out.trim(),
        "✓ reconfigured try\n  LANGUAGE: python → rust\n  it takes effect the next time you activate it"
    );
    let again = sb.ok(&["workspaces", "reconfigure", "try", "--set", "LANGUAGE=rust"]);
    assert_eq!(again.trim(), "nothing changed in try");

    let launched = sb.activate("try");
    assert!(launched.contains("-rust") && launched.contains("cargo run"), "{launched}");
    // While it runs, stop would read the new answers to undo what the old ones started.
    let err = sb.fails(&["workspaces", "reconfigure", "try", "--set", "LANGUAGE=go"]);
    assert!(err.contains("stop it first"), "{err}");
    let err = sb.fails(&["workspaces", "reconfigure", "try", "--set", "LANGUAGE="]);
    assert!(err.contains("stop it first"), "{err}");
    sb.stop("try");

    let err = sb.fails(&["workspaces", "reconfigure", "try", "--set", "CODE_EDITOR=notepad"]);
    assert!(err.contains("must be one of"), "{err}");
    sb.remove("try");
}

// ---------------------------------------------------------------- rename

/// The scheduler's triggers, as JSON.
/// The schedules a person added, not modules' own (fetchers' heartbeat, ADR 0028 §5; notify's
/// hourly `notify.flush`).
fn triggers(sb: &Sandbox) -> Vec<serde_json::Value> {
    let o = sb.run(&["--json", "scheduler", "list"]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    v["triggers"].as_array().into_iter().flatten().filter(|t| t["source"]["type"] == "user").cloned().collect()
}

#[test]
fn rename_moves_the_workspace_and_only_with_a_yes_its_schedules() {
    let sb = Sandbox::new();
    sb.create("smoke", "smoke-test", &[]);
    sb.ok(&["scheduler", "add", "cron", "0 9 * * 1-5", "--catch-up", "skip", "workspaces", "activate", "smoke"]);
    sb.ok(&["scheduler", "add", "every", "1d", "--catch-up", "skip", "workspaces", "stop", "smoke"]);
    let paused = triggers(&sb).into_iter().find(|t| t["op"] == "workspaces.stop").unwrap();
    sb.ok(&["scheduler", "pause", paused["id"].as_str().unwrap()]);

    // No terminal, no --move-schedules: renamed, and the schedules are named but left alone.
    let out = sb.ok(&["workspaces", "rename", "smoke", "checks"]);
    assert!(out.starts_with("✓ renamed smoke to checks\n  2 schedules still use the old name smoke:"), "{out}");
    assert!(out.contains("they'll fail until moved"), "{out}");
    assert!(triggers(&sb).iter().all(|t| t["params"]["id"] == "smoke"), "untouched");
    assert_eq!(sb.state("checks"), "ready");
    assert!(sb.fails(&["workspaces", "status", "smoke"]).contains("no workspace 'smoke'"));

    // Back, then with --move-schedules: each added again for the new name, paused if it was.
    sb.ok(&["workspaces", "rename", "checks", "smoke"]);
    let out = sb.ok(&["workspaces", "rename", "smoke", "checks", "--move-schedules"]);
    assert_eq!(out.matches("\n  moved usr-").count(), 2, "{out}");
    let now = triggers(&sb);
    assert_eq!(now.len(), 2);
    assert!(now.iter().all(|t| t["params"]["id"] == "checks"), "{now:#?}");
    let stop = now.iter().find(|t| t["op"] == "workspaces.stop").unwrap();
    assert_eq!(stop["paused"], true, "still paused");
    assert_eq!(now.iter().find(|t| t["op"] == "workspaces.activate").unwrap()["schedule"]["cron"], "0 9 * * 1-5");

    // It works under its new name, and can't be renamed while it runs.
    sb.activate("checks");
    let err = sb.fails(&["workspaces", "rename", "checks", "other"]);
    assert!(err.contains("stop it first"), "{err}");
    sb.stop("checks");
    assert!(sb.fails(&["workspaces", "rename", "checks", "Bad Name"]).contains("not a valid workspace id"));
    sb.remove("checks");
}

// ---------------------------------------------------------------- copy

#[test]
fn a_copy_runs_beside_its_original_and_each_stops_only_its_own() {
    let sb = Sandbox::new();
    let (site, blog) = (sb.home().join("code/site"), sb.home().join("code/blog"));
    repo(&sb, &site, &[("index.html", "site")]);
    repo(&sb, &blog, &[("index.html", "blog")]);
    let answers = [
        ("PROJECT_DIR", site.to_str().unwrap()),
        ("CODE_EDITOR", "vscode"),
        ("DEV_COMMAND", "python3 -m http.server 18601 --bind 127.0.0.1"),
        ("LOCAL_URL", "http://127.0.0.1:18601"),
        ("REPO_PAGE", "none"),
        ("TERMINAL_APP", "kitty"),
    ];
    sb.create("site", "web-project", &answers);
    sb.activate("site");

    // Copied while the original runs: only read, and the copy starts ready.
    let same = sb.ok(&["workspaces", "copy", "site", "twin"]);
    assert!(same.contains("same answers as site"), "{same}");
    assert_eq!(sb.state("twin"), "ready");
    sb.remove("twin");

    let blog_dir = format!("PROJECT_DIR={}", blog.display());
    let out = sb.ok(&[
        "workspaces",
        "copy",
        "site",
        "blog",
        "--label",
        "Blog",
        "--set",
        &blog_dir,
        "--set",
        "DEV_COMMAND=python3 -m http.server 18602 --bind 127.0.0.1",
        "--set",
        "LOCAL_URL=http://127.0.0.1:18602",
    ]);
    assert!(
        out.contains("PROJECT_DIR:") && out.contains("LOCAL_URL: http://127.0.0.1:18601 → http://127.0.0.1:18602"),
        "{out}"
    );
    sb.activate("blog");
    assert!(up("http://127.0.0.1:18601") && up("http://127.0.0.1:18602"), "both run at once");
    settles(&sb, "blog", "sleep 600", 1, "the copy's own terminal");

    sb.stop("blog");
    assert!(up("http://127.0.0.1:18601"), "stopping the copy leaves the original running");
    settles(&sb, "site", "sleep 600", 1, "and the original's terminal open");
    sb.stop("site");
    sb.remove("blog");
    sb.remove("site");
}
