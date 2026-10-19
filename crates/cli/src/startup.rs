//! `shimmer startup on|off|status` (ADR 0034): have the operating system start the daemon at
//! every login, so schedules fire after a restart without anyone typing a command first.
//!
//! It writes one file outside `$SHIMMER_HOME` (a systemd user unit, or a launchd LaunchAgent)
//! under the same rules as packs' links (ADR 0013 §10): never overwrite a file it didn't write,
//! only remove its own, and record it in the CLI's own `cli.toml`. The service manager is only
//! ever reached through [`Services`], so tests never touch a real one.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use shimmer_core::{Error, Result};

use crate::packs::settings::{self, Settings, Startup};
use crate::style::{paint, state, Role};

pub const USAGE: &str = "usage: shimmer startup <command>

commands:
  on                    start Shimmer's daemon every time you log in, from now on
  off                   stop doing that; the daemon running now keeps running
  status                whether it's on, how it starts, and whether it needs updating

Off until you turn it on. Schedules only run while the daemon is running: this is what makes
them fire after a restart. It's per user and needs no sudo: a systemd user service on Linux, a
LaunchAgent on macOS. 'shimmer shutdown' still stops the daemon, and it stays stopped until
your next login or your next command.";

/// The first line of every file `on` writes: how `off` and `on` know a file is theirs.
const MARK: &str = "Written by `shimmer startup on`. Remove with `shimmer startup off`.";
/// The systemd unit's name, and the LaunchAgent's label (ADR 0034 §2, §3).
const UNIT: &str = "shimmer.service";
const LABEL: &str = "io.github.verifiedz.shimmer";
/// `shimmer daemon`'s status when one already runs: success, never restarted (ADR 0034 §5).
const ALREADY_RUNNING: u8 = 3;

#[derive(Clone, Debug, PartialEq)]
pub enum StartupCmd {
    Help,
    On,
    Off,
    Status,
}

/// `rest` is what follows `shimmer startup`.
pub fn parse(rest: Vec<String>) -> std::result::Result<StartupCmd, String> {
    let mut words = rest.into_iter();
    let Some(verb) = words.next() else { return Ok(StartupCmd::Help) };
    if let Some(extra) = words.next() {
        return Err(format!("'startup {verb}' takes nothing after it, not '{extra}'"));
    }
    match verb.as_str() {
        "help" => Ok(StartupCmd::Help),
        "on" => Ok(StartupCmd::On),
        "off" => Ok(StartupCmd::Off),
        "status" => Ok(StartupCmd::Status),
        other if other.starts_with('-') => Err(format!("unknown option '{other}' for 'startup'")),
        other => Err(format!("unknown startup command '{other}': it's on, off or status")),
    }
}

// ---------------------------------------------------------------- what it reads from outside

/// The service manager (`systemctl`, `launchctl`, `id`): the one place this runs a program.
pub trait Services {
    /// Runs `program args`: whether it succeeded, and what it printed (stdout, then stderr).
    /// An error only when it can't be run at all (not installed).
    fn run(&mut self, program: &str, args: &[&str]) -> std::io::Result<(bool, String)>;
}

pub struct Real;

