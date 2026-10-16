//! In-memory `Ctx` for module tests (§12 rule 13). Enable with the `testing` feature.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::bus::{Emitter, EventSink};
use crate::clock::Clock;
use crate::ctx::{Ctx, ModuleConfig};
use crate::error::{Error, Result};
use crate::event::Event;
use crate::http::{HttpBackend, HttpGateway, HttpResponse};
use crate::ids::{ModuleId, TaskId, TriggerId};
use crate::launcher::{LaunchBackend, LaunchStep, Launcher, StepOutcome};
use crate::queue::{EnqueueRequest, QueueHandle, TaskHandle, TaskSubmitter};
use crate::schedules::{OneTime, ScheduleBackend, Schedules};
use crate::store::{validate_path, StoreBackend, TxPlan, Write};

/// In-memory store that is also the event sink. Committed events are inspectable.
#[derive(Default)]
pub struct MemBackend {
    files: Mutex<BTreeMap<(String, String), Vec<u8>>>,
    events: Mutex<Vec<Event>>,
}

impl MemBackend {
    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

impl StoreBackend for MemBackend {
    fn read(&self, ns: &str, path: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.files.lock().unwrap().get(&(ns.into(), path.into())).cloned())
    }

    fn list(&self, ns: &str, prefix: &str) -> Result<Vec<String>> {
        let dir = format!("{prefix}/");
        Ok(self
            .files
            .lock()
            .unwrap()
            .keys()
            .filter(|(n, p)| n == ns && (prefix.is_empty() || p.starts_with(&dir)))
            .map(|(_, p)| p.clone())
            .collect())
    }

    fn commit(&self, ns: &str, plan: TxPlan) -> Result<()> {
        for w in &plan.writes {
            let (Write::Put { path, .. } | Write::Delete { path }) = w;
            validate_path(path)?;
        }
        let mut files = self.files.lock().unwrap();
        for w in plan.writes {
            match w {
                Write::Put { path, bytes } => {
                    files.insert((ns.into(), path), bytes);
                }
                Write::Delete { path } => {
                    files.remove(&(ns.into(), path));
                }
            }
        }
        self.events.lock().unwrap().extend(plan.events);
        Ok(())
    }
}

impl EventSink for MemBackend {
    fn emit(&self, event: Event) -> Result<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

/// Records what a module enqueued; hands back sequential positions.
#[derive(Default)]
pub struct StubQueue {
    pub requests: Mutex<Vec<EnqueueRequest>>,
}

#[async_trait]
impl TaskSubmitter for StubQueue {
    async fn submit(&self, req: EnqueueRequest) -> Result<TaskHandle> {
        let mut r = self.requests.lock().unwrap();
        let lane = req.lane.clone().unwrap_or_else(crate::LaneId::default_lane);
        r.push(req);
        Ok(TaskHandle { task_id: TaskId::new(), lane, position: r.len() - 1 })
    }
}

/// Serves canned bodies by exact URL; anything else is `unavailable`.
#[derive(Default)]
pub struct StubHttp {
    pub responses: Mutex<BTreeMap<String, HttpResponse>>,
}

#[async_trait]
impl HttpBackend for StubHttp {
    async fn get(&self, _module: &ModuleId, url: &str) -> Result<HttpResponse> {
        self.responses.lock().unwrap().get(url).cloned().ok_or_else(|| Error::unavailable(format!("no stub for {url}")))
    }
}

/// Scripted per-step outcomes for `ctx.launcher` (ADR 0010 §6). Spawns nothing. `run` pops
/// the next scripted outcome — in order, one per call — and always records the step it was
/// given, so a test can assert on `received` even for a step whose outcome it didn't bother
/// scripting. Calling `run` more times than were scripted is a bug in the test, not a silent
/// pass: it panics, naming the step, rather than blocking or fabricating a result.
#[derive(Default)]
pub struct FakeLauncher {
    pub outcomes: Mutex<VecDeque<Result<StepOutcome>>>,
    pub received: Mutex<Vec<LaunchStep>>,
}

#[async_trait]
impl LaunchBackend for FakeLauncher {
    async fn run(&self, step: &LaunchStep, _cancel: &CancellationToken) -> Result<StepOutcome> {
        self.received.lock().unwrap().push(step.clone());
        self.outcomes.lock().unwrap().pop_front().unwrap_or_else(|| {
            panic!("FakeLauncher: no scripted outcome for step {step:?} — script every run() call the test makes")
        })
    }
}

/// Records the one-time schedules modules add (ADR 0035 §5.6). Nothing fires: a test that wants
/// a schedule to run calls the module's op itself.
#[derive(Default)]
pub struct FakeSchedules {
    pub added: Mutex<Vec<(ModuleId, TriggerId, OneTime)>>,
}

impl ScheduleBackend for FakeSchedules {
    fn add_once(&self, module: &ModuleId, once: OneTime) -> Result<TriggerId> {
        let mut added = self.added.lock().unwrap();
        let id = TriggerId::new(format!("{module}-{}", added.len() + 1));
        added.push((module.clone(), id.clone(), once));
        Ok(id)
    }

