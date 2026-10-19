//! `workspaces.activate`, `force_relaunch` and `cleanup` against `FakeLauncher` (ADR 0010 §6)
//! and the in-memory `Ctx` (CLAUDE.md §12 rule 13): nothing is spawned; each step's outcome is
//! scripted, and the steps the module asked for are inspected afterwards.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use shimmer_core::testing::{FakeLauncher, TestEnv};
use shimmer_core::{
    Error, ErrorCode, LaunchBackend, LaunchStep, Launcher, Module, Result, SpawnMode, Step, StepOutcome, StoreBackend,
};
use shimmer_workspaces::Workspaces;
use tokio_util::sync::CancellationToken;

const DEEP_WORK: &str = r#"
[workspace]
label = "Deep Work"

[[step]]
name = "setup"
mode = "supervised"
timeout_s = 60

[[step]]
name = "editor"
mode = "detached"

[[step]]
name = "terminal"
mode = "detached"

[cleanup]
timeout_s = 30

[env]
PROJECT_DIR = "/home/me/code/shimmer"
"#;

const DIRTY_STATE: &str = r#"state = "dirty"

[dirty]
reason = "exit code 1"
step = { index = 1, count = 3, name = "setup" }
failed_at = "2026-09-16T09:12:44Z"
log = "logs/deep-work-old.log"
"#;

fn put(env: &TestEnv, path: &str, text: &str) {
    env.ctx.store.write(path, text).unwrap();
}

fn file(env: &TestEnv, path: &str) -> Option<String> {
    env.backend.read("workspaces", path).unwrap().map(|b| String::from_utf8(b).unwrap())
}

fn topics(env: &TestEnv) -> Vec<String> {
    env.backend.events().into_iter().map(|e| e.topic).collect()
}

fn env_with_deep_work() -> TestEnv {
    let env = TestEnv::new("workspaces");
    put(&env, "deep-work/workspace.toml", DEEP_WORK);
    for script in ["steps/01-setup.sh", "steps/02-editor.sh", "steps/03-terminal.sh", "cleanup.sh"] {
        put(&env, &format!("deep-work/{script}"), "true\n");
    }
    env
}

fn outcome(exit_code: Option<i32>, timed_out: bool, log: &str) -> StepOutcome {
    StepOutcome { session_id: "01SESSION".into(), exit_code, timed_out, log_path: log.into() }
}

/// A supervised step that exited 0 / a detached step that started.
fn ok_supervised() -> Result<StepOutcome> {
    Ok(outcome(Some(0), false, "logs/deep-work-01SESSION-setup.log"))
}
fn ok_detached() -> Result<StepOutcome> {
    Ok(outcome(None, false, ""))
}

fn script(launcher: &FakeLauncher, outcomes: impl IntoIterator<Item = Result<StepOutcome>>) {
    launcher.outcomes.lock().unwrap().extend(outcomes);
}

fn received(launcher: &FakeLauncher) -> Vec<LaunchStep> {
    launcher.received.lock().unwrap().clone()
}

async fn call(w: &Workspaces, env: &TestEnv, op: &str) -> Result<Value> {
    w.handle(op, json!({"id": "deep-work"}), &env.ctx).await
}

fn state_line(env: &TestEnv) -> String {
    file(env, "deep-work/state.toml").unwrap_or_default().lines().next().unwrap_or_default().to_owned()
}

// ---------------------------------------------------------------- activate: success

#[tokio::test]
async fn activate_runs_every_step_in_order_and_goes_active() {
    let env = env_with_deep_work();
    script(&env.launcher, [ok_supervised(), ok_detached(), ok_detached()]);
    let w = Workspaces::default();

    let data = call(&w, &env, "workspaces.activate").await.unwrap();
    // The last step is detached, so there is no log to show (ADR 0025 §6, update-everything).
    assert_eq!(data, json!({"id": "deep-work", "state": "active", "session_id": "01SESSION", "log": ""}));

    let steps = received(&env.launcher);
    let asked: Vec<_> = steps.iter().map(|s| (s.step.clone(), s.mode)).collect();
    assert_eq!(
        asked,
        [
            (
                Step::Launch { index: 1, count: 3, name: "setup".into() },
                SpawnMode::Supervised { timeout: Duration::from_secs(60) }
            ),
            (Step::Launch { index: 2, count: 3, name: "editor".into() }, SpawnMode::Detached),
            (Step::Launch { index: 3, count: 3, name: "terminal".into() }, SpawnMode::Detached),
        ]
    );
    // The workspace's own folder and [env], never a path (ADR 0010 §4).
    assert!(steps.iter().all(|s| s.workspace_id == "deep-work" && s.workspace_dir == "deep-work"));
    assert_eq!(steps[0].user_env, [("PROJECT_DIR".to_owned(), "/home/me/code/shimmer".to_owned())]);

    assert_eq!(state_line(&env), "state = \"active\"");
    assert_eq!(topics(&env), ["workspaces.session.launched"]);
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.payload, json!({"workspace": "deep-work", "session_id": "01SESSION", "forced": false, "steps": 3}));

    let status = call(&w, &env, "workspaces.status").await.unwrap();
    assert_eq!(status["state"], "active");
    assert_eq!(status["last_session"]["outcome"], "launched");
    assert_eq!(status["last_session"]["forced"], false);
}