impl Services for Real {
    fn run(&mut self, program: &str, args: &[&str]) -> std::io::Result<(bool, String)> {
        let out = std::process::Command::new(program).args(args).stdin(std::process::Stdio::null()).output()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        Ok((out.status.success(), text.trim().to_owned()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Os {
    Linux,
    Mac,
    Other,
}

/// Everything the commands read from outside, spelled out so tests can point it at temp folders.
pub struct Env {
    pub os: Os,
    /// `~/.config`: where systemd's user manager reads units, unless its own environment names
    /// another `XDG_CONFIG_HOME` ([`unit_file`]). Never the shell's `XDG_CONFIG_HOME`: systemd
    /// doesn't see it, and a unit written there "does not exist" for systemd.
    pub config_dir: Option<PathBuf>,
    /// `$HOME`: where the LaunchAgent goes (`~/Library/LaunchAgents`).
    pub home_dir: Option<PathBuf>,
    pub cli_toml: Option<PathBuf>,
    /// The real `shimmer` binary, symlinks resolved: what the service runs.
    pub exe: std::io::Result<PathBuf>,
    /// `SHIMMER_HOME` and `SHIMMER_SOCKET` as your commands resolve them: the service gets the
    /// same, so it and your commands always find each other (ADR 0034 §2, [`pinned`]).
    pub pinned: Vec<(String, String)>,
    /// A desktop is showing: `WAYLAND_DISPLAY` or `DISPLAY` is set.
    pub display: bool,
    /// Running under Hyprland, for the exact line to add to its config (ADR 0034 §4).
    pub hyprland: bool,
    /// A daemon answers on the socket right now.
    pub daemon_up: bool,
}

/// The data folder and socket your commands use, resolved now, always. A login service runs with
/// the service manager's environment, not your shell's: on Linux a `XDG_DATA_HOME` or
/// `XDG_RUNTIME_DIR` set in your shell's rc isn't there, so leaving them to the service's own
/// defaults could put the login daemon on another folder, two daemons with their schedules
/// never firing after a restart (#242's review).
fn pinned() -> Vec<(String, String)> {
    vec![
        ("SHIMMER_HOME".into(), shimmer_proto::paths::shimmer_home().display().to_string()),
        ("SHIMMER_SOCKET".into(), shimmer_proto::paths::socket_path().display().to_string()),
    ]
}

impl Env {
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        let home_dir = var("HOME").map(PathBuf::from);
        let os = if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Other
        };
        Self {
            os,
            config_dir: home_dir.as_ref().map(|h| h.join(".config")),
            home_dir,
            cli_toml: settings::path(),
            exe: std::env::current_exe().and_then(|p| p.canonicalize()),
            pinned: pinned(),
            display: var("WAYLAND_DISPLAY").is_some() || var("DISPLAY").is_some(),
            hyprland: var("HYPRLAND_INSTANCE_SIGNATURE").is_some(),
            daemon_up: std::os::unix::net::UnixStream::connect(shimmer_proto::paths::socket_path()).is_ok(),
        }
    }

    /// Where `on` writes its file on this system.
    fn service_file(&self) -> Result<PathBuf> {
        let missing = || Error::unavailable("HOME isn't set, so there's nowhere to put the login service");
        match self.os {
            Os::Linux => Ok(self.config_dir.clone().ok_or_else(missing)?.join("systemd/user").join(UNIT)),
            Os::Mac => Ok(self
                .home_dir
                .clone()
                .ok_or_else(missing)?
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist"))),
            Os::Other => Err(Error::unavailable(
                "starting at login works on Linux and macOS for now; Windows comes with the daemon there",
            )),
        }
    }

    fn settings(&self) -> Result<(PathBuf, Settings)> {
        let path = self.cli_toml.clone().ok_or_else(|| {
            Error::unavailable("neither XDG_CONFIG_HOME nor HOME is set, so there's no cli.toml to record it in")
        })?;
        let saved = settings::load(&path)
            .map_err(|problems| Error::invalid_params(format!("{}; fix or remove it first", problems.join("; "))))?;
        Ok((path, saved))
    }
}

/// When the Linux service starts (ADR 0034 §4).
#[derive(Clone, Copy, Debug, PartialEq)]
enum When {
    /// No desktop here (a server over SSH): at any login.
    AnyLogin,
    /// With a desktop session that reaches `graphical-session.target` (GNOME, KDE, uwsm).
    WithDesktop,
    /// A desktop that never reaches that target (Hyprland started directly): at any login, and
    /// the desktop's startup hands it the display with one line.
    DesktopWithoutTarget,
}

impl When {
    fn target(self) -> &'static str {
        match self {
            When::WithDesktop => "graphical-session.target",
            When::AnyLogin | When::DesktopWithoutTarget => "default.target",
        }
    }
}

pub fn run(cmd: &StartupCmd, env: &Env, services: &mut impl Services) -> Result<String> {
    match cmd {
        StartupCmd::Help => Ok(USAGE.into()),
        StartupCmd::On => on(env, services),
        StartupCmd::Off => off(env, services),
        StartupCmd::Status => status(env, services),
    }
}

// ---------------------------------------------------------------- on

fn on(env: &Env, services: &mut impl Services) -> Result<String> {
    // On Linux, systemd is asked once: whether it's there, which login to start with, and where
    // it reads units.
    let linux = if env.os == Os::Linux { Some(linux_when(env, services)?) } else { None };
    let file = match &linux {
        Some((_, manager)) => unit_file_in(env, manager)?,
        None => env.service_file()?,
    };
    let exe =
        env.exe.as_ref().map_err(|e| Error::unavailable(format!("can't tell where shimmer is installed: {e}")))?;
    let (cli_toml, mut saved) = env.settings()?;
    if std::fs::read_to_string(&file).is_ok_and(|text| !is_ours(&text)) {
        return Err(Error::conflict(format!(
            "{} already exists and Shimmer didn't write it, so it's left alone. Move it away, then run 'shimmer startup on' again",
            file.display()
        )));
    }
    let mut out = Vec::new();
    let (text, target) = match (&linux, env.os) {
        (Some((when, _)), _) => (systemd_unit(exe, &env.pinned, *when == When::WithDesktop)?, *when),
        (None, Os::Mac) => (launchd_plist(exe, &env.pinned)?, When::WithDesktop),
        (None, _) => unreachable!("service_file refuses other systems"),
    };
    write_atomic(&file, &text).map_err(|e| Error::internal(format!("can't write {}: {e}", file.display())))?;
    // Recorded before the service manager runs, so 'off' can always remove it if a step fails.
    saved.startup = Some(Startup {
        file: file.clone(),
        target: if env.os == Os::Mac { "login".into() } else { target.target().into() },
    });
    settings::save(&cli_toml, &saved).map_err(|e| {
        Error::internal(format!("wrote {} but can't record it in {}: {e}", file.display(), cli_toml.display()))
    })?;

    match env.os {
        Os::Linux => {
            ok(services, "systemctl", &["--user", "daemon-reload"])?;
            // A daemon already running keeps running: starting the service now would only find it
            // (ADR 0034 §5), and stopping it would cancel its tasks.
            if env.daemon_up {
                ok(services, "systemctl", &["--user", "enable", UNIT])?;
            } else {
                ok(services, "systemctl", &["--user", "enable", "--now", UNIT])?;
            }
            out.push(
                match target {
                    When::AnyLogin => {
                        "✓ on: Shimmer's daemon starts every time you log in (no desktop here, so at any login)"
                    }
                    When::WithDesktop => "✓ on: Shimmer's daemon starts with your desktop every time you log in",
                    When::DesktopWithoutTarget => "✓ on: Shimmer's daemon starts every time you log in",
                }
                .to_owned(),
            );
            if target == When::DesktopWithoutTarget {
                out.push(String::new());
                out.push(display_hint(env.hyprland));
            }
        }
        Os::Mac => {
            // Loaded now only when no daemon runs; otherwise launchd reads it at your next login.
            // `bootout` to reload a rewritten file would stop the running daemon.
            let uid = ok(services, "id", &["-u"])?;
            let domain = format!("gui/{uid}");
            let loaded = services.run("launchctl", &["print", &format!("{domain}/{LABEL}")]).is_ok_and(|(ok, _)| ok);
            if !env.daemon_up && !loaded {
                ok(services, "launchctl", &["bootstrap", &domain, &file.display().to_string()])?;
            }
            out.push("✓ on: Shimmer's daemon starts every time you log in".to_owned());
        }
        Os::Other => {}
    }
    out.push(format!("  {}", file.display()));
    if env.daemon_up {
        out.push(
            "The daemon running now was started by a command and keeps running; from your next login, startup runs it."
                .into(),
        );
    }
    if let Some(warning) = build_folder(exe) {
        out.push(warning_line(&warning));
    }
    Ok(out.join("\n"))
}

/// Where the service file goes. On Linux, where systemd's user manager reads it: its own
/// `XDG_CONFIG_HOME` if its environment has one, else `~/.config` (found testing on GNOME: a
/// shell's `XDG_CONFIG_HOME` put the unit where systemd never looked).
fn unit_file(env: &Env, services: &mut impl Services) -> Result<PathBuf> {
    if env.os != Os::Linux {
        return env.service_file();
    }
    let manager = services.run("systemctl", &["--user", "show-environment"]).map(|(_, said)| said).unwrap_or_default();
    unit_file_in(env, &manager)
}

/// [`unit_file`], given what `systemctl --user show-environment` printed.
fn unit_file_in(env: &Env, manager_env: &str) -> Result<PathBuf> {
    match manager_env.lines().find_map(|l| l.strip_prefix("XDG_CONFIG_HOME=")).map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => Ok(dir.join("systemd/user").join(UNIT)),
        _ => env.service_file(),
    }
}

/// Which login the Linux service starts with, from where `on` runs (ADR 0034 §4).
/// Also returns what `systemctl --user show-environment` printed, for [`unit_file_in`].
fn linux_when(env: &Env, services: &mut impl Services) -> Result<(When, String)> {
    let manager = match services.run("systemctl", &["--user", "show-environment"]) {
        Err(_) => {
            return Err(Error::unavailable(
                "systemd isn't installed here, so there's no user service to start Shimmer at login",
            ))
        }
        Ok((false, said)) => {
            return Err(Error::unavailable(format!(
                "systemd's user services aren't running for you here ({said}), so nothing can start Shimmer at login"
            )))
        }
        Ok((true, said)) => said,
    };
    if !env.display {
        return Ok((When::AnyLogin, manager));
    }
    let desktop_target = services.run("systemctl", &["--user", "is-active", "graphical-session.target"]);
    let when = if desktop_target.is_ok_and(|(ok, _)| ok) { When::WithDesktop } else { When::DesktopWithoutTarget };
    Ok((when, manager))
}

/// The line that hands the desktop's display to the service, for a desktop that never reaches
/// `graphical-session.target`. Restarting there is harmless: the daemon has only just started.
fn display_hint(hyprland: bool) -> String {
    let line = format!(
        "systemctl --user import-environment WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP && systemctl --user try-restart {UNIT}"
    );
    if hyprland {
        format!("So workspaces can open windows, add this line to your Hyprland config (hyprland.conf):\n  exec-once = {line}")
    } else {
        format!("So workspaces can open windows, have your desktop run this when it starts:\n  {line}")
    }
}

/// A binary in a cargo build folder: `cargo clean` or the next build would break the service.
fn build_folder(exe: &Path) -> Option<String> {
    let text = exe.display().to_string();
    (text.contains("/target/debug/") || text.contains("/target/release/")).then(|| {
        format!("{text} is in a build folder; install shimmer (cargo install --path crates/app) and run 'shimmer startup on' again from there")
    })
}

// ---------------------------------------------------------------- off

fn off(env: &Env, services: &mut impl Services) -> Result<String> {
    let expected = env.service_file()?;
    let (cli_toml, mut saved) = env.settings()?;
    // The recorded file, or one at the usual place that says it's ours (cli.toml was lost).
    let file = match saved.startup.as_ref() {
        Some(s) => s.file.clone(),
        None => unit_file(env, services)?,
    };
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if saved.startup.take().is_some() {
                settings::save(&cli_toml, &saved)
                    .map_err(|e| Error::internal(format!("can't update {}: {e}", cli_toml.display())))?;
            }
            return Ok("startup is already off".into());
        }
        Err(e) => return Err(Error::internal(format!("can't read {}: {e}", file.display()))),
    };
    // Only ever its own file, by name and by its first line, whatever cli.toml says.
    if file.file_name() != expected.file_name() || !is_ours(&text) {
        return Err(Error::conflict(format!(
            "{} wasn't written by 'shimmer startup on', so it's left alone",
            file.display()
        )));
    }
    if env.os == Os::Linux {
        // A unit systemd has never loaded (written where it doesn't look) is as good as
        // disabled: say so and carry on removing the file, rather than stopping half way.
        match services.run("systemctl", &["--user", "disable", UNIT]) {
            Ok((true, _)) => {}
            Ok((false, said)) if said.contains("does not exist") || said.contains("not found") => {}
            Ok((false, said)) => {
                return Err(Error::unavailable(format!("'systemctl --user disable {UNIT}' failed: {said}")))
            }
            Err(e) => return Err(Error::unavailable(format!("can't run systemctl: {e}"))),
        }
    }
    std::fs::remove_file(&file).map_err(|e| Error::internal(format!("can't remove {}: {e}", file.display())))?;
    if env.os == Os::Linux {
        ok(services, "systemctl", &["--user", "daemon-reload"])?;
    }
    // launchd keeps a removed agent loaded until logout, and would restart a daemon that crashes
    // meanwhile. Unloaded now when no daemon runs; `bootout` would stop a running one, so then it
    // stays until logout, and says so (#242's review).
    let mut lingers = false;
    if env.os == Os::Mac {
        if env.daemon_up {
            lingers = true;
        } else if let Ok(uid) = ok(services, "id", &["-u"]) {
            let _ = services.run("launchctl", &["bootout", &format!("gui/{uid}/{LABEL}")]);
        }
    }
    saved.startup = None;
    settings::save(&cli_toml, &saved)
        .map_err(|e| Error::internal(format!("can't update {}: {e}", cli_toml.display())))?;
    let mut out = format!(
        "✓ off: Shimmer's daemon won't start at login any more (removed {}).\nThe daemon running now keeps running; 'shimmer shutdown' stops it.",
        file.display()
    );
    if lingers {
        out.push_str("\nUntil you log out, macOS still restarts it if it crashes.");
    }
    Ok(out)
}

