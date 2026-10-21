use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use shimmer_core::params::decode;
use shimmer_core::{
    CommandSpec, Ctx, Error, ErrorCode, Execution, LaneConfig, Manifest, Module, Result, TriggerSpec, WriteLock,
};

use crate::dedup::{self, RunGuard};
use crate::schedule::{self, Sources, RUN_TASK_OP, TICK_OP};
use crate::source::{self, RawItem, Source};

/// Network-bound and slow, same reasoning as every other fetch-shaped lane (CLAUDE.md §6.1).
pub const LANE: &str = "fetchers";

pub const LIST_OP: &str = "fetchers.list";
pub const STATUS_OP: &str = "fetchers.status";
pub const TEST_OP: &str = "fetchers.test";
pub const RELOAD_OP: &str = "fetchers.reload";
/// Inline: checks the source exists, then enqueues [`RUN_TASK_OP`] itself -- unlike a plain
/// `Execution::Queued` op, whose generic daemon-side ack happens before the module ever sees
/// the params, this lets a request for an unknown source be refused with `not_found` instead
/// of acking "queued" and failing silently once the task actually runs (issue #277's own
/// end-to-end sweep). Diverges from every other queued op in the system on purpose for this
/// one reason; not a pattern to copy elsewhere without the same justification.
pub const RUN_OP: &str = "fetchers.run";

pub const NEW_OP: &str = "fetchers.new";
pub const EDIT_OP: &str = "fetchers.edit";
pub const SHOW_OP: &str = "fetchers.show";
pub const REMOVE_OP: &str = "fetchers.remove";
pub const RESTORE_OP: &str = "fetchers.restore";
pub const PURGE_OP: &str = "fetchers.purge";
pub const ENABLE_OP: &str = "fetchers.enable";
pub const DISABLE_OP: &str = "fetchers.disable";
pub const RENAME_OP: &str = "fetchers.rename";
pub const COPY_OP: &str = "fetchers.copy";

/// Caps `fetchers.item.found`'s `raw_data` (ADR 0028 §3) -- big enough for a job posting's
/// text, small enough that one large item can't become the next cache-bloat problem.
const MAX_RAW_DATA_CHARS: usize = 8 * 1024;
/// However many fields an item has, each still keeps enough characters to be recognisable
/// rather than being cut to nothing.
const MIN_FIELD_CHARS: usize = 64;

#[derive(Default)]
pub struct Fetchers {
    /// A scheduled tick and a manual `fetchers.run` for the same source must never overlap
    /// (ADR 0028 §5).
    running: RunGuard,
    /// Serialises `last_run.toml`'s read-modify-write (`schedule::record_run`) against itself
    /// -- the `fetchers` lane runs up to 8 fetches at once, each ending in its own call to
    /// `record_run`, and an unlocked read-check-write there can drop whichever of two
    /// concurrently-finishing sources' updates writes second (PR #126 review). A different
    /// lock from `running`: that one is per-source exclusivity for a whole run, this one is
    /// held only across the few lines that touch the shared last-run file.
    write: WriteLock,
    /// Every source file, read once at `init` and re-read only on `fetchers.reload` (ADR 0028
    /// §5) -- never re-scanned on every op call, the same shape `automations`' own rule cache
    /// uses.
    state: Mutex<Sources>,
}

impl Fetchers {
    fn state(&self) -> std::sync::MutexGuard<'_, Sources> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Re-read every source file.
    fn load(&self, ctx: &Ctx) -> Result<()> {
        *self.state() = Sources::load(ctx)?;
        Ok(())
    }
}

/// Every topic fetchers emits, with a line on it for `shimmer automations events` (ADR 0035 §5.5).
const TOPICS: [(&str, &str); 12] = [
    ("fetchers.item.found", "a source found a new item"),
    ("fetchers.fetch.finished", "a fetch of a source finished"),
    ("fetchers.fetch.failed", "a fetch of a source failed"),
    ("fetchers.source.created", "a source was made"),
    ("fetchers.source.edited", "a source's file was changed"),
    ("fetchers.source.removed", "a source was removed (it can be restored)"),
    ("fetchers.source.restored", "a removed source came back"),
    ("fetchers.source.purged", "removed sources were deleted for good"),
    ("fetchers.source.renamed", "a source was given a new id"),
    ("fetchers.source.copied", "a new source was made from another (switched off)"),
    ("fetchers.source.enabled", "a source was switched on"),
    ("fetchers.source.disabled", "a source was switched off"),
];