#[tokio::test]
async fn a_launch_that_ends_with_a_supervised_step_returns_that_steps_log() {
    // A summary step at the end (update-everything): `activate --wait` prints its log.
    let env = TestEnv::new("workspaces");
    let manifest = DEEP_WORK.replace(
        "name = \"terminal\"\nmode = \"detached\"",
        "name = \"terminal\"\nmode = \"supervised\"\ntimeout_s = 5",
    );
    put(&env, "deep-work/workspace.toml", &manifest);
    for script in ["steps/01-setup.sh", "steps/02-editor.sh", "steps/03-terminal.sh", "cleanup.sh"] {
        put(&env, &format!("deep-work/{script}"), "true\n");
    }
    script(
        &env.launcher,
        [ok_supervised(), ok_detached(), Ok(outcome(Some(0), false, "logs/deep-work-01SESSION-terminal.log"))],
    );

    let data = call(&Workspaces::default(), &env, "workspaces.activate").await.unwrap();
    assert_eq!(data["log"], "logs/deep-work-01SESSION-terminal.log");
}

#[tokio::test]
async fn every_step_of_one_attempt_is_asked_to_share_the_first_steps_session_id() {
    // ADR 0010 §9 amendment: the backend mints an id only for an attempt's first `LaunchStep`
    // (`session_id: None`); the module must carry that id forward on every later step of the
    // same attempt so they all get the same `SHIMMER_SESSION_ID`. `received` is what the module
    // sent, independent of whatever the (fake) backend happens to hand back.
    let env = env_with_deep_work();
    script(&env.launcher, [ok_supervised(), ok_detached(), ok_detached()]);
    let w = Workspaces::default();

    call(&w, &env, "workspaces.activate").await.unwrap();

    let steps = received(&env.launcher);
    assert_eq!(steps[0].session_id, None, "the first step mints its own");
    assert_eq!(steps[1].session_id, Some("01SESSION".into()), "later steps carry it forward");
    assert_eq!(steps[2].session_id, Some("01SESSION".into()));
}

#[tokio::test]
async fn an_active_workspace_can_be_activated_again() {
    let env = env_with_deep_work();
    script(&env.launcher, [ok_supervised(), ok_detached(), ok_detached()]);
    script(&env.launcher, [ok_supervised(), ok_detached(), ok_detached()]);
    let w = Workspaces::default();
    call(&w, &env, "workspaces.activate").await.unwrap();
    call(&w, &env, "workspaces.activate").await.unwrap();
    assert_eq!(received(&env.launcher).len(), 6);
}

// ---------------------------------------------------------------- activate: a step fails

#[tokio::test]
async fn a_failed_step_stops_the_launch_and_leaves_it_dirty() {
    let env = env_with_deep_work();
    script(&env.launcher, [Ok(outcome(Some(1), false, "logs/deep-work-01SESSION-setup.log"))]);
    let w = Workspaces::default();

    let e = call(&w, &env, "workspaces.activate").await.unwrap_err();
    // Step 2 and 3 (detached) never started after the failed setup step (ADR 0010 §2a).
    assert_eq!(received(&env.launcher).len(), 1);

    // The task fails with the same error a later activate gets (docs/protocol.md).
    assert_eq!(e.code, ErrorCode::WorkspaceDirty);
    let detail = e.detail.unwrap();
    assert_eq!(detail["failed_step"], "1/3 setup");
    assert_eq!(detail["log"], "logs/deep-work-01SESSION-setup.log");
    assert_eq!(detail["has_cleanup_script"], true);
    assert_eq!(detail["fix"], format!("shimmer workspaces cleanup {}", detail["workspace"].as_str().unwrap()));

    assert_eq!(state_line(&env), "state = \"dirty\"");
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.topic, "workspaces.session.dirty");
    assert_eq!(ev.payload["reason"], "exit code 1");
    assert_eq!(ev.payload["session_id"], "01SESSION");
    assert!(ev.payload.get("recovered").is_none(), "its failed task reports it, not this event");

    // Dirty blocks activation outright (§10.3): nothing runs.
    let e = call(&w, &env, "workspaces.activate").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::WorkspaceDirty);
    assert_eq!(received(&env.launcher).len(), 1);
}