// ---------------------------------------------------------------- status

/// A warning line, `warning:` in the warning colour (shared styling doesn't mark it by itself).
fn warning_line(warning: &str) -> String {
    format!("{} {warning}", paint(Role::Warning, "warning:"))
}

fn status(env: &Env, services: &mut impl Services) -> Result<String> {
    env.service_file()?;
    let saved = env.settings().map(|(_, s)| s).unwrap_or_default();
    let file = match saved.startup.as_ref() {
        Some(s) => s.file.clone(),
        None => unit_file(env, services)?,
    };
    let Some(text) = std::fs::read_to_string(&file).ok() else {
        return Ok(format!("{}: Shimmer's daemon starts when a command needs it.\n'shimmer startup on' starts it every time you log in, so schedules run after a restart.", state("off")));
    };
    if !is_ours(&text) {
        return Ok(format!("{}. {} exists, but Shimmer didn't write it.", state("off"), file.display()));
    }
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    match env.os {
        Os::Linux => {
            let desktop = text.contains("WantedBy=graphical-session.target");
            out.push(if desktop {
                format!("{}: starts with your desktop every time you log in", state("on"))
            } else {
                format!("{}: starts every time you log in", state("on"))
            });
            out.push(format!("  {}", file.display()));
            let enabled = services.run("systemctl", &["--user", "is-enabled", UNIT]);
            if !enabled.as_ref().is_ok_and(|(ok, _)| *ok) {
                warnings.push("systemd doesn't have it enabled: run 'shimmer startup on' again".to_owned());
            }
            let active = services.run("systemctl", &["--user", "is-active", UNIT]).is_ok_and(|(ok, _)| ok);
            out.push(running(active, env.daemon_up));
            // A desktop here that the service can't see: workspaces couldn't open windows.
            if env.display {
                let manager =
                    services.run("systemctl", &["--user", "show-environment"]).map(|(_, t)| t).unwrap_or_default();
                let sees = manager.lines().any(|l| l.starts_with("WAYLAND_DISPLAY=") || l.starts_with("DISPLAY="));
                if !sees {
                    warnings.push(format!("the login service can't see your desktop. {}", display_hint(env.hyprland)));
                }
            }
        }
        Os::Mac => {
            out.push(format!("{}: starts every time you log in", state("on")));
            out.push(format!("  {}", file.display()));
            // launchd says `state = running` for a job whose process it started and still has.
            let by_launchd = ok(services, "id", &["-u"])
                .and_then(|uid| ok(services, "launchctl", &["print", &format!("gui/{uid}/{LABEL}")]))
                .is_ok_and(|said| said.contains("state = running"));
            out.push(running(by_launchd, env.daemon_up));
        }
        Os::Other => {}
    }
    match program_in(&text) {
        Some(exe) if !exe.exists() => {
            warnings.push(format!("it runs {}, which no longer exists: run 'shimmer startup on' again", exe.display()))
        }
        Some(exe) if env.exe.as_ref().is_ok_and(|me| *me != exe) => warnings.push(format!(
            "it runs {}, not this shimmer ({}): 'shimmer startup on' points it here",
            exe.display(),
            env.exe.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        )),
        Some(_) => {}
        None => warnings.push("can't tell which shimmer it runs: run 'shimmer startup on' again".to_owned()),
    }
    out.extend(warnings.iter().map(|w| warning_line(w)));
    Ok(out.join("\n"))
}