    fn remove(&self, module: &ModuleId, id: &TriggerId) -> Result<bool> {
        let mut added = self.added.lock().unwrap();
        let before = added.len();
        added.retain(|(m, i, _)| !(m == module && i == id));
        Ok(added.len() < before)
    }
}

/// A `Ctx` wired to in-memory fakes, with the fakes exposed for assertions.
pub struct TestEnv {
    pub ctx: Ctx,
    pub backend: Arc<MemBackend>,
    pub queue: Arc<StubQueue>,
    pub http: Arc<StubHttp>,
    pub launcher: Arc<FakeLauncher>,
    pub schedules: Arc<FakeSchedules>,
    pub clock: Clock,
}

impl TestEnv {
    /// Namespace equals the module id.
    pub fn new(module: &str) -> Self {
        let backend = Arc::new(MemBackend::default());
        let queue = Arc::new(StubQueue::default());
        let http = Arc::new(StubHttp::default());
        let launcher = Arc::new(FakeLauncher::default());
        let clock = Clock::fake(chrono::DateTime::UNIX_EPOCH + chrono::Duration::days(20_000));
        let id = ModuleId::new(module);
        let mut ctx = Ctx::new(
            crate::NamespacedStore::new(module, id.clone(), backend.clone(), clock.clone()),
            HttpGateway::new(id.clone(), http.clone()),
            Emitter::new(id.clone(), backend.clone(), clock.clone()),
            QueueHandle::new(id.clone(), queue.clone()),
            clock.clone(),
            crate::time::LocalTimezone::UTC,
            CancellationToken::new(),
            ModuleConfig::default(),
            id,
        );
        ctx.launcher = Launcher::new(launcher.clone());
        let schedules = Arc::new(FakeSchedules::default());
        ctx.schedules = Schedules::new(ctx.module_id.clone(), schedules.clone());
        Self { ctx, backend, queue, http, launcher, schedules, clock }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::launcher::{SpawnMode, Step};

    fn step(index: u32, count: u32, name: &str) -> LaunchStep {
        LaunchStep {
            workspace_id: "deep-work".into(),
            workspace_dir: "deep-work".into(),
            step: Step::Launch { index, count, name: name.into() },
            mode: SpawnMode::Supervised { timeout: Duration::from_secs(30) },
            user_env: vec![],
            session_id: None,
        }
    }

    fn outcome(session_id: &str, exit_code: Option<i32>, timed_out: bool, log_path: &str) -> StepOutcome {
        StepOutcome { session_id: session_id.into(), exit_code, timed_out, log_path: log_path.into() }
    }

    #[tokio::test]
    async fn fake_launcher_can_be_driven_to_a_timeout() {
        let env = TestEnv::new("workspaces");
        env.launcher.outcomes.lock().unwrap().push_back(Ok(outcome(
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            None,
            true,
            "logs/deep-work.log",
        )));

        let result = env.ctx.launcher.run(&step(1, 1, "launch"), &CancellationToken::new()).await.unwrap();

        assert!(result.timed_out);
        assert_eq!(result.log_path, "logs/deep-work.log");
        let received = env.launcher.received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].workspace_id, "deep-work");
    }

    #[tokio::test]
    async fn fake_launcher_serves_scripted_outcomes_in_order() {
        let env = TestEnv::new("workspaces");
        {
            let mut outcomes = env.launcher.outcomes.lock().unwrap();
            outcomes.push_back(Ok(outcome("s1", Some(0), false, "a")));
            outcomes.push_back(Ok(outcome("s2", Some(1), false, "b")));
        }
        let first = env.ctx.launcher.run(&step(1, 2, "setup"), &CancellationToken::new()).await.unwrap();
        let second = env.ctx.launcher.run(&step(2, 2, "editor"), &CancellationToken::new()).await.unwrap();
        assert_eq!((first.exit_code, second.exit_code), (Some(0), Some(1)));
        assert_eq!(env.launcher.received.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn fake_launcher_records_each_numbered_step_of_a_multi_step_launch() {
        // A workspace's launch is an ordered list of steps, each its own `LaunchStep` (ADR
        // 0010 §2a) — the module runs them one at a time, in order, and can tell exactly
        // which one failed from `Step::Launch`'s `index`/`count`/`name`.
        let env = TestEnv::new("workspaces");
        {
            let mut outcomes = env.launcher.outcomes.lock().unwrap();
            outcomes.push_back(Ok(outcome("s1", Some(0), false, "logs/01-setup.log")));
            outcomes.push_back(Ok(outcome("s2", Some(1), false, "logs/02-editor.log")));
        }
        let _ = env.ctx.launcher.run(&step(1, 3, "setup"), &CancellationToken::new()).await.unwrap();
        let second = env.ctx.launcher.run(&step(2, 3, "editor"), &CancellationToken::new()).await.unwrap();

        assert_eq!(second.exit_code, Some(1));
        let received = env.launcher.received.lock().unwrap();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].step, Step::Launch { index: 1, count: 3, name: "setup".into() });
        assert_eq!(received[1].step, Step::Launch { index: 2, count: 3, name: "editor".into() });
    }

    #[tokio::test]
    async fn fake_launcher_can_also_script_a_backend_level_error() {
        let env = TestEnv::new("workspaces");
        env.launcher.outcomes.lock().unwrap().push_back(Err(Error::unavailable("script missing")));
        let e = env.ctx.launcher.run(&step(1, 1, "launch"), &CancellationToken::new()).await.unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::Unavailable);
    }

    #[tokio::test]
    #[should_panic(expected = "no scripted outcome")]
    async fn fake_launcher_panics_rather_than_silently_passing_when_outcomes_run_out() {
        let env = TestEnv::new("workspaces");
        let _ = env.ctx.launcher.run(&step(1, 1, "launch"), &CancellationToken::new()).await;
    }
}