#[async_trait]
impl Module for Fetchers {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "fetchers".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            namespace: "fetchers".into(),
            topics: TOPICS.iter().map(|(t, _)| t.to_string()).collect(),
            topic_descriptions: TOPICS.iter().map(|(t, d)| (t.to_string(), d.to_string())).collect(),
            // hn-whoishiring (ADR 0028 §10) needs the real ctx.http.
            capabilities: vec!["network".into()],
        }
    }

    async fn init(&self, ctx: &Ctx) -> Result<()> {
        self.load(ctx)
    }

    fn lanes(&self) -> Vec<LaneConfig> {
        vec![LaneConfig::new(LANE, 8)]
    }

    fn triggers(&self) -> Vec<TriggerSpec> {
        vec![schedule::trigger()]
    }

    fn commands(&self) -> Vec<CommandSpec> {
        vec![
            CommandSpec {
                op: TICK_OP.into(),
                summary: "Internal: check configured sources and enqueue a fetch for each one due (the scheduler \
                          calls this, not a person)"
                    .into(),
                params_schema: json!({"type": "null"}),
                execution: Execution::Queued { lane: LANE.into() },
            },
            CommandSpec {
                op: RUN_OP.into(),
                summary: "Fetch one source now".into(),
                params_schema: json!({
                    "type": "object", "required": ["id"],
                    "properties": {
                        "id": {"type": "string"},
                        "title_contains": {"type": "array", "items": {"type": "string"}},
                        "title_excludes": {"type": "array", "items": {"type": "string"}},
                    },
                }),
                // Inline, not Queued (see RUN_OP's own doc comment): it checks the source
                // exists itself, then enqueues RUN_TASK_OP for the real work.
                execution: Execution::Inline,
            },
            CommandSpec {
                op: RUN_TASK_OP.into(),
                summary: "Internal: the real fetch (fetchers.run enqueues this after checking the source exists)"
                    .into(),
                params_schema: json!({
                    "type": "object", "required": ["id"],
                    "properties": {
                        "id": {"type": "string"},
                        "title_contains": {"type": "array", "items": {"type": "string"}},
                        "title_excludes": {"type": "array", "items": {"type": "string"}},
                    },
                }),
                execution: Execution::Queued { lane: LANE.into() },
            },
            CommandSpec {
                op: LIST_OP.into(),
                summary: "Every known source, its configured state, and its last run (or, with \
                          'removed', every removed one)"
                    .into(),
                params_schema: json!({"type": "object", "properties": {"removed": {"type": "boolean"}}}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: STATUS_OP.into(),
                summary: "The whole feature at a glance: how many sources, how many enabled, any problems".into(),
                params_schema: json!({"type": "null"}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: TEST_OP.into(),
                summary:
                    "Fetch one page from a source and show its parsed items, without saving or marking anything seen"
                        .into(),
                params_schema: json!({
                    "type": "object", "required": ["id"],
                    "properties": {
                        "id": {"type": "string"},
                        "title_contains": {"type": "array", "items": {"type": "string"}},
                        "title_excludes": {"type": "array", "items": {"type": "string"}},
                    },
                }),
                execution: Execution::Queued { lane: LANE.into() },
            },
            CommandSpec {
                op: RELOAD_OP.into(),
                summary: "Re-read the source files after editing them by hand".into(),
                params_schema: json!({"type": "null"}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: NEW_OP.into(),
                summary: "Make a new source".into(),
                params_schema: source_input_schema(),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: EDIT_OP.into(),
                summary: "Replace a source's whole file text".into(),
                params_schema: json!({
                    "type": "object", "required": ["id", "text"],
                    "properties": {"id": {"type": "string"}, "text": {"type": "string"}},
                }),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: SHOW_OP.into(),
                summary: "One source in full, with its file text".into(),
                params_schema: json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: REMOVE_OP.into(),
                summary: "Remove a source (it can be restored)".into(),
                params_schema: json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: RESTORE_OP.into(),
                summary: "Bring a removed source back".into(),
                params_schema: json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: PURGE_OP.into(),
                summary: "Delete removed sources for good: one by id, or all of them (asks first)".into(),
                params_schema: json!({
                    "type": "object",
                    "properties": {"id": {"type": "string"}, "confirm": {"type": "object"}},
                }),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: ENABLE_OP.into(),
                summary: "Switch a source on".into(),
                params_schema: json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: DISABLE_OP.into(),
                summary: "Switch a source off".into(),
                params_schema: json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: RENAME_OP.into(),
                summary: "Give a source a new id".into(),
                params_schema: json!({
                    "type": "object", "required": ["id", "to"],
                    "properties": {"id": {"type": "string"}, "to": {"type": "string"}},
                }),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: COPY_OP.into(),
                summary: "A new source from another's file text, switched off".into(),
                params_schema: json!({
                    "type": "object", "required": ["id", "to"],
                    "properties": {
                        "id": {"type": "string"}, "to": {"type": "string"}, "label": {"type": "string"},
                    },
                }),
                execution: Execution::Inline,
            },
        ]
    }

    async fn handle(&self, op: &str, params: Value, ctx: &Ctx) -> Result<Value> {
        match op {
            TICK_OP => self.tick(ctx).await,
            RUN_OP => self.run(ctx, params).await,
            RUN_TASK_OP => {
                let p: FetchParams = decode(params)?;
                let filter = TitleFilterOverride::from_params(p.title_contains, p.title_excludes)?;
                self.fetch(ctx, p.id, filter).await
            }
            LIST_OP => {
                let p: ListParams = decode(params)?;
                self.list(ctx, p).await
            }
            STATUS_OP => self.status(ctx),
            TEST_OP => {
                let p: FetchParams = decode(params)?;
                let filter = TitleFilterOverride::from_params(p.title_contains, p.title_excludes)?;
                self.test(ctx, p.id, filter).await
            }
            RELOAD_OP => {
                self.load(ctx)?;
                let st = self.state();
                Ok(json!({"sources": st.sources.len(), "problems": problems_of(&st)}))
            }
            NEW_OP => {
                let p: SourceInput = decode(params)?;
                self.create(ctx, p)
            }
            EDIT_OP => {
                let p: EditInput = decode(params)?;
                self.edit(ctx, &p.id, &p.text)
            }
            SHOW_OP => {
                let p: Id = decode(params)?;
                self.show(ctx, &p.id)
            }
            REMOVE_OP => {
                let p: Id = decode(params)?;
                self.remove(ctx, &p.id)
            }
            RESTORE_OP => {
                let p: Id = decode(params)?;
                self.restore(ctx, &p.id)
            }
            PURGE_OP => {
                let p: Purge = decode(params)?;
                self.purge(ctx, p)
            }
            ENABLE_OP => {
                let p: Id = decode(params)?;
                self.set_enabled(ctx, &p.id, true)
            }
            DISABLE_OP => {
                let p: Id = decode(params)?;
                self.set_enabled(ctx, &p.id, false)
            }
            RENAME_OP => {
                let p: MoveParams = decode(params)?;
                self.rename(ctx, p)
            }
            COPY_OP => {
                let p: CopyParams = decode(params)?;
                self.copy(ctx, p)
            }
            _ => Err(Error::unknown_op(format!("fetchers has no op '{op}'"))),
        }
    }
}

#[derive(Deserialize)]
struct FetchParams {
    id: String,
    #[serde(default)]
    title_contains: Option<Vec<String>>,
    #[serde(default)]
    title_excludes: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

#[derive(Default, Deserialize)]
struct ListParams {
    #[serde(default)]
    removed: bool,
}

/// `fetchers.new`/`.edit`'s params (ADR 0028 §9): a source's complete settings, structured
/// rather than raw file text -- `kind`'s own fields are validated by `Kind::parse` the same
/// way a hand-edited file's `[kind]` table is, through `schedule::render` then
/// `SourceConfig::parse` (one validation path for both). `kind` stays optional for now: the
/// fixed, compiled-in registry (`source::registry`) is only removed in ADR 0028 §10, a later
/// step, and a bare entry for one of those ids must keep working until then.
#[derive(Deserialize)]
struct SourceInput {
    id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    enabled: bool,
    interval: schedule::IntervalSpec,
    #[serde(default)]
    params: Map<String, Value>,
    #[serde(default)]
    title_contains: Vec<String>,
    #[serde(default)]
    title_excludes: Vec<String>,
}

fn source_input_schema() -> Value {
    json!({
        "type": "object", "required": ["id", "interval"],
        "properties": {
            "id": {"type": "string"},
            "label": {"type": "string"},
            "kind": {"type": "string"},
            "enabled": {"type": "boolean"},
            "interval": {},
            "params": {"type": "object"},
            "title_contains": {"type": "array", "items": {"type": "string"}},
            "title_excludes": {"type": "array", "items": {"type": "string"}},
        },
    })
}

/// `fetchers.edit`'s params (ADR 0028 §9, amended by #305's review): the whole replacement
/// file text, same shape as `automations.edit`'s -- not a `SourceInput`, so nothing not
/// explicitly retyped is silently reset.
#[derive(Deserialize)]
struct EditInput {
    id: String,
    text: String,
}

#[derive(Deserialize)]
struct MoveParams {
    id: String,
    to: String,
}

#[derive(Deserialize)]
struct CopyParams {
    id: String,
    to: String,
    label: Option<String>,
}

#[derive(Deserialize)]
struct Purge {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    confirm: Option<Value>,
}

/// An ad-hoc title filter from a `fetchers.run`/`fetchers.test` caller (the CLI's
/// `--contains`/`--excludes` flags), scoped to one call -- it replaces, never merges with,
/// whatever this source's own file configures (ADR 0028 §14).
/// `None` means neither flag was given at all, so the configured filter (if any) applies
/// exactly as before this existed.
struct TitleFilterOverride {
    contains: Vec<String>,
    excludes: Vec<String>,
}

impl TitleFilterOverride {
    fn from_params(contains: Option<Vec<String>>, excludes: Option<Vec<String>>) -> Result<Option<Self>> {
        if contains.is_none() && excludes.is_none() {
            return Ok(None);
        }
        let contains = contains.unwrap_or_default();
        let excludes = excludes.unwrap_or_default();
        // An empty or whitespace term is a substring of every title, so it would exclude or
        // include everything -- the same reason config load already rejects one (#126's
        // review, B3). `fetchers.run`/`fetchers.test` reach this past the CLI's own
        // comma-split filtering, so an automation or `shimmer call` sending `[""]` must be
        // rejected here too, not just stop marking items seen (#279's re-review).
        for (what, terms) in [("title_contains", &contains), ("title_excludes", &excludes)] {
            if terms.iter().any(|t| t.trim().is_empty()) {
                return Err(Error::invalid_params(format!(
                    "{what}: an empty term matches every title -- remove it (a trailing comma?)"
                )));
            }
        }
        Ok(Some(Self { contains, excludes }))
    }
}

impl Fetchers {
    async fn tick(&self, ctx: &Ctx) -> Result<Value> {
        let sources = self.state().sources.clone();
        self.run_due(ctx, &sources).await
    }

    /// Split from [`Fetchers::tick`] so the enqueue-and-skip-if-running logic is testable
    /// against a hand-built map, without needing to write any source files at all.
    async fn run_due(&self, ctx: &Ctx, sources: &BTreeMap<String, schedule::SourceConfig>) -> Result<Value> {
        let last_run = schedule::load_last_run(ctx)?;
        let due = schedule::due_sources(sources, &last_run, ctx.clock.now());

        let (mut enqueued, mut already_running) = (Vec::new(), Vec::new());
        for source in due {
            // A peek only -- an optimization to avoid enqueuing a source we can already see
            // is running. `fetch`'s own `try_acquire` is the real, race-free guarantee.
            if self.running.is_running(&source) {
                already_running.push(source);
                continue;
            }
            ctx.queue.enqueue(RUN_TASK_OP, json!({"id": source})).await?;
            enqueued.push(source);
        }
        Ok(json!({"enqueued": enqueued, "already_running": already_running}))
    }

    /// `fetchers.run` (inline): refuses an unknown source with `not_found` before acking
    /// anything, rather than acking "queued" and only failing once the task actually runs
    /// (issue #277). Enqueues [`RUN_TASK_OP`] itself for a source that does exist, replicating
    /// the same `{"queued": true, "task_id", "lane", "position"}` shape the daemon's generic
    /// `Execution::Queued` path would have returned -- a client can't tell the difference.
    ///
    /// The one cost: a task this enqueues has `Origin::Module`, not `Origin::User` --
    /// `Ctx`/`QueueHandle` has no way for a module to submit as the user, by design (CLAUDE.md
    /// §5: "adding a field is a security decision"). So `shimmer queue list` shows "module:
    /// fetchers" for a person's own `fetchers run`, same as a heartbeat-triggered one, rather
    /// than attributing it to them directly. Priority and lane ordering are unaffected --
    /// `Origin::User` and `Origin::Module` both get `Priority::Normal`, the same arrival-order
    /// queue (CLAUDE.md §6.2) -- this is a display-only cost, accepted for the correctness fix.
    async fn run(&self, ctx: &Ctx, params: Value) -> Result<Value> {
        let p: FetchParams = decode(params.clone())?;
        if p.id.trim().is_empty() {
            return Err(Error::invalid_params("id must not be empty"));
        }
        let sources = self.state().clone();
        if source::resolve(&sources, &p.id)?.is_none() {
            return Err(Error::not_found(format!("no source '{}'", p.id)));
        }
        let handle = ctx.queue.enqueue(RUN_TASK_OP, params).await?;
        Ok(json!({
            "queued": true,
            "task_id": handle.task_id.to_string(),
            "lane": handle.lane.to_string(),
            "position": handle.position,
        }))
    }

    async fn fetch(&self, ctx: &Ctx, source_id: String, filter_override: Option<TitleFilterOverride>) -> Result<Value> {
        if source_id.trim().is_empty() {
            return Err(Error::invalid_params("id must not be empty"));
        }
        let sources = self.state().clone();
        let source = source::resolve(&sources, &source_id)?
            .ok_or_else(|| Error::not_found(format!("no source '{source_id}'")))?;
        let _permit = self
            .running
            .try_acquire(&source_id)
            .ok_or_else(|| Error::module_error(format!("source '{source_id}' is already fetching")))?;

        let configured = sources.sources.get(&source_id);
        let outcome = self.run_source(ctx, source.as_ref(), filter_override.as_ref(), configured).await;
        // Recorded regardless of outcome -- including failure -- so a broken source is
        // retried at most once per interval (ADR 0028 §5, §6: "after that, wait for the
        // next trigger"), not hammered every tick. Held only across this call -- up to 8
        // fetches finish around the same time on this lane, and this file's own
        // read-modify-write must not interleave with another source's (PR #126 review).
        //
        // The one exception is cancellation (#126's review, B11): the user stopping a run
        // isn't the source having been fetched, and recording it made the next run wait a
        // full interval (a day, for `daily`) having fetched nothing.
        if !ctx.cancel.is_cancelled() {
            let _g = self.write.lock();
            schedule::record_run(ctx, &source_id, ctx.clock.now())?;
        }

        match outcome {
            Ok(new_items) => {
                let _ = ctx.bus.emit("fetchers.fetch.finished", json!({"source": source_id, "new_items": new_items}));
                Ok(json!({"source": source_id, "new_items": new_items}))
            }
            Err(e) => {
                let reason = classify_failure(ctx, &e);
                let _ = ctx
                    .bus
                    .emit("fetchers.fetch.failed", json!({"source": source_id, "error": e.message, "reason": reason}));
                Err(e)
            }
        }
    }

    /// Every source the user has actually opted into seeing: a fixed-registry source once it
    /// has its own file, plus every configured `kind` instance (ADR 0028 §2a/§19),
    /// cross-referenced with that entry, its last run and whether it's currently mid-fetch
    /// (ADR 0028 §9 merges the prior design's separate per-source `list`/`status` into one).
    /// `{"removed": true}` lists removed sources instead (ADR 0038 §3), by id only -- a
    /// removed source may no longer parse, so there is nothing else safe to show about it
    /// without reading its file.
    async fn list(&self, ctx: &Ctx, p: ListParams) -> Result<Value> {
        if p.removed {
            return Ok(json!({"removed": schedule::removed(ctx)?}));
        }
        let last_run = schedule::load_last_run(ctx)?;
        let st = self.state();
        let sources: Vec<Value> = source::known(&st)
            .iter()
            .map(|s| {
                let id = s.id();
                let cfg = st.sources.get(id);
                json!({
                    "id": id,
                    "label": cfg.and_then(|c| c.label.clone()),
                    "method": match s.method() { source::Method::Api => "api", source::Method::Scrape => "scrape" },
                    "kind": cfg.and_then(|c| c.kind.clone()),
                    "enabled": cfg.is_some_and(|c| c.enabled),
                    // 0: already validated at load time (§126 review's per-method floor);
                    // this is read-only display, not re-validation, same as `due_sources`.
                    "interval_s": cfg.and_then(|c| c.interval.resolve(0).ok()),
                    "title_contains": cfg.map(|c| c.title_contains.clone()).unwrap_or_default(),
                    "title_excludes": cfg.map(|c| c.title_excludes.clone()).unwrap_or_default(),
                    "last_run_at": last_run.get(id),
                    "running": self.running.is_running(id),
                })
            })
            .collect();
        // A source whose own file couldn't be read is reported here rather than being
        // invisible (or, as before, failing this whole op) -- #126's review, B1.
        let mut problems = problems_of(&st);
        drop(st);
        problems.extend(schedule::last_run_problem(ctx).map(|why| json!({"id": "last_run", "problem": why})));
        Ok(json!({"sources": sources, "problems": problems}))
    }

    /// The whole feature at a glance (ADR 0028 §9): how many sources exist, how many are
    /// enabled, and every problem -- the same "feature-wide summary, not per-source" shape
    /// `automations.status` already has. Per-source detail (what `fetchers.status` used to
    /// answer) is `fetchers.list`/`fetchers.show` now.
    fn status(&self, ctx: &Ctx) -> Result<Value> {
        let st = self.state();
        let enabled = st.sources.values().filter(|c| c.enabled).count();
        let total = st.sources.len();
        let mut problems = problems_of(&st);
        drop(st);
        problems.extend(schedule::last_run_problem(ctx).map(|why| json!({"id": "last_run", "problem": why})));
        Ok(json!({"sources": total, "enabled": enabled, "disabled": total - enabled, "problems": problems}))
    }

    /// Fetches exactly one page from a source and returns its parsed items -- never the
    /// `SeenSet`, `last_run.toml`, or any event (issue #128: "without saving or marking
    /// anything seen"). Deliberately simpler than [`Fetchers::run_source`]: a dry run has no
    /// state of its own to protect, so it skips the overlap guard too -- a `test` can run
    /// alongside a real `fetch` of the same source without conflict.
    async fn test(&self, ctx: &Ctx, source_id: String, filter_override: Option<TitleFilterOverride>) -> Result<Value> {
        if source_id.trim().is_empty() {
            return Err(Error::invalid_params("id must not be empty"));
        }
        let sources = self.state().clone();
        let source = source::resolve(&sources, &source_id)?
            .ok_or_else(|| Error::not_found(format!("no source '{source_id}'")))?;
        let page = source.fetch_page(ctx, None).await?;
        // Applied here too (§13/§14), not just in `run_source` -- a preview that shows items
        // a real run would never keep is a misleading preview.
        let configured = sources.sources.get(&source_id);
        let items: Vec<Value> = page
            .items
            .into_iter()
            .filter_map(RawItem::sanitized)
            .filter(|item| passes_title_filter(&item.title, filter_override.as_ref(), configured))
            .map(|item| item_payload(&source_id, &item))
            .collect();
        Ok(json!({"source": source_id, "items": items}))
    }

    /// Runs one `Source` to completion: pages through it (capped, ADR 0028 §1), dedupes
    /// every item it sees against that source's `SeenSet` (touching both new and
    /// already-seen ids, ADR 0028 §4), and writes the updated set plus one
    /// `fetchers.item.found` per genuinely new item in a single transaction (ADR 0028 §4,
    /// §7.1) -- so a failure partway through this function leaves the previous run's state
    /// exactly as it was, never a half-updated one.
    async fn run_source(
        &self,
        ctx: &Ctx,
        source: &dyn Source,
        filter_override: Option<&TitleFilterOverride>,
        configured: Option<&schedule::SourceConfig>,
    ) -> Result<usize> {
        let source_id = source.id();
        let mut seen = dedup::load(ctx, source_id)?;
        let now = ctx.clock.now();
        let mut found: Vec<RawItem> = Vec::new();
        let mut cursor: Option<String> = None;
        // A title this source's own config excludes, or that `filter_override` excludes for
        // this one call (§13/§14), is marked seen like any other examined item, so it is
        // never re-examined on a later run -- but it is a deliberate exclusion, not a
        // capacity miss, so (unlike the cap below) it is never retried.

        for page_index in 0..source::MAX_PAGES {
            // A full run can issue up to `MAX_PAGES * PAGE_SIZE` HTTP calls (CLAUDE.md §12
            // rule 11: "a task that cannot be cancelled is a bug") -- checked once per page
            // here, and again per-item inside a `Source` whose own page can itself fan out
            // into many calls (e.g. `sources::hn`'s per-kid loop).
            if ctx.cancel.is_cancelled() {
                return Err(Error::new(ErrorCode::ModuleError, "cancelled"));
            }
            ctx.progress(
                page_index as f32 / source::MAX_PAGES as f32,
                &format!("page {}/{}", page_index + 1, source::MAX_PAGES),
            );
            let page = source.fetch_page(ctx, cursor.as_deref()).await?;
            // Fetched and found empty: marked seen so it is never fetched again (B6).
            for id in &page.examined_only {
                seen.touch(id.clone(), now);
            }
            // Untrusted remote input is cleaned (and an item with a non-http(s) url dropped)
            // before dedup, the filter or an event ever sees it -- #126's review, B7/B8.
            let next = page.next_cursor;
            for item in page.items.into_iter().filter_map(RawItem::sanitized) {
                if seen.is_new(&item.source_id) {
                    if !passes_title_filter(&item.title, filter_override, configured) {
                        // A *configured* filter is a standing decision, so a title it drops is
                        // marked seen and never re-examined. A one-off `--contains`/`--excludes`
                        // on `run` is not: marking those seen meant a single experiment hid the
                        // rest of a source's backlog for the full 90-day retention, and a stray
                        // comma hid all of it (#126's review, B2 with B3). An override leaves
                        // the items it drops untouched, so the next plain run still finds them.
                        if filter_override.is_none() {
                            seen.touch(item.source_id.clone(), now);
                        }
                        continue;
                    }
                    if found.len() < dedup::MAX_NEW_PER_RUN {
                        seen.touch(item.source_id.clone(), now);
                        found.push(item);
                    }
                    // else: left unmarked on purpose -- picked up by a later run (§8).
                } else {
                    seen.touch(item.source_id.clone(), now);
                }
            }
            if found.len() >= dedup::MAX_NEW_PER_RUN {
                break;
            }
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        seen.prune(now);
        let new_count = found.len();
        ctx.store.transaction(|tx| {
            tx.put(&dedup::path(source_id), seen.to_toml())?;
            for item in &found {
                tx.emit("fetchers.item.found", item_payload(source_id, item))?;
            }
            Ok(())
        })?;
        Ok(new_count)
    }
}

/// The command set (ADR 0028 §9, ADR 0038 §3/§6): every op that creates, changes, or
/// removes a source's own file, mirroring `automations`' rule commands almost verbatim --
/// the two modules store one definition per named thing the same way, so the same shape of
/// code handles it (CLAUDE.md §8).
impl Fetchers {
    /// A new source (ADR 0028 §9): validated the same way a hand-edited file is, through
    /// `schedule::render` then `SourceConfig::parse` -- one validation path for both a
    /// structured command and `fetchers.reload`. Unlike `automations.new`, there is no
    /// starter/template path yet (ADR 0028 §10 is a later step).
    fn create(&self, ctx: &Ctx, p: SourceInput) -> Result<Value> {
        let _g = self.write.lock();
        if ctx.store.read(&schedule::path(&p.id))?.is_some() {
            return Err(Error::conflict(format!("source '{}' already exists", p.id)));
        }
        self.write_source(ctx, p, "fetchers.source.created")
    }

    /// Replace a source's file text wholesale, after checking it (same rule `automations.edit`
    /// uses): a broken edit is refused and the old file kept. Takes the *whole* file, not a
    /// `SourceInput` built from flags -- `new`'s structured params are fine for a source that
    /// doesn't exist yet, but re-deriving them for `edit` meant re-passing every flag just to
    /// change one, with whatever you forgot to retype silently reset (#305's review,
    /// blocking). The CLI reads the current text first (`fetchers.show`) so this is a
    /// $EDITOR-on-a-copy flow, same as `automations edit`/`workspaces edit`.
    fn edit(&self, ctx: &Ctx, id: &str, text: &str) -> Result<Value> {
        let _g = self.write.lock();
        if ctx.store.read(&schedule::path(id))?.is_none() {
            return Err(Error::not_found(format!("no source '{id}'")));
        }
        let cfg = schedule::SourceConfig::parse(id, text).map_err(Error::invalid_params)?;
        ctx.store.transaction(|tx| {
            tx.put(&schedule::path(id), text)?;
            tx.emit("fetchers.source.edited", json!({"source": id}))
        })?;
        let mut st = self.state();
        st.problems.remove(id);
        st.sources.insert(id.to_owned(), cfg.clone());
        drop(st);
        Ok(view(id, &cfg))
    }

    fn write_source(&self, ctx: &Ctx, p: SourceInput, topic: &str) -> Result<Value> {
        let text = schedule::render(
            p.label.as_deref(),
            p.kind.as_deref(),
            p.enabled,
            &p.interval,
            &p.params,
            &p.title_contains,
            &p.title_excludes,
        )
        .map_err(Error::invalid_params)?;
        let cfg = schedule::SourceConfig::parse(&p.id, &text).map_err(Error::invalid_params)?;
        ctx.store.transaction(|tx| {
            tx.put(&schedule::path(&p.id), text.as_str())?;
            tx.emit(topic, json!({"source": p.id}))
        })?;
        let mut st = self.state();
        st.problems.remove(&p.id);
        st.sources.insert(p.id.clone(), cfg.clone());
        drop(st);
        Ok(view(&p.id, &cfg))
    }

    /// One source in full: its settings, its file text, its last run and whether it's
    /// currently mid-fetch (ADR 0028 §9) -- `fetchers.list`'s own per-source shape, plus the
    /// raw text. Read fresh rather than from the cache for the settings/text, so a broken file
    /// shows the same parse error `new`/`edit` would, never a stale cached copy.
    fn show(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let text =
            ctx.store.read_string(&schedule::path(id))?.ok_or_else(|| Error::not_found(format!("no source '{id}'")))?;
        let cfg = schedule::SourceConfig::parse(id, &text).map_err(Error::invalid_params)?;
        let last_run = schedule::load_last_run(ctx)?;
        let mut v = view(id, &cfg);
        v["last_run_at"] = json!(last_run.get(id));
        v["running"] = json!(self.running.is_running(id));
        v["text"] = json!(text);
        Ok(v)
    }

    /// Move a source's file aside; `restore` or `purge` decide what happens to it next
    /// (ADR 0038 §1).
    fn remove(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let _g = self.write.lock();
        if self.running.is_running(id) {
            return Err(Error::conflict(format!("'{id}' is fetching right now -- wait for it to finish first")));
        }
        let Some(text) = ctx.store.read(&schedule::path(id))? else {
            return Err(Error::not_found(format!("no source '{id}'")));
        };
        ctx.store.transaction(|tx| {
            tx.put(&schedule::removed_path(id), text)?;
            tx.delete(&schedule::path(id))?;
            tx.emit("fetchers.source.removed", json!({"source": id}))
        })?;
        let mut st = self.state();
        st.sources.remove(id);
        st.problems.remove(id);
        Ok(json!({"removed": true, "source": id}))
    }

    /// Delete removed sources for good: `p.id`'s, or every one, after the caller confirms how
    /// many it was shown, as `automations.purge`/`records.purge` ask (ADR 0038 §1).
    fn purge(&self, ctx: &Ctx, p: Purge) -> Result<Value> {
        let _g = self.write.lock();
        let ids = match &p.id {
            Some(id) if ctx.store.read(&schedule::removed_path(id))?.is_none() => {
                return Err(Error::not_found(format!("no removed source '{id}'")));
            }
            Some(id) => vec![id.clone()],
            None => schedule::removed(ctx)?,
        };
        let sources = ids.len();
        if sources == 0 {
            return Ok(json!({"purged": 0}));
        }
        let expected = json!({"sources": sources});
        if p.confirm.as_ref() != Some(&expected) {
            let what = match &p.id {
                Some(id) => format!("the removed source '{id}'"),
                None => format!("all {sources} removed source(s)"),
            };
            return Err(Error::new(
                ErrorCode::ConfirmationRequired,
                format!("purging permanently deletes {what}; confirm with {{\"sources\": {sources}}}"),
            )
            .with_detail(expected));
        }
        let mut event = json!({"sources": sources});
        if let Some(id) = &p.id {
            event["source"] = json!(id);
        }
        // A purged id's dedup state and last-run time go too (#305's review, should-fix) --
        // left behind, a new source reusing that id inherits them: its first run is "due"
        // later than it should be, and whatever it already "saw" under the old source stays
        // hidden forever.
        let last_run = schedule::remove_last_run(ctx, &ids)?;
        ctx.store.transaction(|tx| {
            for id in &ids {
                tx.delete(&schedule::removed_path(id))?;
                tx.delete(&dedup::path(id))?;
            }
            if let Some(last_run) = &last_run {
                tx.put(schedule::LAST_RUN_PATH, last_run.as_str())?;
            }
            tx.emit("fetchers.source.purged", event)
        })?;
        Ok(json!({"purged": sources}))
    }

    /// Bring a removed source back, as it was. One that no longer parses is still restored,
    /// and reported like any broken source file (same rule `automations.restore` uses).
    fn restore(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let _g = self.write.lock();
        let Some(text) = ctx.store.read_string(&schedule::removed_path(id))? else {
            return Err(Error::not_found(format!("no removed source '{id}'")));
        };
        if ctx.store.read(&schedule::path(id))?.is_some() {
            return Err(Error::conflict(format!("a source '{id}' exists: remove or rename it first")));
        }
        ctx.store.transaction(|tx| {
            tx.put(&schedule::path(id), text.as_str())?;
            tx.delete(&schedule::removed_path(id))?;
            tx.emit("fetchers.source.restored", json!({"source": id}))
        })?;
        let parsed = schedule::SourceConfig::parse(id, &text);
        let mut st = self.state();
        match parsed {
            Ok(cfg) => {
                st.sources.insert(id.to_owned(), cfg.clone());
                drop(st);
                Ok(json!({"restored": true, "source": view(id, &cfg), "problem": null}))
            }
            Err(e) => {
                st.problems.insert(id.to_owned(), e.clone());
                Ok(json!({"restored": true, "source": null, "problem": e}))
            }
        }
    }

    /// Switch a source on or off, keeping the rest of its file as written.
    fn set_enabled(&self, ctx: &Ctx, id: &str, on: bool) -> Result<Value> {
        let _g = self.write.lock();
        let Some(text) = ctx.store.read_string(&schedule::path(id))? else {
            return Err(Error::not_found(format!("no source '{id}'")));
        };
        let topic = if on { "fetchers.source.enabled" } else { "fetchers.source.disabled" };
        let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| Error::internal(e.to_string()))?;
        // A hand-edited `source = "x"` is not a table -- index into it and this panics
        // (#305's review, should-fix; the same class #255's review flagged). Refuse it
        // like any other broken file, rather than crash the daemon over one.
        if !doc.get("source").is_some_and(toml_edit::Item::is_table_like) {
            return Err(Error::invalid_params(format!("'{id}': [source] is not a table")));
        }
        doc["source"]["enabled"] = toml_edit::value(on);
        let text = doc.to_string();
        let cfg = schedule::SourceConfig::parse(id, &text).map_err(Error::invalid_params)?;
        ctx.store.transaction(|tx| {
            tx.put(&schedule::path(id), text.as_str())?;
            tx.emit(topic, json!({"source": id}))
        })?;
        self.state().sources.insert(id.to_owned(), cfg.clone());
        Ok(view(id, &cfg))
    }

    /// Give a source a new id (ADR 0038 §6): its dedup state (`<id>/seen.toml`) and
    /// `last_run.toml` entry move with it (ADR 0028 §7), so a renamed source is never
    /// mistaken for brand new on its next run.
    fn rename(&self, ctx: &Ctx, p: MoveParams) -> Result<Value> {
        let _g = self.write.lock();
        if p.id == p.to {
            return Err(Error::invalid_params(format!("'{}' is already its id", p.to)));
        }
        // A run in flight loads `seen` at the start and writes it back, at the end, under
        // the *old* id (`dedup::path`) -- renaming out from under it would make the new id
        // miss this run's items while the old id's dedup state (moved away already) still
        // gets written to, so next run re-reports them as new: duplicate
        // `fetchers.item.found`, duplicate automation messages (#305's review, blocking).
        if self.running.is_running(&p.id) {
            return Err(Error::conflict(format!("'{}' is fetching right now -- wait for it to finish first", p.id)));
        }
        if ctx.store.read(&schedule::path(&p.to))?.is_some() {
            return Err(Error::conflict(format!("a source '{}' exists: pick another id", p.to)));
        }
        let Some(text) = ctx.store.read_string(&schedule::path(&p.id))? else {
            return Err(Error::not_found(format!("no source '{}'", p.id)));
        };
        let renamed = schedule::SourceConfig::parse(&p.to, &text).map_err(Error::invalid_params)?;
        let seen = ctx.store.read(&dedup::path(&p.id))?;
        let last_run = schedule::rename_last_run(ctx, &p.id, &p.to)?;
        ctx.store.transaction(|tx| {
            tx.put(&schedule::path(&p.to), text.as_str())?;
            tx.delete(&schedule::path(&p.id))?;
            if let Some(seen) = &seen {
                tx.put(&dedup::path(&p.to), seen.clone())?;
                tx.delete(&dedup::path(&p.id))?;
            }
            if let Some(last_run) = &last_run {
                tx.put(schedule::LAST_RUN_PATH, last_run.as_str())?;
            }
            tx.emit("fetchers.source.renamed", json!({"from": p.id, "to": p.to}))
        })?;
        let mut st = self.state();
        st.sources.remove(&p.id);
        st.problems.remove(&p.id);
        st.sources.insert(p.to.clone(), renamed.clone());
        drop(st);
        Ok(view(&p.to, &renamed))
    }

    /// A new source from another's file text, switched off so the same schedule doesn't fire
    /// both before you change it (ADR 0038 §6). `label`: the copy's own. Starts with no dedup
    /// or last-run state of its own -- it is a new definition, not a continuation.
    fn copy(&self, ctx: &Ctx, p: CopyParams) -> Result<Value> {
        let _g = self.write.lock();
        if p.id == p.to {
            return Err(Error::invalid_params(format!("'{}' is already its id", p.to)));
        }
        if self.running.is_running(&p.id) {
            return Err(Error::conflict(format!("'{}' is fetching right now -- wait for it to finish first", p.id)));
        }
        if ctx.store.read(&schedule::path(&p.to))?.is_some() {
            return Err(Error::conflict(format!("a source '{}' exists: pick another id", p.to)));
        }
        let Some(text) = ctx.store.read_string(&schedule::path(&p.id))? else {
            return Err(Error::not_found(format!("no source '{}'", p.id)));
        };
        let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| Error::internal(e.to_string()))?;
        if !doc.get("source").is_some_and(toml_edit::Item::is_table_like) {
            return Err(Error::invalid_params(format!("'{}': [source] is not a table", p.id)));
        }
        doc["source"]["enabled"] = toml_edit::value(false);
        if let Some(label) = &p.label {
            doc["source"]["label"] = toml_edit::value(label.as_str());
        }
        let text = doc.to_string();
        let copy = schedule::SourceConfig::parse(&p.to, &text).map_err(Error::invalid_params)?;
        ctx.store.transaction(|tx| {
            tx.put(&schedule::path(&p.to), text.as_str())?;
            tx.emit("fetchers.source.copied", json!({"from": p.id, "to": p.to}))
        })?;
        self.state().sources.insert(p.to.clone(), copy.clone());
        Ok(view(&p.to, &copy))
    }
}

/// A source's wire shape, shared by every op that returns one in full (`new`, `edit`,
/// `enable`/`disable`, `rename`, `copy`, `restore`, `show`) -- one place deciding what a
/// source looks like on the wire, rather than each op building its own `json!` (ADR 0028 §9).
fn view(id: &str, cfg: &schedule::SourceConfig) -> Value {
    json!({
        "id": id,
        "label": cfg.label,
        "enabled": cfg.enabled,
        // 0: already validated at parse time, same as `list`'s own display-only resolve.
        "interval_s": cfg.interval.resolve(0).ok(),
        "kind": cfg.kind,
        "kind_params": cfg.kind_params,
        "title_contains": cfg.title_contains,
        "title_excludes": cfg.title_excludes,
    })
}

/// Classifies a fetch failure for `fetchers.fetch.failed`'s `reason` (ADR 0028 §9, amended by
/// PR #126 review to add `"blocked"`). `ctx.cancel` is checked first since a cancelled run
/// can surface as almost any underlying error, depending on exactly where the cancellation
/// landed. `http_error` relies on `http::require_ok`'s `"http_error:"` message convention;
/// `blocked` relies on `sources::wwr`'s own `"robots.txt disallows fetching..."` message --
/// its own bucket, since the page never got fetched at all, unlike a genuine parse failure.
/// Anything else is `module_error`, treated as `parse_empty` -- a deliberate simplification
/// (see `http::require_ok`'s own doc comment) rather than inventing a precise classifier, or
/// a sixth reason, for every possible failure shape. (A corrupt `seen.toml`, for instance,
/// also falls into this `parse_empty` catch-all -- a known, deliberate tradeoff, see
/// `dedup.rs`'s `a_malformed_seen_file_is_a_reported_error_not_a_silent_empty_set` test; left
/// unchanged.)
fn classify_failure(ctx: &Ctx, err: &Error) -> &'static str {
    if ctx.cancel.is_cancelled() {
        "cancelled"
    } else if err.code == ErrorCode::Unavailable {
        "network"
    } else if err.message.starts_with("http_error:") {
        "http_error"
    } else if err.message.contains("robots.txt") {
        "blocked"
    } else {
        "parse_empty"
    }
}

/// Whether an item's title should be kept: `filter_override` (ADR 0028 §14), when given,
/// completely replaces `configured`'s own `title_contains`/`title_excludes` for this one
/// call -- the two are never merged. With neither present, everything passes, exactly as
/// before either of them existed.
fn passes_title_filter(
    title: &str,
    filter_override: Option<&TitleFilterOverride>,
    configured: Option<&schedule::SourceConfig>,
) -> bool {
    match filter_override {
        Some(f) => schedule::matches_title(title, &f.contains, &f.excludes),
        None => configured.is_none_or(|c| c.matches_title(title)),
    }
}

/// Sources whose own file could not be read, as `[{id, problem}]` (#126's review, B1). Empty
/// in the normal case, so a client can show it only when there is something wrong.
fn problems_of(state: &Sources) -> Vec<Value> {
    state.problems.iter().map(|(id, why)| json!({"id": id, "problem": why})).collect()
}

/// `fetchers.item.found`'s payload (ADR 0028 §3, §9).
fn item_payload(source_id: &str, item: &RawItem) -> Value {
    json!({
        "source_name": source_id,
        "source_id": item.source_id,
        "title": item.title,
        "url": item.url,
        "raw_data": cap_raw_data(&item.raw_data),
    })
}

/// Caps an item's `raw_data` (ADR 0028 §3) **without changing its shape** (#126's review,
/// B12). Replacing an over-long object with `{"truncated": true, "preview": "<json text>"}`
/// meant an automation reading `{raw_data.company}` broke on exactly the long postings and
/// worked on every short one -- the worst possible failure distribution. Now each oversized
/// *string field* is truncated in place, keys and nesting stay put, and a field that was cut
/// says so.
fn cap_raw_data(v: &Value) -> Value {
    if v.to_string().chars().count() <= MAX_RAW_DATA_CHARS {
        return v.clone();
    }
    match v {
        Value::Object(fields) => {
            // Budget shared evenly across the fields, so one enormous description can't
            // starve the small, structured fields an automation is most likely to read.
            let budget = (MAX_RAW_DATA_CHARS / fields.len().max(1)).max(MIN_FIELD_CHARS);
            Value::Object(fields.iter().map(|(k, v)| (k.clone(), cap_value(v, budget))).collect())
        }
        other => cap_value(other, MAX_RAW_DATA_CHARS),
    }
}

/// One `raw_data` value, truncated to `budget` characters if it is a long string, recursed
/// into if it is an array or object, and left alone if it is a number/bool/null.
fn cap_value(v: &Value, budget: usize) -> Value {
    match v {
        Value::String(s) if s.chars().count() > budget => {
            json!(format!("{}… (truncated)", s.chars().take(budget).collect::<String>()))
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| cap_value(i, budget)).collect()),
        Value::Object(fields) => Value::Object(fields.iter().map(|(k, v)| (k.clone(), cap_value(v, budget))).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use shimmer_core::testing::TestEnv;
    use shimmer_core::HttpResponse;

    use super::*;
    use crate::schedule::test_support::{source_file, write_source};
    use crate::schedule::IntervalSpec;

    #[test]
    fn manifest_declares_its_topics_and_the_network_capability() {
        let m = Fetchers::default().manifest();
        assert_eq!(m.id.as_str(), "fetchers");
        assert_eq!(m.namespace, "fetchers");
        assert_eq!(
            m.topics,
            vec![
                "fetchers.item.found".to_string(),
                "fetchers.fetch.finished".to_string(),
                "fetchers.fetch.failed".to_string(),
                "fetchers.source.created".to_string(),
                "fetchers.source.edited".to_string(),
                "fetchers.source.removed".to_string(),
                "fetchers.source.restored".to_string(),
                "fetchers.source.purged".to_string(),
                "fetchers.source.renamed".to_string(),
                "fetchers.source.copied".to_string(),
                "fetchers.source.enabled".to_string(),
                "fetchers.source.disabled".to_string(),
            ]
        );
        assert_eq!(m.capabilities, vec!["network".to_string()]);
    }

    #[test]
    fn declares_the_fetchers_lane_at_the_documented_concurrency() {
        let lanes = Fetchers::default().lanes();
        assert_eq!(lanes, vec![LaneConfig::new(LANE, 8)]);
    }

    #[test]
    fn declares_exactly_the_heartbeat_trigger() {
        let triggers = Fetchers::default().triggers();
        assert_eq!(triggers, vec![schedule::trigger()]);
    }

    #[tokio::test]
    async fn init_succeeds_with_nothing_registered() {
        let env = TestEnv::new("fetchers");
        Fetchers::default().init(&env.ctx).await.unwrap();
    }

    #[tokio::test]
    async fn init_loads_every_source_file_so_list_sees_them_with_no_reload_needed() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily"}));
        let fetchers = Fetchers::default();
        fetchers.init(&env.ctx).await.unwrap();

        let out = fetchers.list(&env.ctx, ListParams::default()).await.unwrap();
        let sources = out["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0]["id"], "hn-whoishiring");
    }

    #[tokio::test]
    async fn reload_picks_up_a_file_written_after_init_and_reports_a_broken_one() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        fetchers.init(&env.ctx).await.unwrap();
        assert!(fetchers.list(&env.ctx, ListParams::default()).await.unwrap()["sources"]
            .as_array()
            .unwrap()
            .is_empty());

        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily"}));
        env.ctx.store.write(&format!("{}/broken.toml", schedule::SOURCES_DIR), "this is not [[[ valid toml").unwrap();

        let out = fetchers.handle(RELOAD_OP, Value::Null, &env.ctx).await.unwrap();
        assert_eq!(out["sources"], 1);
        assert_eq!(out["problems"].as_array().unwrap().len(), 1);

        let listed = fetchers.list(&env.ctx, ListParams::default()).await.unwrap();
        assert_eq!(listed["sources"].as_array().unwrap().len(), 1);
        assert_eq!(listed["problems"].as_array().unwrap().len(), 1, "{listed:?}");
    }

    #[tokio::test]
    async fn an_unknown_op_is_rejected_rather_than_silently_accepted() {
        let env = TestEnv::new("fetchers");
        let err = Fetchers::default().handle("fetchers.bogus", json!({}), &env.ctx).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::UnknownOp);
    }

    #[test]
    fn capping_raw_data_keeps_its_shape_so_an_automation_can_still_read_a_field() {
        // #126's review, B12: over 8 KiB, raw_data became {"truncated": true, "preview": "..."},
        // so an automation reading {raw_data.company} broke on exactly the long postings and
        // worked on every short one.
        let huge = "x".repeat(MAX_RAW_DATA_CHARS * 2);
        let capped = cap_raw_data(&json!({"company": "Acme", "text": huge, "remote": true, "salary": 120}));

        assert_eq!(capped["company"], "Acme", "a short field is untouched");
        assert_eq!(capped["remote"], true, "and so are non-strings");
        assert_eq!(capped["salary"], 120);
        assert!(capped["text"].as_str().unwrap().ends_with("(truncated)"), "the long one says it was cut");
        assert!(capped.get("truncated").is_none(), "the old shape-replacing marker is gone");
        assert!(capped.to_string().chars().count() < MAX_RAW_DATA_CHARS * 2, "and it actually got smaller");
    }

    #[test]
    fn capping_raw_data_leaves_anything_already_small_enough_exactly_as_it_was() {
        let small = json!({"company": "Acme", "nested": {"a": [1, "two"]}});
        assert_eq!(cap_raw_data(&small), small);
    }

    #[test]
    fn title_filter_override_is_none_only_when_neither_flag_was_given() {
        assert!(TitleFilterOverride::from_params(None, None).unwrap().is_none());
        let f = TitleFilterOverride::from_params(Some(vec!["junior".into()]), None).unwrap().unwrap();
        assert_eq!((f.contains, f.excludes), (vec!["junior".to_string()], Vec::new()));
        let f = TitleFilterOverride::from_params(None, Some(vec!["senior".into()])).unwrap().unwrap();
        assert_eq!((f.contains, f.excludes), (Vec::new(), vec!["senior".to_string()]));
    }

    #[test]
    fn an_empty_title_filter_term_from_a_caller_is_rejected_not_just_defanged() {
        // B2 already stops an override from marking a dropped item seen, so this was only ever
        // wrong, not destructive -- but `fetchers.run`/`fetchers.test` reach `from_params`
        // straight from an op's params, past the CLI's own comma-split filtering (#59)
        // and config load's own rejection (B3), so an automation or `shimmer call` sending
        // `{"title_excludes": ["senior", ""]}` must hit the same rejection here (#279's re-review).
        match TitleFilterOverride::from_params(None, Some(vec!["senior".into(), String::new()])) {
            Ok(_) => panic!("an empty title_excludes term must be rejected"),
            Err(e) => {
                assert_eq!(e.code, shimmer_core::ErrorCode::InvalidParams);
                assert!(e.message.contains("title_excludes"), "{}", e.message);
            }
        }
        match TitleFilterOverride::from_params(Some(vec!["  ".into()]), None) {
            Ok(_) => panic!("a whitespace-only title_contains term must be rejected"),
            Err(e) => assert!(e.message.contains("title_contains"), "{}", e.message),
        }
    }

    fn source_config(enabled: bool, interval_s: u64) -> schedule::SourceConfig {
        schedule::SourceConfig {
            label: None,
            enabled,
            interval: IntervalSpec::Seconds(interval_s),
            kind: None,
            title_contains: Vec::new(),
            title_excludes: Vec::new(),
            kind_params: serde_json::Map::new(),
        }
    }

    #[tokio::test]
    async fn tick_enqueues_every_due_source_and_only_those() {
        let env = TestEnv::new("fetchers");
        let mut sources = BTreeMap::new();
        sources.insert("hn-whoishiring".into(), source_config(true, schedule::MIN_INTERVAL_API_S));
        sources.insert("weworkremotely".into(), source_config(false, schedule::MIN_INTERVAL_API_S));

        let fetchers = Fetchers::default();
        let result = fetchers.run_due(&env.ctx, &sources).await.unwrap();
        assert_eq!(result["enqueued"], json!(["hn-whoishiring"]));
        assert_eq!(result["already_running"], json!([]));

        let submitted = env.queue.requests.lock().unwrap();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].op, RUN_TASK_OP);
        assert_eq!(submitted[0].params, json!({"id": "hn-whoishiring"}));
    }

    #[tokio::test]
    async fn tick_skips_a_source_it_already_sees_running() {
        let env = TestEnv::new("fetchers");
        let mut sources = BTreeMap::new();
        sources.insert("hn-whoishiring".into(), source_config(true, schedule::MIN_INTERVAL_API_S));

        let fetchers = Fetchers::default();
        let _permit = fetchers.running.try_acquire("hn-whoishiring").unwrap();
        let result = fetchers.run_due(&env.ctx, &sources).await.unwrap();

        assert_eq!(result["enqueued"], json!([]));
        assert_eq!(result["already_running"], json!(["hn-whoishiring"]));
        assert!(env.queue.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_source_already_fetching_is_refused_not_double_run() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let _permit = fetchers.running.try_acquire("hn-whoishiring").unwrap();

        let err = fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        // Refused before doing anything -- no run recorded.
        assert!(schedule::load_last_run(&env.ctx).unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_source_is_rejected() {
        let env = TestEnv::new("fetchers");
        let err = Fetchers::default().fetch(&env.ctx, "  ".into(), None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn an_unregistered_source_is_not_found() {
        let env = TestEnv::new("fetchers");
        let err = Fetchers::default().fetch(&env.ctx, "not-a-real-source".into(), None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        // Not even an attempt -- no permit taken, no run recorded.
        assert!(schedule::load_last_run(&env.ctx).unwrap().is_empty());
    }

    fn json_response(body: Value) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: body.to_string().into_bytes(),
            from_cache: false,
            stale: false,
            retry_after_secs: None,
        }
    }

    /// Stubs just enough of the real `hn-whoishiring` endpoints (StubHttp matches by exact
    /// URL, and `source::find` always builds `HnWhoIsHiring::default()`, which points at the
    /// real `hacker-news.firebaseio.com`) for one thread with one valid comment -- module.rs's
    /// own tests are about the dedup/event/record-keeping orchestration around a `Source`,
    /// not HN's own API shape, which `sources::hn`'s tests already cover against a real
    /// local server.
    fn stub_one_hn_comment(env: &TestEnv, comment_id: u64, text: &str) {
        const BASE: &str = "https://hacker-news.firebaseio.com/v0";
        let mut responses = env.http.responses.lock().unwrap();
        responses.insert(format!("{BASE}/user/whoishiring.json"), json_response(json!({"submitted": [1]})));
        responses.insert(
            format!("{BASE}/item/1.json"),
            // `title` must match `sources::hn::latest_thread`'s "who is hiring" check (PR #126
            // review) or this stub's thread is never picked, and every test using it fails.
            json_response(json!({"id": 1, "title": "Ask HN: Who is hiring? (October 2026)", "kids": [comment_id]})),
        );
        responses.insert(
            format!("{BASE}/item/{comment_id}.json"),
            json_response(json!({"id": comment_id, "by": "acme", "text": text})),
        );
    }

    #[tokio::test]
    async fn a_successful_fetch_emits_item_found_and_dedupes_on_the_next_run() {
        let env = TestEnv::new("fetchers");
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring Rust engineers.");

        let fetchers = Fetchers::default();
        let out = fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();
        assert_eq!((out["source"].as_str(), out["new_items"].as_u64()), (Some("hn-whoishiring"), Some(1)));

        let events = env.backend.events();
        let found: Vec<_> = events.iter().filter(|e| e.topic == "fetchers.item.found").collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].payload["source_name"], "hn-whoishiring");
        assert_eq!(found[0].payload["source_id"], "10");
        assert!(events.iter().any(|e| e.topic == "fetchers.fetch.finished"));

        let last_run = schedule::load_last_run(&env.ctx).unwrap();
        assert_eq!(last_run.get("hn-whoishiring"), Some(&env.ctx.clock.now()));
        assert!(!fetchers.running.is_running("hn-whoishiring"), "the permit must be released on success");

        // Re-running with the identical comment still present must not re-emit it.
        let second = fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();
        assert_eq!(second["new_items"].as_u64(), Some(0));
        let found_after_second = env.backend.events().iter().filter(|e| e.topic == "fetchers.item.found").count();
        assert_eq!(found_after_second, 1, "an already-seen item must never be emitted twice");
    }

    #[tokio::test]
    async fn run_source_drops_an_excluded_title_and_never_retries_it() {
        let env = TestEnv::new("fetchers");
        write_source(
            &env,
            "hn-whoishiring",
            json!({"enabled": true, "interval": "daily", "title_excludes": ["senior"]}),
        );
        const BASE: &str = "https://hacker-news.firebaseio.com/v0";
        {
            let mut responses = env.http.responses.lock().unwrap();
            responses.insert(format!("{BASE}/user/whoishiring.json"), json_response(json!({"submitted": [1]})));
            responses.insert(
                format!("{BASE}/item/1.json"),
                json_response(json!({"id": 1, "title": "Ask HN: Who is hiring? (October 2026)", "kids": [10, 11]})),
            );
            responses.insert(
                format!("{BASE}/item/10.json"),
                json_response(json!({"id": 10, "by": "acme", "text": "<p>Senior Rust Engineer<p>5+ years."})),
            );
            responses.insert(
                format!("{BASE}/item/11.json"),
                json_response(json!({"id": 11, "by": "acme", "text": "<p>Junior Rust Engineer<p>New grad welcome."})),
            );
        }

        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        let out = fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();
        assert_eq!(out["new_items"], 1, "only the junior posting should count as new");

        let found: Vec<_> = env.backend.events().into_iter().filter(|e| e.topic == "fetchers.item.found").collect();
        assert_eq!(found.len(), 1);
        assert!(found[0].payload["title"].as_str().unwrap().contains("Junior"), "{:?}", found[0].payload);

        // The excluded item is marked seen -- never re-examined -- same as a kept one, not
        // left unmarked the way a cap-overflow miss would be.
        let seen = dedup::load(&env.ctx, "hn-whoishiring").unwrap();
        assert!(!seen.is_new("10"), "an excluded title must still be marked seen");
        assert!(!seen.is_new("11"));

        // Re-running finds nothing new: the excluded item was never queued for a retry.
        let second = fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();
        assert_eq!(second["new_items"], 0);
    }

    #[tokio::test]
    async fn test_applies_the_same_title_filter_a_real_run_would() {
        let env = TestEnv::new("fetchers");
        write_source(
            &env,
            "hn-whoishiring",
            json!({"enabled": true, "interval": "daily", "title_contains": ["junior"]}),
        );
        const BASE: &str = "https://hacker-news.firebaseio.com/v0";
        {
            let mut responses = env.http.responses.lock().unwrap();
            responses.insert(format!("{BASE}/user/whoishiring.json"), json_response(json!({"submitted": [1]})));
            responses.insert(
                format!("{BASE}/item/1.json"),
                json_response(json!({"id": 1, "title": "Ask HN: Who is hiring? (October 2026)", "kids": [10, 11]})),
            );
            responses.insert(
                format!("{BASE}/item/10.json"),
                json_response(json!({"id": 10, "by": "acme", "text": "<p>Senior Rust Engineer"})),
            );
            responses.insert(
                format!("{BASE}/item/11.json"),
                json_response(json!({"id": 11, "by": "acme", "text": "<p>Junior Rust Engineer"})),
            );
        }

        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        let out = fetchers.test(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();
        let items = out["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(items[0]["title"].as_str().unwrap().contains("Junior"));
    }

    #[tokio::test]
    async fn an_override_filter_replaces_rather_than_merges_with_the_configured_one() {
        let env = TestEnv::new("fetchers");
        // Configured to want only "junior" titles -- the override below asks for the
        // opposite (anything but "junior") and must win outright, not combine with this.
        write_source(
            &env,
            "hn-whoishiring",
            json!({"enabled": true, "interval": "daily", "title_contains": ["junior"]}),
        );
        const BASE: &str = "https://hacker-news.firebaseio.com/v0";
        {
            let mut responses = env.http.responses.lock().unwrap();
            responses.insert(format!("{BASE}/user/whoishiring.json"), json_response(json!({"submitted": [1]})));
            responses.insert(
                format!("{BASE}/item/1.json"),
                json_response(json!({"id": 1, "title": "Ask HN: Who is hiring? (October 2026)", "kids": [10, 11]})),
            );
            responses.insert(
                format!("{BASE}/item/10.json"),
                json_response(json!({"id": 10, "by": "acme", "text": "<p>Senior Rust Engineer"})),
            );
            responses.insert(
                format!("{BASE}/item/11.json"),
                json_response(json!({"id": 11, "by": "acme", "text": "<p>Junior Rust Engineer"})),
            );
        }

        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        let override_filter = TitleFilterOverride { contains: vec!["senior".into()], excludes: Vec::new() };
        let out = fetchers.test(&env.ctx, "hn-whoishiring".into(), Some(override_filter)).await.unwrap();
        let items = out["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(items[0]["title"].as_str().unwrap().contains("Senior"), "the override must win, not merge with config");
    }

    #[tokio::test]
    async fn an_override_filter_on_a_real_run_leaves_the_items_it_drops_findable_later() {
        let env = TestEnv::new("fetchers"); // no configured filter at all -- the override is the only one
        const BASE: &str = "https://hacker-news.firebaseio.com/v0";
        {
            let mut responses = env.http.responses.lock().unwrap();
            responses.insert(format!("{BASE}/user/whoishiring.json"), json_response(json!({"submitted": [1]})));
            responses.insert(
                format!("{BASE}/item/1.json"),
                json_response(json!({"id": 1, "title": "Ask HN: Who is hiring? (October 2026)", "kids": [10, 11]})),
            );
            responses.insert(
                format!("{BASE}/item/10.json"),
                json_response(json!({"id": 10, "by": "acme", "text": "<p>Senior Rust Engineer"})),
            );
            responses.insert(
                format!("{BASE}/item/11.json"),
                json_response(json!({"id": 11, "by": "acme", "text": "<p>Junior Rust Engineer"})),
            );
        }

        let override_filter = TitleFilterOverride { contains: Vec::new(), excludes: vec!["senior".into()] };
        let fetchers = Fetchers::default();
        let out = fetchers.fetch(&env.ctx, "hn-whoishiring".into(), Some(override_filter)).await.unwrap();
        assert_eq!(out["new_items"], 1);

        // This test used to assert the opposite -- that an override marked what it dropped as
        // seen, the same as a configured filter. #126's review showed why that's wrong: one
        // `--excludes` experiment then hid the rest of a source's backlog for the full 90-day
        // retention, and a stray trailing comma hid all of it. An ad-hoc override is an
        // experiment, so what it drops stays findable (B2).
        let seen = dedup::load(&env.ctx, "hn-whoishiring").unwrap();
        assert!(seen.is_new("10"), "an ad-hoc override must not permanently hide what it drops");

        // Proof it is genuinely still reachable: the same run with no override finds it.
        let second = fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();
        assert_eq!(second["new_items"], 1, "the item the override dropped is found by a plain run");
    }

    #[tokio::test]
    async fn a_configured_rss_kind_fetches_through_the_same_pipeline_as_a_fixed_source() {
        const FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Blog</title>
  <id>urn:uuid:feed-1</id>
  <updated>2026-10-01T00:00:00Z</updated>
  <entry>
    <id>urn:uuid:entry-1</id>
    <title>First post</title>
    <link href="https://example.test/posts/1"/>
    <updated>2026-10-01T00:00:00Z</updated>
  </entry>
</feed>"#;

        let env = TestEnv::new("fetchers");
        write_source(
            &env,
            "my-team-blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"}),
        );
        env.http.responses.lock().unwrap().insert(
            "https://example.test/feed.xml".to_string(),
            HttpResponse {
                status: 200,
                body: FEED.as_bytes().to_vec(),
                from_cache: false,
                stale: false,
                retry_after_secs: None,
            },
        );

        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        let out = fetchers.fetch(&env.ctx, "my-team-blog".into(), None).await.unwrap();
        assert_eq!((out["source"].as_str(), out["new_items"].as_u64()), (Some("my-team-blog"), Some(1)));

        let found: Vec<_> = env.backend.events().into_iter().filter(|e| e.topic == "fetchers.item.found").collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].payload["source_name"], "my-team-blog");
        assert_eq!(found[0].payload["source_id"], "urn:uuid:entry-1");
    }

    #[tokio::test]
    async fn an_id_that_is_neither_a_fixed_source_nor_a_configured_kind_is_not_found() {
        let env = TestEnv::new("fetchers");
        let err = Fetchers::default().fetch(&env.ctx, "not-a-real-source".into(), None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn list_shows_only_an_opted_into_fixed_source_and_a_configured_kind() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily"}));
        write_source(
            &env,
            "my-team-blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"}),
        );

        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        let out = fetchers.list(&env.ctx, ListParams::default()).await.unwrap();
        let sources = out["sources"].as_array().unwrap();
        // weworkremotely is compiled in but never configured here, so a user with no
        // interest in job hunting never sees it (ADR 0028 §19).
        assert_eq!(sources.len(), 2, "{sources:?}");
        assert!(sources.iter().all(|s| s["id"] != "weworkremotely"), "{sources:?}");

        let hn = sources.iter().find(|s| s["id"] == "hn-whoishiring").unwrap();
        assert_eq!(
            (hn["method"].as_str(), hn["enabled"].as_bool(), hn["kind"].as_str()),
            (Some("api"), Some(true), None)
        );
        assert_eq!(hn["interval_s"], 86_400);

        let blog = sources.iter().find(|s| s["id"] == "my-team-blog").unwrap();
        assert_eq!(
            (blog["method"].as_str(), blog["enabled"].as_bool(), blog["kind"].as_str()),
            (Some("api"), Some(true), Some("rss"))
        );
    }

    #[tokio::test]
    async fn list_reports_last_run_and_whether_a_source_is_mid_fetch() {
        // ADR 0028 §9 merges the prior design's separate per-source `list`/`status` into one.
        let env = TestEnv::new("fetchers");
        // Both must have their own entry to show up in list at all (ADR 0028 §19) --
        // fetching hn-whoishiring works regardless (resolve() needs none), but seeing it
        // and weworkremotely listed afterward does not.
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily"}));
        write_source(&env, "weworkremotely", json!({"enabled": false, "interval": schedule::MIN_INTERVAL_SCRAPE_S}));
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring.");
        fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();

        let out = fetchers.list(&env.ctx, ListParams::default()).await.unwrap();
        let sources = out["sources"].as_array().unwrap();
        let hn = sources.iter().find(|s| s["id"] == "hn-whoishiring").unwrap();
        assert_eq!(hn["last_run_at"], json!(env.ctx.clock.now()));
        assert_eq!(hn["running"], false);

        let wwr = sources.iter().find(|s| s["id"] == "weworkremotely").unwrap();
        assert!(wwr["last_run_at"].is_null(), "never fetched -- no last run");
    }

    #[test]
    fn status_is_a_feature_wide_summary_not_per_source() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily"}));
        write_source(&env, "weworkremotely", json!({"enabled": false, "interval": schedule::MIN_INTERVAL_SCRAPE_S}));
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();

        let out = fetchers.status(&env.ctx).unwrap();
        assert_eq!(out, json!({"sources": 2, "enabled": 1, "disabled": 1, "problems": []}));
    }

    #[tokio::test]
    async fn test_shows_parsed_items_without_touching_dedup_last_run_or_events() {
        let env = TestEnv::new("fetchers");
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring Rust engineers.");

        let out = Fetchers::default().test(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();
        assert_eq!(out["source"], "hn-whoishiring");
        let items = out["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_id"], "10");

        assert!(schedule::load_last_run(&env.ctx).unwrap().is_empty(), "test must never record a run");
        assert!(env.backend.events().iter().all(|e| e.topic != "fetchers.item.found"), "test must never emit");
        let seen = dedup::load(&env.ctx, "hn-whoishiring").unwrap();
        assert!(seen.is_new("10"), "test must never touch the seen set");
    }

    #[tokio::test]
    async fn test_does_not_conflict_with_a_concurrent_real_fetch_of_the_same_source() {
        let env = TestEnv::new("fetchers");
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring.");
        let fetchers = Fetchers::default();
        let _permit = fetchers.running.try_acquire("hn-whoishiring").unwrap();

        // A real fetch would be refused right now (see
        // `a_source_already_fetching_is_refused_not_double_run`); `test` must not be, since
        // it never touches the state the overlap guard protects.
        let out = fetchers.test(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();
        assert_eq!(out["items"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_transport_failure_still_records_the_run_and_emits_fetch_failed() {
        // No responses stubbed at all -- every ctx.http.get call fails with `unavailable`.
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();

        let err = fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);

        let last_run = schedule::load_last_run(&env.ctx).unwrap();
        assert_eq!(
            last_run.get("hn-whoishiring"),
            Some(&env.ctx.clock.now()),
            "a broken source must still be recorded as run, so it waits a full interval rather than being hammered every tick"
        );
        let failed: Vec<_> = env.backend.events().into_iter().filter(|e| e.topic == "fetchers.fetch.failed").collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].payload["reason"], "network");
    }

    #[tokio::test]
    async fn handle_routes_both_ops_by_name() {
        let env = TestEnv::new("fetchers");
        stub_one_hn_comment(&env, 20, "<p>Widgets Inc<p>Hiring.");
        let fetchers = Fetchers::default();

        let tick = fetchers.handle(TICK_OP, Value::Null, &env.ctx).await.unwrap();
        assert_eq!(tick["enqueued"], json!([]), "no source files at all by default");

        let run = fetchers.handle(RUN_OP, json!({"id": "hn-whoishiring"}), &env.ctx).await.unwrap();
        assert_eq!(run["queued"], true);
        assert!(run["task_id"].is_string(), "{run:?}");

        let fetch = fetchers.handle(RUN_TASK_OP, json!({"id": "hn-whoishiring"}), &env.ctx).await.unwrap();
        assert_eq!(fetch["source"], "hn-whoishiring");
        assert_eq!(fetch["new_items"], 1);
    }

    #[tokio::test]
    async fn run_refuses_an_unknown_source_instead_of_acking_queued_and_failing_silently() {
        // Issue #277's own end-to-end sweep: `fetchers run nope` used to say "queued", then
        // the task failed at once with `not_found` -- never shown to whoever ran it.
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let err = fetchers.handle(RUN_OP, json!({"id": "nope"}), &env.ctx).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(env.queue.requests.lock().unwrap().is_empty(), "nothing should have been queued at all");
    }

    #[tokio::test]
    async fn run_enqueues_run_task_with_the_same_params_it_was_given() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let out = fetchers
            .handle(RUN_OP, json!({"id": "hn-whoishiring", "title_contains": ["junior"]}), &env.ctx)
            .await
            .unwrap();
        assert_eq!(out["queued"], true);

        let submitted = env.queue.requests.lock().unwrap();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].op, RUN_TASK_OP);
        assert_eq!(submitted[0].params, json!({"id": "hn-whoishiring", "title_contains": ["junior"]}));
    }

    #[tokio::test]
    async fn run_source_bails_immediately_once_cancelled_rather_than_paging_on() {
        let env = TestEnv::new("fetchers");
        // Deliberately no stubs at all: if the cancellation check did not run before the
        // first page fetch, this would instead fail with "no stub for...", not the
        // `cancelled` classification this test is actually about.
        env.ctx.cancel.cancel();
        let fetchers = Fetchers::default();

        let source = source::find("hn-whoishiring").unwrap();
        let err = fetchers.run_source(&env.ctx, source.as_ref(), None, None).await.unwrap_err();
        assert!(env.ctx.cancel.is_cancelled());
        assert_eq!(classify_failure(&env.ctx, &err), "cancelled");
    }

    #[tokio::test]
    async fn a_cancelled_fetch_is_classified_and_emitted_as_cancelled_not_whatever_error_surfaced() {
        let env = TestEnv::new("fetchers");
        env.ctx.cancel.cancel();
        let fetchers = Fetchers::default();

        let err = fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap_err();
        assert_eq!(classify_failure(&env.ctx, &err), "cancelled");
        let failed: Vec<_> = env.backend.events().into_iter().filter(|e| e.topic == "fetchers.fetch.failed").collect();
        assert_eq!(failed[0].payload["reason"], "cancelled");
    }

    #[test]
    fn a_robots_refusal_is_classified_as_blocked_not_the_parse_empty_catch_all() {
        let env = TestEnv::new("fetchers");
        let err = Error::module_error("robots.txt disallows fetching https://example.test/categories/x".to_owned());
        assert_eq!(classify_failure(&env.ctx, &err), "blocked");
    }

    #[test]
    fn an_ordinary_module_error_still_falls_back_to_parse_empty() {
        let env = TestEnv::new("fetchers");
        let err = Error::module_error("some other failure shape entirely".to_owned());
        assert_eq!(classify_failure(&env.ctx, &err), "parse_empty");
    }

    #[tokio::test]
    async fn run_source_reports_progress_once_per_page() {
        let env = TestEnv::new("fetchers");
        stub_one_hn_comment(&env, 30, "<p>Globex<p>Hiring.");
        let progress = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let p = progress.clone();
        let ctx = env.ctx.for_task(
            shimmer_core::TaskId::new(),
            env.ctx.cancel.clone(),
            std::sync::Arc::new(move |f: f32, n: &str| p.lock().unwrap().push((f, n.to_owned()))),
        );
        let fetchers = Fetchers::default();

        let source = source::find("hn-whoishiring").unwrap();
        fetchers.run_source(&ctx, source.as_ref(), None, None).await.unwrap();

        let reported = progress.lock().unwrap();
        assert!(!reported.is_empty(), "at least the first page must report progress");
        assert!(reported[0].1.contains("page 1/"), "{:?}", reported[0]);
    }

    #[tokio::test]
    async fn two_sources_finishing_their_run_record_do_not_clobber_each_others_last_run_entry() {
        // Not a true concurrency test (see PR #126 review reply) -- this proves the lock is
        // correctly scoped to `record_run` alone: two sequential holders must each still see
        // the other's write, which a lock held across the *wrong* span (or none at all, if the
        // two calls raced) could still get right by accident, but a lock held and released
        // incorrectly inside `fetch` could not.
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        {
            let _g = fetchers.write.lock();
            schedule::record_run(&env.ctx, "hn-whoishiring", env.ctx.clock.now()).unwrap();
        }
        {
            let _g = fetchers.write.lock();
            schedule::record_run(&env.ctx, "weworkremotely", env.ctx.clock.now()).unwrap();
        }
        let all = schedule::load_last_run(&env.ctx).unwrap();
        assert!(all.contains_key("hn-whoishiring") && all.contains_key("weworkremotely"));
    }

    fn rss_text(url: &str) -> String {
        source_file(&json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": url}))
    }

    /// `fetchers.new`/`.edit`'s structured params (ADR 0028 §9), an `rss` source enabled on an
    /// hourly interval -- the shape every create/edit test below starts from.
    fn rss_input(id: &str, url: &str) -> SourceInput {
        SourceInput {
            id: id.into(),
            label: None,
            kind: Some("rss".into()),
            enabled: true,
            interval: schedule::IntervalSpec::Preset("hourly".into()),
            params: json!({"url": url}).as_object().unwrap().clone(),
            title_contains: Vec::new(),
            title_excludes: Vec::new(),
        }
    }

    #[test]
    fn create_makes_a_new_source_and_refuses_a_duplicate_id() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let v = fetchers.create(&env.ctx, rss_input("blog", "https://example.test/f.xml")).unwrap();
        assert_eq!(v["id"], "blog");
        assert_eq!(v["kind"], "rss");
        assert!(fetchers.state().sources.contains_key("blog"));
        let saved = env.ctx.store.read_string(&schedule::path("blog")).unwrap().unwrap();
        let cfg = schedule::SourceConfig::parse("blog", &saved).unwrap();
        assert_eq!(cfg.kind_params.get("url").and_then(Value::as_str), Some("https://example.test/f.xml"));

        let err = fetchers.create(&env.ctx, rss_input("blog", "https://example.test/f.xml")).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::Conflict);
    }

    #[test]
    fn create_rejects_invalid_params_and_leaves_nothing_behind() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let mut bad = rss_input("blog", "https://example.test/f.xml");
        bad.params = Map::new(); // rss needs a url
        let err = fetchers.create(&env.ctx, bad).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);
        assert!(env.ctx.store.read(&schedule::path("blog")).unwrap().is_none());
    }

    #[test]
    fn edit_replaces_an_existing_source_and_refuses_an_unknown_id() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": false, "interval": "hourly", "kind": "rss", "url": "https://a.test/f.xml"}),
        );
        fetchers.load(&env.ctx).unwrap();

        let v = fetchers.edit(&env.ctx, "blog", &rss_text("https://b.test/f.xml")).unwrap();
        assert_eq!(v["kind_params"]["url"], "https://b.test/f.xml");
        assert!(fetchers.state().sources.get("blog").unwrap().enabled);

        let err = fetchers.edit(&env.ctx, "nope", &rss_text("https://c.test/f.xml")).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::NotFound);
    }

    #[test]
    fn edit_rejects_a_broken_replacement_and_keeps_the_old_file() {
        // #305's review, blocking: a full replace must mean the caller sends the *whole*
        // file (so nothing not retyped is silently reset), not a structured `SourceInput`
        // the CLI would have to reassemble from flags, forgetting whatever it didn't
        // re-pass.
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let original = rss_text("https://a.test/f.xml");
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://a.test/f.xml"}),
        );
        fetchers.load(&env.ctx).unwrap();

        let err = fetchers.edit(&env.ctx, "blog", "not [[[ valid toml").unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);
        assert_eq!(
            env.ctx.store.read_string(&schedule::path("blog")).unwrap().unwrap(),
            original,
            "the old file must survive a rejected edit"
        );
    }

    #[test]
    fn show_returns_the_source_with_its_file_text_and_not_found_for_an_unknown_id() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let text = rss_text("https://example.test/f.xml");
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/f.xml"}),
        );

        let v = fetchers.show(&env.ctx, "blog").unwrap();
        assert_eq!(v["id"], "blog");
        assert_eq!(v["text"].as_str().unwrap(), text);

        let err = fetchers.show(&env.ctx, "nope").unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn remove_restore_round_trips_and_purge_asks_before_deleting_for_good() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/f.xml"}),
        );
        fetchers.load(&env.ctx).unwrap();

        fetchers.remove(&env.ctx, "blog").unwrap();
        assert!(!fetchers.state().sources.contains_key("blog"));
        assert!(env.ctx.store.read(&schedule::path("blog")).unwrap().is_none());
        assert_eq!(fetchers.list(&env.ctx, ListParams { removed: true }).await.unwrap()["removed"], json!(["blog"]));

        let err = fetchers.remove(&env.ctx, "blog").unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::NotFound);

        let confirm_err = fetchers.purge(&env.ctx, Purge { id: None, confirm: None }).unwrap_err();
        assert_eq!(confirm_err.code, shimmer_core::ErrorCode::ConfirmationRequired);
        assert_eq!(confirm_err.detail, Some(json!({"sources": 1})));

        let restored = fetchers.restore(&env.ctx, "blog").unwrap();
        assert_eq!(restored["source"]["id"], "blog");
        assert!(fetchers.state().sources.contains_key("blog"));

        // Removed again, then a fresh file reappears at the same id before the old removed
        // copy is restored: the restore must refuse to clobber it.
        fetchers.remove(&env.ctx, "blog").unwrap();
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://other.test/f.xml"}),
        );
        let err = fetchers.restore(&env.ctx, "blog").unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::Conflict, "a source 'blog' already exists again");

        fetchers.remove(&env.ctx, "blog").unwrap();
        let purged =
            fetchers.purge(&env.ctx, Purge { id: Some("blog".into()), confirm: Some(json!({"sources": 1})) }).unwrap();
        assert_eq!(purged["purged"], 1);
        assert!(env.ctx.store.read(&schedule::removed_path("blog")).unwrap().is_none());
        assert_eq!(
            fetchers.purge(&env.ctx, Purge { id: None, confirm: None }).unwrap()["purged"],
            0,
            "nothing left to purge, no question asked"
        );
    }

    #[test]
    fn purge_also_deletes_the_sources_dedup_state_and_last_run_entry() {
        // #305's review, should-fix: left behind, a new source reusing a purged id inherits
        // its old dedup state (hiding every item it already "saw") and last-run time (its
        // first run delayed instead of running right away).
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/f.xml"}),
        );
        fetchers.load(&env.ctx).unwrap();
        env.ctx.store.write(&dedup::path("blog"), "c-1 = \"2026-01-01T00:00:00Z\"\n").unwrap();
        schedule::record_run(&env.ctx, "blog", env.ctx.clock.now()).unwrap();

        fetchers.remove(&env.ctx, "blog").unwrap();
        fetchers.purge(&env.ctx, Purge { id: Some("blog".into()), confirm: Some(json!({"sources": 1})) }).unwrap();

        assert!(env.ctx.store.read(&dedup::path("blog")).unwrap().is_none(), "seen.toml must go with it");
        assert!(!schedule::load_last_run(&env.ctx).unwrap().contains_key("blog"), "and its last_run.toml entry");
    }

    #[test]
    fn enable_and_disable_toggle_the_flag_and_keep_the_rest_of_the_file() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": false, "interval": "hourly", "kind": "rss", "url": "https://example.test/f.xml"}),
        );
        fetchers.load(&env.ctx).unwrap();

        let v = fetchers.set_enabled(&env.ctx, "blog", true).unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["kind_params"]["url"], "https://example.test/f.xml");
        assert!(fetchers.state().sources.get("blog").unwrap().enabled);

        let v = fetchers.set_enabled(&env.ctx, "blog", false).unwrap();
        assert_eq!(v["enabled"], false);

        let err = fetchers.set_enabled(&env.ctx, "nope", true).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::NotFound);
    }

    #[test]
    fn set_enabled_and_copy_refuse_a_hand_edited_non_table_source_instead_of_panicking() {
        // #305's review, should-fix (the same class #255's review flagged): indexing
        // `doc["source"]["enabled"]` into a `source = "x"` panics instead of erroring.
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        env.ctx.store.write(&schedule::path("blog"), "source = \"not a table\"\n").unwrap();

        let err = fetchers.set_enabled(&env.ctx, "blog", true).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);

        let err =
            fetchers.copy(&env.ctx, CopyParams { id: "blog".into(), to: "blog-2".into(), label: None }).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);
    }

    #[test]
    fn rename_moves_the_source_file_its_dedup_state_and_its_last_run_entry() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/f.xml"}),
        );
        fetchers.load(&env.ctx).unwrap();
        env.ctx.store.write(&dedup::path("blog"), "c-1 = \"2026-01-01T00:00:00Z\"\n").unwrap();
        schedule::record_run(&env.ctx, "blog", env.ctx.clock.now()).unwrap();

        let v = fetchers.rename(&env.ctx, MoveParams { id: "blog".into(), to: "team-blog".into() }).unwrap();
        assert_eq!(v["id"], "team-blog");
        assert!(env.ctx.store.read(&schedule::path("blog")).unwrap().is_none());
        assert!(env.ctx.store.read(&schedule::path("team-blog")).unwrap().is_some());
        assert!(env.ctx.store.read(&dedup::path("blog")).unwrap().is_none());
        assert!(env.ctx.store.read(&dedup::path("team-blog")).unwrap().is_some());
        let last_run = schedule::load_last_run(&env.ctx).unwrap();
        assert!(!last_run.contains_key("blog"));
        assert!(last_run.contains_key("team-blog"));
        assert!(!fetchers.state().sources.contains_key("blog"));
        assert!(fetchers.state().sources.contains_key("team-blog"));

        let err = fetchers.rename(&env.ctx, MoveParams { id: "team-blog".into(), to: "team-blog".into() }).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);
    }

    #[test]
    fn rename_refuses_a_collision_with_an_existing_id() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://a.test/f.xml"}),
        );
        write_source(
            &env,
            "other",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://b.test/f.xml"}),
        );
        fetchers.load(&env.ctx).unwrap();

        let err = fetchers.rename(&env.ctx, MoveParams { id: "blog".into(), to: "other".into() }).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::Conflict);
    }

    #[test]
    fn rename_copy_and_remove_all_refuse_a_source_that_is_fetching_right_now() {
        // #305's review, blocking: `run_source` loads `seen` at the start and writes it back,
        // at the end, under the *old* id -- renaming mid-fetch makes the new id miss that
        // run's items while the old id's moved-away dedup state still gets written to, so
        // next run re-reports them as new (duplicate `fetchers.item.found`, duplicate
        // automation messages).
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://a.test/f.xml"}),
        );
        fetchers.load(&env.ctx).unwrap();
        let _permit = fetchers.running.try_acquire("blog").unwrap();

        let err = fetchers.rename(&env.ctx, MoveParams { id: "blog".into(), to: "team-blog".into() }).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::Conflict);
        let err =
            fetchers.copy(&env.ctx, CopyParams { id: "blog".into(), to: "blog-2".into(), label: None }).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::Conflict);
        let err = fetchers.remove(&env.ctx, "blog").unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::Conflict);
        // Untouched: the file is still there under its original id.
        assert!(env.ctx.store.read(&schedule::path("blog")).unwrap().is_some());
    }

    #[test]
    fn copy_starts_disabled_with_no_dedup_or_last_run_state_and_takes_an_optional_label() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/f.xml"}),
        );
        fetchers.load(&env.ctx).unwrap();
        env.ctx.store.write(&dedup::path("blog"), "c-1 = \"2026-01-01T00:00:00Z\"\n").unwrap();
        schedule::record_run(&env.ctx, "blog", env.ctx.clock.now()).unwrap();

        let v = fetchers
            .copy(&env.ctx, CopyParams { id: "blog".into(), to: "blog-2".into(), label: Some("Blog copy".into()) })
            .unwrap();
        assert_eq!(v["enabled"], false);
        assert_eq!(v["label"], "Blog copy");
        assert!(env.ctx.store.read(&dedup::path("blog-2")).unwrap().is_none());
        assert!(!schedule::load_last_run(&env.ctx).unwrap().contains_key("blog-2"));
        // The original is untouched.
        assert!(fetchers.state().sources.get("blog").unwrap().enabled);

        let err =
            fetchers.copy(&env.ctx, CopyParams { id: "blog".into(), to: "blog-2".into(), label: None }).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::Conflict);
    }

    #[tokio::test]
    async fn list_removed_lists_ids_only() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/f.xml"}),
        );
        fetchers.load(&env.ctx).unwrap();
        fetchers.remove(&env.ctx, "blog").unwrap();

        let out = fetchers.list(&env.ctx, ListParams { removed: true }).await.unwrap();
        assert_eq!(out, json!({"removed": ["blog"]}));
    }
}