fn running(by_service: bool, up: bool) -> String {
    match (by_service, up) {
        (true, _) => "  running now, started at login".into(),
        (false, true) => "  running now, started by a command; from your next login, startup runs it".into(),
        (false, false) => "  not running now; it starts at your next login, or with any shimmer command".into(),
    }
}

// ---------------------------------------------------------------- the files (pure)

fn is_ours(text: &str) -> bool {
    text.lines().take(3).any(|l| l.contains(MARK))
}

/// The systemd user unit (ADR 0034 §2). `desktop`: start with `graphical-session.target`.
fn systemd_unit(exe: &Path, pinned: &[(String, String)], desktop: bool) -> Result<String> {
    let mut unit = format!("# {MARK}\n[Unit]\nDescription=Shimmer daemon\n");
    if desktop {
        unit.push_str("PartOf=graphical-session.target\nAfter=graphical-session.target\n");
    }
    // `$` would be read as a variable in ExecStart, `%` as a specifier everywhere.
    let exec = systemd_quote(&exe.display().to_string())?.replace('$', "$$");
    unit.push_str(&format!("\n[Service]\nExecStart={exec} daemon\n"));
    for (key, value) in pinned {
        unit.push_str(&format!("Environment={}\n", systemd_quote(&format!("{key}={value}"))?));
    }
    unit.push_str(&format!(
        "Restart=on-failure\nRestartSec=2\nSuccessExitStatus={ALREADY_RUNNING}\nRestartPreventExitStatus={ALREADY_RUNNING}\n\n[Install]\nWantedBy={}\n",
        if desktop { "graphical-session.target" } else { "default.target" }
    ));
    Ok(unit)
}