#[tokio::test]
async fn each_kind_of_failure_has_its_own_reason() {
    let cases = [
        (vec![Ok(outcome(Some(0), true, "l"))], "timed out after 60s", "1/3 setup"),
        (vec![Ok(outcome(None, false, "l"))], "killed by a signal", "1/3 setup"),
        (vec![Ok(outcome(Some(2), false, "l"))], "exit code 2", "1/3 setup"),
        (
            vec![ok_supervised(), Err(Error::unavailable("script 'steps/02-editor.sh' is missing"))],
            "could not start: script 'steps/02-editor.sh' is missing",
            "2/3 editor",
        ),
    ];
    for (outcomes, reason, step) in cases {
        let env = env_with_deep_work();
        script(&env.launcher, outcomes);
        let w = Workspaces::default();
        let e = call(&w, &env, "workspaces.activate").await.unwrap_err();
        assert_eq!(e.code, ErrorCode::WorkspaceDirty, "{reason}");
        let ev = env.backend.events().pop().unwrap();
        assert_eq!(ev.payload["reason"], reason);
        assert_eq!(ev.payload["failed_step"], step);
    }
}

#[tokio::test]
async fn if_the_first_step_cannot_start_nothing_ran_so_it_is_not_dirty() {
    // What the real daemon does today: no launch backend is wired in yet (ADR 0010).
    let env = env_with_deep_work();
    script(&env.launcher, [Err(Error::unavailable("no launch backend registered"))]);
    let w = Workspaces::default();

    let e = call(&w, &env, "workspaces.activate").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert_eq!(state_line(&env), "state = \"ready\"", "put back as it was");
    // Not dirty, but not silent either: the claimed launch is recorded as abandoned.
    assert_eq!(topics(&env), ["workspaces.session.abandoned"]);
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(
        ev.payload,
        json!({"workspace": "deep-work", "reason": "step 1 could not start: no launch backend registered",
               "forced": false, "back_to": "ready"})
    );
}

// ---------------------------------------------------------------- activate: refused before anything runs

#[tokio::test]
async fn activate_checks_everything_before_running_anything() {
    // An invalid workspace.toml.
    let env = env_with_deep_work();
    put(&env, "deep-work/workspace.toml", "[workspace]\nlabel = \"x\"\n");
    let e = call(&Workspaces::default(), &env, "workspaces.activate").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);

    // A step's script missing for this platform (ADR 0012 §4).
    let env = env_with_deep_work();
    env.ctx.store.delete("deep-work/steps/03-terminal.sh").unwrap();
    let e = call(&Workspaces::default(), &env, "workspaces.activate").await.unwrap_err();
    assert_eq!(e.message, "deep-work: step 3 (\"terminal\"): missing steps/03-terminal.sh");

    // An unknown workspace.
    let e = Workspaces::default().handle("workspaces.activate", json!({"id": "nope"}), &env.ctx).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);

    assert!(received(&env.launcher).is_empty(), "nothing was run");
    assert!(file(&env, "deep-work/state.toml").is_none(), "nothing was written");
}

#[tokio::test]
async fn a_dirty_workspace_reports_dirty_even_if_its_file_is_also_broken() {
    let env = env_with_deep_work();
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    put(&env, "deep-work/workspace.toml", "broken [");
    let e = call(&Workspaces::default(), &env, "workspaces.activate").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::WorkspaceDirty);
}

// ---------------------------------------------------------------- a launch in progress

/// A launcher that does something before each step, then behaves like `FakeLauncher`.
struct Hooked {
    fake: Arc<FakeLauncher>,
    before: Box<dyn Fn(&LaunchStep) + Send + Sync>,
}

#[async_trait]
impl LaunchBackend for Hooked {
    async fn run(&self, step: &LaunchStep, cancel: &CancellationToken) -> Result<StepOutcome> {
        (self.before)(step);
        self.fake.run(step, cancel).await
    }
}