fn systemd_quote(text: &str) -> Result<String> {
    if text.contains(['\n', '\r']) {
        return Err(Error::invalid_params(format!("{text:?} has a line break, which a systemd unit can't hold")));
    }
    Ok(format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%")))
}

/// The LaunchAgent (ADR 0034 §3): started at login, restarted only after a failure.
fn launchd_plist(exe: &Path, pinned: &[(String, String)]) -> Result<String> {
    let mut env = String::new();
    if !pinned.is_empty() {
        env.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (key, value) in pinned {
            env.push_str(&format!("    <key>{}</key><string>{}</string>\n", xml(key), xml(value)));
        }
        env.push_str("  </dict>\n");
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- {MARK} -->
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
{env}</dict>
</plist>
"#,
        xml(&exe.display().to_string())
    ))
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The binary a file `on` wrote runs: `ExecStart`'s first word, or `ProgramArguments`' first.
fn program_in(text: &str) -> Option<PathBuf> {
    if let Some(line) = text.lines().find_map(|l| l.strip_prefix("ExecStart=")) {
        let quoted = line.strip_prefix('"')?;
        let mut path = String::new();
        let mut chars = quoted.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => path.push(chars.next()?),
                '"' => break,
                c => path.push(c),
            }
        }
        return Some(PathBuf::from(path.replace("%%", "%").replace("$$", "$")));
    }
    let after = text.split("<key>ProgramArguments</key>").nth(1)?;
    let first = after.split("<string>").nth(1)?.split("</string>").next()?;
    let unescaped = first.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&");
    Some(PathBuf::from(unescaped))
}

/// All or nothing (CLAUDE.md §7.1): a temp file beside it, `fsync`, then `rename`.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".shimmer-startup.{}.tmp", std::process::id()));
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

/// Runs a step that must work; its own words in the error when it doesn't.
fn ok(services: &mut impl Services, program: &str, args: &[&str]) -> Result<String> {
    match services.run(program, args) {
        Ok((true, said)) => Ok(said),
        Ok((false, said)) => Err(Error::unavailable(format!("'{program} {}' failed: {said}", args.join(" ")))),
        Err(e) => Err(Error::unavailable(format!("can't run {program}: {e}"))),
    }
}

// ---------------------------------------------------------------- the one-time nudge

/// ADR 0034 §7: the first time someone makes something that only runs while the daemon runs (a
/// schedule now; automations and reminders later), a note, once. `None` when startup is on, it
/// was said before, or it can't be remembered (so it's never repeated each time).
pub fn nudge(cli_toml: Option<&Path>, os: Os, what: &str) -> Option<String> {
    if os == Os::Other {
        return None;
    }
    let path = cli_toml?;
    let mut saved = settings::load(path).ok()?;
    if saved.told_startup || saved.startup.is_some() {
        return None;
    }
    saved.told_startup = true;
    settings::save(path, &saved).ok()?;
    Some(format!(
        "note: {what} only run while Shimmer's daemon is running.\n      'shimmer startup on' starts it every time you log in."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A service manager that answers from a script and remembers what it was asked.
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        /// Commands (as `program args…`) that fail; everything else succeeds.
        failing: Vec<&'static str>,
        /// What `systemctl --user show-environment` prints.
        manager_env: &'static str,
        /// What a failing command prints.
        failure_says: &'static str,
        missing: bool,
    }

    impl Services for Fake {
        fn run(&mut self, program: &str, args: &[&str]) -> std::io::Result<(bool, String)> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.push(line.clone());
            if self.missing {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            let failed = self.failing.contains(&line.as_str());
            let said = match line.as_str() {
                _ if failed => self.failure_says.to_owned(),
                "id -u" => "501".to_owned(),
                "systemctl --user show-environment" => self.manager_env.to_owned(),
                _ => String::new(),
            };
            Ok((!failed, said))
        }
    }

    struct T {
        _tmp: tempfile::TempDir,
        env: Env,
    }

    fn env(os: Os) -> T {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let exe = root.join("bin/shimmer");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "").unwrap();
        let env = Env {
            os,
            config_dir: Some(root.join("config")),
            home_dir: Some(root.join("home")),
            cli_toml: Some(root.join("config/shimmer/cli.toml")),
            exe: Ok(exe),
            pinned: Vec::new(),
            display: false,
            hyprland: false,
            daemon_up: false,
        };
        T { _tmp: tmp, env }
    }

    fn unit_file(t: &T) -> PathBuf {
        t.env.config_dir.clone().unwrap().join("systemd/user/shimmer.service")
    }

    fn saved(t: &T) -> Settings {
        settings::load(t.env.cli_toml.as_ref().unwrap()).unwrap()
    }

    #[test]
    fn parses_its_three_commands() {
        let p = |w: &[&str]| parse(w.iter().map(|s| s.to_string()).collect());
        assert_eq!(p(&[]), Ok(StartupCmd::Help));
        assert_eq!(p(&["on"]), Ok(StartupCmd::On));
        assert_eq!(p(&["off"]), Ok(StartupCmd::Off));
        assert_eq!(p(&["status"]), Ok(StartupCmd::Status));
        assert!(p(&["now"]).unwrap_err().contains("on, off or status"));
        assert!(p(&["on", "x"]).unwrap_err().contains("nothing after it"));
        assert!(p(&["--force"]).unwrap_err().contains("unknown option"));
    }

    #[test]
    fn the_unit_runs_this_binary_and_never_restarts_one_already_running() {
        let unit = systemd_unit(Path::new("/home/me/.cargo/bin/shimmer"), &[], false).unwrap();
        assert!(unit.starts_with(&format!("# {MARK}\n")));
        assert!(unit.contains("ExecStart=\"/home/me/.cargo/bin/shimmer\" daemon\n"));
        assert!(unit.contains("Restart=on-failure\nRestartSec=2\nSuccessExitStatus=3\nRestartPreventExitStatus=3\n"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("graphical-session") && !unit.contains("Environment="));

        let pinned = [("SHIMMER_HOME".to_owned(), "/data/my shimmer".to_owned())];
        let desktop = systemd_unit(Path::new("/opt/100%/$x\"y/shimmer"), &pinned, true).unwrap();
        assert!(desktop.contains("PartOf=graphical-session.target\nAfter=graphical-session.target\n"));
        assert!(desktop.contains("WantedBy=graphical-session.target"));
        assert!(desktop.contains("Environment=\"SHIMMER_HOME=/data/my shimmer\"\n"));
        assert!(desktop.contains(r#"ExecStart="/opt/100%%/$$x\"y/shimmer" daemon"#), "{desktop}");
        assert_eq!(program_in(&desktop), Some(PathBuf::from("/opt/100%/$x\"y/shimmer")), "read back as written");
        assert!(systemd_unit(Path::new("/a\nb"), &[], false).is_err());
    }

    #[test]
    fn the_plist_runs_this_binary_and_restarts_only_after_a_failure() {
        let pinned = [("SHIMMER_SOCKET".to_owned(), "/tmp/a&b.sock".to_owned())];
        let plist = launchd_plist(Path::new("/Users/me/bin/shim<mer"), &pinned).unwrap();
        assert!(is_ours(&plist), "the mark is in its first lines");
        assert!(plist.contains("<string>/Users/me/bin/shim&lt;mer</string><string>daemon</string>"));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
        assert!(plist.contains("<dict><key>SuccessfulExit</key><false/></dict>"));
        assert!(plist.contains("<key>SHIMMER_SOCKET</key><string>/tmp/a&amp;b.sock</string>"));
        assert_eq!(program_in(&plist), Some(PathBuf::from("/Users/me/bin/shim<mer")));
        assert!(!launchd_plist(Path::new("/x"), &[]).unwrap().contains("EnvironmentVariables"));
    }

    #[test]
    fn on_without_a_desktop_starts_at_any_login_and_starts_now() {
        let t = env(Os::Linux);
        let mut fake = Fake::default();
        let out = on(&t.env, &mut fake).unwrap();
        assert!(out.starts_with("✓ on: Shimmer's daemon starts every time you log in (no desktop here"), "{out}");
        let unit = std::fs::read_to_string(unit_file(&t)).unwrap();
        assert!(unit.contains("WantedBy=default.target"));
        assert_eq!(
            fake.calls,
            [
                "systemctl --user show-environment",
                "systemctl --user daemon-reload",
                "systemctl --user enable --now shimmer.service"
            ]
        );
        assert_eq!(saved(&t).startup, Some(Startup { file: unit_file(&t), target: "default.target".into() }));
    }

    #[test]
    fn on_in_a_desktop_follows_its_session_or_says_the_line_to_add() {
        let mut t = env(Os::Linux);
        t.env.display = true;
        let out = on(&t.env, &mut Fake::default()).unwrap();
        assert!(out.starts_with("✓ on: Shimmer's daemon starts with your desktop"), "{out}");
        assert!(std::fs::read_to_string(unit_file(&t)).unwrap().contains("WantedBy=graphical-session.target"));

        // Hyprland started directly: the target never comes, so any login, plus its config line.
        t.env.hyprland = true;
        let mut fake = Fake { failing: vec!["systemctl --user is-active graphical-session.target"], ..Fake::default() };
        let out = on(&t.env, &mut fake).unwrap();
        assert!(out.contains("exec-once = systemctl --user import-environment WAYLAND_DISPLAY"), "{out}");
        assert!(std::fs::read_to_string(unit_file(&t)).unwrap().contains("WantedBy=default.target"));
        assert_eq!(saved(&t).startup.unwrap().target, "default.target", "on again rewrites its own file");
    }

    #[test]
    fn on_never_stops_or_doubles_a_running_daemon() {
        let mut t = env(Os::Linux);
        t.env.daemon_up = true;
        let mut fake = Fake::default();
        let out = on(&t.env, &mut fake).unwrap();
        assert!(fake.calls.contains(&"systemctl --user enable shimmer.service".to_owned()), "{:?}", fake.calls);
        assert!(!fake.calls.iter().any(|c| c.contains("--now") || c.contains("restart") || c.contains("stop")));
        assert!(out.contains("keeps running; from your next login, startup runs it"), "{out}");
    }

    #[test]
    fn on_never_overwrites_a_file_shimmer_didnt_write() {
        let t = env(Os::Linux);
        std::fs::create_dir_all(unit_file(&t).parent().unwrap()).unwrap();
        std::fs::write(unit_file(&t), "[Service]\nExecStart=/usr/bin/mine\n").unwrap();
        let mut fake = Fake::default();
        let e = on(&t.env, &mut fake).unwrap_err();
        assert!(e.message.contains("Shimmer didn't write it"), "{e}");
        assert_eq!(std::fs::read_to_string(unit_file(&t)).unwrap(), "[Service]\nExecStart=/usr/bin/mine\n");
        assert_eq!(fake.calls, ["systemctl --user show-environment"], "only asked where systemd reads units");
        let e = off(&t.env, &mut fake).unwrap_err();
        assert!(e.message.contains("wasn't written by"), "{e}");
        assert!(unit_file(&t).exists());
    }

    #[test]
    fn on_says_why_when_there_is_no_systemd() {
        let t = env(Os::Linux);
        let e = on(&t.env, &mut Fake { missing: true, ..Fake::default() }).unwrap_err();
        assert!(e.message.contains("systemd isn't installed"), "{e}");
        let e = on(&t.env, &mut Fake { failing: vec!["systemctl --user show-environment"], ..Fake::default() })
            .unwrap_err();
        assert!(e.message.contains("user services aren't running"), "{e}");
        assert!(!unit_file(&t).exists(), "nothing written");
        let e = on(&env(Os::Other).env, &mut Fake::default()).unwrap_err();
        assert!(e.message.contains("Linux and macOS"), "{e}");
    }

    #[test]
    fn a_failed_step_still_leaves_it_removable() {
        let t = env(Os::Linux);
        let mut fake = Fake { failing: vec!["systemctl --user enable --now shimmer.service"], ..Fake::default() };
        assert!(on(&t.env, &mut fake).unwrap_err().message.contains("enable --now"));
        assert!(saved(&t).startup.is_some(), "recorded, so off can remove it");
        off(&t.env, &mut Fake::default()).unwrap();
        assert!(!unit_file(&t).exists());
    }

    #[test]
    fn off_removes_only_its_own_file_and_leaves_the_daemon_running() {
        let t = env(Os::Linux);
        on(&t.env, &mut Fake::default()).unwrap();
        let mut fake = Fake::default();
        let out = off(&t.env, &mut fake).unwrap();
        assert!(out.starts_with("✓ off: Shimmer's daemon won't start at login"), "{out}");
        assert_eq!(fake.calls, ["systemctl --user disable shimmer.service", "systemctl --user daemon-reload"]);
        assert!(!unit_file(&t).exists());
        assert_eq!(saved(&t).startup, None);
        assert_eq!(off(&t.env, &mut Fake::default()).unwrap(), "startup is already off");

        // cli.toml naming some other file is never trusted to delete it.
        let other = t.env.config_dir.clone().unwrap().join("important.txt");
        std::fs::write(&other, format!("# {MARK}\n")).unwrap();
        let mut s = saved(&t);
        s.startup = Some(Startup { file: other.clone(), target: "default.target".into() });
        settings::save(t.env.cli_toml.as_ref().unwrap(), &s).unwrap();
        assert!(off(&t.env, &mut Fake::default()).is_err());
        assert!(other.exists());
    }

    #[test]
    fn the_unit_goes_where_systemd_reads_units() {
        // Found testing on GNOME: a shell's XDG_CONFIG_HOME is never used (Env::from_process
        // takes ~/.config); systemd's own XDG_CONFIG_HOME is.
        let t = env(Os::Linux);
        let sysd = t._tmp.path().join("systemd-config");
        let manager: &'static str =
            Box::leak(format!("HOME=/home/me\nXDG_CONFIG_HOME={}\n", sysd.display()).into_boxed_str());
        let mut fake = Fake { manager_env: manager, ..Fake::default() };
        let out = on(&t.env, &mut fake).unwrap();
        let written = sysd.join("systemd/user/shimmer.service");
        assert!(written.exists() && out.contains(&written.display().to_string()), "{out}");
        assert_eq!(saved(&t).startup.unwrap().file, written);
        assert!(!unit_file(&t).exists(), "not in ~/.config when systemd reads elsewhere");
        assert_eq!(fake.calls.iter().filter(|c| c.contains("show-environment")).count(), 1, "asked once");
        off(&t.env, &mut Fake { manager_env: manager, ..Fake::default() }).unwrap();
        assert!(!written.exists());
    }

    #[test]
    fn off_finishes_when_systemd_never_loaded_the_unit() {
        let t = env(Os::Linux);
        on(&t.env, &mut Fake::default()).unwrap();
        let mut fake = Fake {
            failing: vec!["systemctl --user disable shimmer.service"],
            failure_says: "Failed to disable unit: Unit file shimmer.service does not exist.",
            ..Fake::default()
        };
        assert!(off(&t.env, &mut fake).unwrap().starts_with("✓ off:"));
        assert!(!unit_file(&t).exists());
        assert_eq!(saved(&t).startup, None);
        // Any other failure still stops it, with systemd's words.
        on(&t.env, &mut Fake::default()).unwrap();
        let mut broken = Fake {
            failing: vec!["systemctl --user disable shimmer.service"],
            failure_says: "Access denied",
            ..Fake::default()
        };
        assert!(off(&t.env, &mut broken).unwrap_err().message.contains("Access denied"));
        assert!(unit_file(&t).exists());
    }

    #[test]
    fn status_says_how_it_starts_and_what_needs_fixing() {
        let mut t = env(Os::Linux);
        assert!(status(&t.env, &mut Fake::default())
            .unwrap()
            .starts_with("off: Shimmer's daemon starts when a command"));
        t.env.display = true;
        on(&t.env, &mut Fake::default()).unwrap();

        let mut fine = Fake { manager_env: "HOME=/home/me\nWAYLAND_DISPLAY=wayland-1\n", ..Fake::default() };
        let out = status(&t.env, &mut fine).unwrap();
        assert!(out.starts_with("on: starts with your desktop"), "{out}");
        assert!(out.contains("running now, started at login") && !out.contains("warning"), "{out}");

        // Not enabled, no display in the service, a daemon a command started, and a moved binary.
        t.env.daemon_up = true;
        t.env.exe = Ok(PathBuf::from("/usr/local/bin/shimmer"));
        let mut broken = Fake {
            failing: vec!["systemctl --user is-enabled shimmer.service", "systemctl --user is-active shimmer.service"],
            manager_env: "HOME=/home/me\n",
            ..Fake::default()
        };
        let out = status(&t.env, &mut broken).unwrap();
        assert!(out.contains("started by a command; from your next login"), "{out}");
        assert!(out.contains("warning: systemd doesn't have it enabled"), "{out}");
        assert!(out.contains("warning: the login service can't see your desktop"), "{out}");
        assert!(out.contains("not this shimmer (/usr/local/bin/shimmer)"), "{out}");

        std::fs::remove_file(t._tmp.path().join("bin/shimmer")).unwrap();
        let out = status(&t.env, &mut Fake { manager_env: "DISPLAY=:0", ..Fake::default() }).unwrap();
        assert!(out.contains("which no longer exists"), "{out}");
    }

    #[test]
    fn macos_loads_it_now_only_when_no_daemon_runs() {
        let t = env(Os::Mac);
        let mut fake = Fake { failing: vec!["launchctl print gui/501/io.github.verifiedz.shimmer"], ..Fake::default() };
        let out = on(&t.env, &mut fake).unwrap();
        assert!(out.starts_with("✓ on: Shimmer's daemon starts every time you log in"), "{out}");
        let plist = t.env.home_dir.clone().unwrap().join("Library/LaunchAgents/io.github.verifiedz.shimmer.plist");
        assert!(fake.calls.contains(&format!("launchctl bootstrap gui/501 {}", plist.display())), "{:?}", fake.calls);
        assert_eq!(saved(&t).startup.unwrap().target, "login");

        let mut t = t;
        t.env.daemon_up = true;
        let mut fake = Fake::default();
        on(&t.env, &mut fake).unwrap();
        assert!(!fake.calls.iter().any(|c| c.contains("bootstrap") || c.contains("bootout")), "{:?}", fake.calls);

        let mut fake = Fake::default();
        let out = off(&t.env, &mut fake).unwrap();
        assert!(!plist.exists());
        assert!(!fake.calls.iter().any(|c| c.contains("bootout")), "off never stops the running daemon");
        assert!(out.contains("Until you log out, macOS still restarts it"), "{out}");

        // With no daemon running, off unloads the agent too, so nothing restarts until logout.
        t.env.daemon_up = false;
        on(&t.env, &mut Fake::default()).unwrap();
        let mut fake = Fake::default();
        let out = off(&t.env, &mut fake).unwrap();
        assert!(
            fake.calls.contains(&"launchctl bootout gui/501/io.github.verifiedz.shimmer".to_owned()),
            "{:?}",
            fake.calls
        );
        assert!(!out.contains("Until you log out"), "{out}");
    }

    /// The service always gets the folder and socket your commands use, resolved: on Linux the
    /// login manager doesn't see your shell's XDG_DATA_HOME (#242's review).
    #[test]
    fn the_service_always_gets_the_resolved_folder_and_socket() {
        let keys: Vec<String> = pinned().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["SHIMMER_HOME", "SHIMMER_SOCKET"]);
        assert!(pinned().iter().all(|(_, v)| v.starts_with('/')), "{:?}", pinned());
        let unit = systemd_unit(Path::new("/x/shimmer"), &pinned(), false).unwrap();
        assert!(unit.contains("Environment=\"SHIMMER_HOME=/") && unit.contains("RestartSec=2"), "{unit}");
    }

    #[test]
    fn a_build_folder_binary_is_warned_about() {
        assert!(build_folder(Path::new("/home/me/shimmer/target/debug/shimmer")).is_some());
        assert!(build_folder(Path::new("/home/me/.cargo/bin/shimmer")).is_none());
    }

    #[test]
    fn the_nudge_is_said_once_and_never_when_startup_is_on() {
        let t = env(Os::Linux);
        let cli_toml = t.env.cli_toml.as_deref();
        let note = nudge(cli_toml, Os::Linux, "schedules").unwrap();
        assert!(note.starts_with("note: schedules only run while Shimmer's daemon is running."), "{note}");
        assert!(note.contains("'shimmer startup on'"));
        assert_eq!(nudge(cli_toml, Os::Linux, "schedules"), None, "once");

        let t = env(Os::Linux);
        on(&t.env, &mut Fake::default()).unwrap();
        assert_eq!(nudge(t.env.cli_toml.as_deref(), Os::Linux, "schedules"), None, "already on");
        assert_eq!(nudge(env(Os::Other).env.cli_toml.as_deref(), Os::Other, "schedules"), None);
        assert_eq!(nudge(None, Os::Linux, "schedules"), None);
    }
}