#[tokio::test]
async fn while_launching_state_toml_names_the_running_step() {
    let mut env = env_with_deep_work();
    script(&env.launcher, [ok_supervised(), ok_detached(), ok_detached()]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (backend, log) = (env.backend.clone(), seen.clone());
    env.ctx.launcher = Launcher::new(Arc::new(Hooked {
        fake: env.launcher.clone(),
        before: Box::new(move |_| {
            let text = String::from_utf8(backend.read("workspaces", "deep-work/state.toml").unwrap().unwrap()).unwrap();
            log.lock().unwrap().push(text);
        }),
    }));

    call(&Workspaces::default(), &env, "workspaces.activate").await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen[1].starts_with("state = \"launching\""), "{}", seen[1]);
    assert!(seen[1].contains("name = \"editor\""), "{}", seen[1]);
    // So a daemon that dies here restarts knowing which step it was on (see workspaces.rs).
}

#[tokio::test]
async fn cancelling_between_steps_leaves_it_dirty() {
    let mut env = env_with_deep_work();
    script(&env.launcher, [ok_supervised()]);
    let cancel = env.ctx.cancel.clone();
    env.ctx.launcher = Launcher::new(Arc::new(Hooked {
        fake: env.launcher.clone(),
        before: Box::new(move |_| cancel.cancel()), // the user cancels while step 1 runs
    }));

    let e = call(&Workspaces::default(), &env, "workspaces.activate").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::WorkspaceDirty);
    assert_eq!(received(&env.launcher).len(), 1, "step 2 never started");
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.payload["reason"], "cancelled during step 1/3 setup");
    assert!(file(&env, "deep-work/state.toml").unwrap().contains("outcome = \"cancelled\""));
}

#[tokio::test]
async fn cancelled_before_any_step_ran_changes_nothing() {
    let env = env_with_deep_work();
    env.ctx.cancel.cancel();
    let e = call(&Workspaces::default(), &env, "workspaces.activate").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ModuleError);
    assert!(received(&env.launcher).is_empty());
    assert_eq!(state_line(&env), "state = \"ready\"");
    assert_eq!(topics(&env), ["workspaces.session.abandoned"]);
    assert_eq!(env.backend.events()[0].payload["reason"], "cancelled before any step ran");
}

#[tokio::test]
async fn a_forced_relaunch_that_never_ran_leaves_a_trail() {
    // Review on #57: forced was recorded, then nothing ran and the state went back to dirty.
    // The log must say why, not just show a forced relaunch followed by silence.
    let env = env_with_deep_work();
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    env.ctx.cancel.cancel();
    call(&Workspaces::default(), &env, "workspaces.force_relaunch").await.unwrap_err();

    assert_eq!(topics(&env), ["workspaces.session.forced", "workspaces.session.abandoned"]);
    let abandoned = &env.backend.events()[1].payload;
    assert_eq!(abandoned["forced"], true);
    assert_eq!(abandoned["back_to"], "dirty");
    assert_eq!(state_line(&env), "state = \"dirty\"", "back to dirty, with its original failure");
    assert!(file(&env, "deep-work/state.toml").unwrap().contains("reason = \"exit code 1\""));
}

// ---------------------------------------------------------------- force_relaunch

#[tokio::test]
async fn force_relaunch_is_recorded_then_launches_a_dirty_workspace() {
    let env = env_with_deep_work();
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    script(&env.launcher, [ok_supervised(), ok_detached(), ok_detached()]);
    let w = Workspaces::default();

    call(&w, &env, "workspaces.force_relaunch").await.unwrap();

    assert_eq!(topics(&env), ["workspaces.session.forced", "workspaces.session.launched"]);
    let forced = &env.backend.events()[0];
    assert_eq!(forced.payload["prior"]["reason"], "exit code 1", "the dirty reason it overrode");
    assert_eq!(env.backend.events()[1].payload["forced"], true);
    assert_eq!(state_line(&env), "state = \"active\"");
    assert_eq!(call(&w, &env, "workspaces.status").await.unwrap()["last_session"]["forced"], true);
}

#[tokio::test]
async fn a_forced_launch_can_fail_again() {
    let env = env_with_deep_work();
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    script(&env.launcher, [Ok(outcome(Some(1), false, "l"))]);
    let e = call(&Workspaces::default(), &env, "workspaces.force_relaunch").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::WorkspaceDirty);
    assert_eq!(topics(&env), ["workspaces.session.forced", "workspaces.session.dirty"]);
    assert_eq!(env.backend.events()[1].payload["forced"], true);
}

#[tokio::test]
async fn force_relaunch_needs_a_dirty_workspace() {
    let env = env_with_deep_work();
    let e = call(&Workspaces::default(), &env, "workspaces.force_relaunch").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("use activate"), "{}", e.message);
    assert!(received(&env.launcher).is_empty());
}

// ---------------------------------------------------------------- cleanup

#[tokio::test]
async fn cleanup_runs_the_script_supervised_and_goes_ready() {
    let env = env_with_deep_work();
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    script(&env.launcher, [Ok(outcome(Some(0), false, "logs/deep-work-cleanup.log"))]);
    let w = Workspaces::default();

    let data = call(&w, &env, "workspaces.cleanup").await.unwrap();
    assert_eq!(data, json!({"id": "deep-work", "state": "ready", "log": "logs/deep-work-cleanup.log"}));
    let step = &received(&env.launcher)[0];
    assert_eq!(step.step, Step::Cleanup);
    assert_eq!(step.mode, SpawnMode::Supervised { timeout: Duration::from_secs(30) });
    assert_eq!(state_line(&env), "state = \"ready\"");
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.topic, "workspaces.session.cleaned");
    assert_eq!(ev.payload, json!({"workspace": "deep-work", "log": "logs/deep-work-cleanup.log"}));
}

#[tokio::test]
async fn a_failed_cleanup_leaves_it_dirty() {
    let env = env_with_deep_work();
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    script(&env.launcher, [Ok(outcome(Some(0), true, "logs/deep-work-cleanup.log"))]);
    let e = call(&Workspaces::default(), &env, "workspaces.cleanup").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ModuleError);
    assert_eq!(e.message, "deep-work: cleanup failed (timed out after 30s); still dirty");
    assert_eq!(e.detail.unwrap()["log"], "logs/deep-work-cleanup.log");
    assert_eq!(state_line(&env), "state = \"dirty\"");
    assert!(topics(&env).is_empty());
}

#[tokio::test]
async fn cleanup_is_refused_when_there_is_nothing_to_clean() {
    // Not dirty.
    let env = env_with_deep_work();
    let e = call(&Workspaces::default(), &env, "workspaces.cleanup").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("not dirty"), "{}", e.message);

    // Dirty, but there's no cleanup script: reset is the way out (§10.3).
    let env = TestEnv::new("workspaces");
    put(
        &env,
        "deep-work/workspace.toml",
        "[workspace]\nlabel = \"x\"\n[[step]]\nname = \"setup\"\nmode = \"detached\"\n",
    );
    put(&env, "deep-work/steps/01-setup.sh", "true\n");
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    let e = call(&Workspaces::default(), &env, "workspaces.cleanup").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("workspaces.reset"), "{}", e.message);
    assert!(received(&env.launcher).is_empty());
}

#[tokio::test]
async fn the_whole_cycle() {
    // ready → activate fails → dirty → activate refused → cleanup → ready → activate → active
    let env = env_with_deep_work();
    let w = Workspaces::default();
    script(&env.launcher, [Ok(outcome(Some(1), false, "l"))]);
    call(&w, &env, "workspaces.activate").await.unwrap_err();
    assert_eq!(call(&w, &env, "workspaces.activate").await.unwrap_err().code, ErrorCode::WorkspaceDirty);
    script(&env.launcher, [Ok(outcome(Some(0), false, "l"))]);
    call(&w, &env, "workspaces.cleanup").await.unwrap();
    script(&env.launcher, [ok_supervised(), ok_detached(), ok_detached()]);
    call(&w, &env, "workspaces.activate").await.unwrap();
    assert_eq!(state_line(&env), "state = \"active\"");
}

#[tokio::test]
async fn the_final_write_checks_the_state_on_disk_first() {
    // Review on #56: the last write of a launch used to assume `launching` without looking.
    // If something rewrote state.toml while a step ran, the launch must not clobber it.
    let mut env = env_with_deep_work();
    script(&env.launcher, [ok_supervised(), ok_detached(), ok_detached()]);
    let store = env.ctx.store.clone();
    env.ctx.launcher = Launcher::new(Arc::new(Hooked {
        fake: env.launcher.clone(),
        before: Box::new(move |step| {
            if let Step::Launch { index: 3, .. } = step.step {
                store.write("deep-work/state.toml", "state = \"ready\"\n").unwrap();
            }
        }),
    }));
    let e = call(&Workspaces::default(), &env, "workspaces.activate").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Internal, "{}", e.message);
    assert_eq!(state_line(&env), "state = \"ready\"", "not overwritten with active");
    assert!(!topics(&env).contains(&"workspaces.session.launched".to_owned()));
}
