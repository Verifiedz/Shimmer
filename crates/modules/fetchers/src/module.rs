use std::sync::Mutex;

use async_trait::async_trait;
use chrono::NaiveDate;
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use shimmer_core::filter::Filter;
use shimmer_core::params::decode;
use shimmer_core::{
    local_date, CommandSpec, Ctx, Error, ErrorCode, Execution, LaneConfig, Manifest, Module, Result, TriggerId,
    WriteLock,
};

use crate::dedup::{self, RunGuard};
use crate::schedule::{self, Sources, RUN_TASK_OP};
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
pub const TEMPLATES_OP: &str = "fetchers.templates";
pub const PEEK_OP: &str = "fetchers.peek";
pub const HEALTH_OP: &str = "fetchers.health";
/// Inline, answered from [`ITEMS`] alone -- never the event log (ADR 0042: a module has no way
/// to read that back; `core.events.list` reaches a client, not code running inside the daemon).
pub const ITEMS_OP: &str = "fetchers.items";

/// Caps `fetchers.item.found`'s `raw_data` (ADR 0028 §3) -- big enough for a job posting's
/// text, small enough that one large item can't become the next cache-bloat problem.
const MAX_RAW_DATA_CHARS: usize = 8 * 1024;
/// Every source's found items, oldest first, capped: `automations.history`'s precedent (ADR
/// 0042), not a second copy of `core.events.list`'s own query path. `fetchers.item.found` stays
/// the source of truth -- this is a convenience that answers "what did I just find" fast and
/// for free, same posture `automations`' own `history.json` already takes ("the event log has
/// everything").
const ITEMS: &str = "items.json";
/// Oldest dropped past this many. Twenty sources each finding a full `dedup::MAX_NEW_PER_RUN`
/// in one run would alone fill this -- the cap exists so one noisy run across many sources
/// can't make `fetchers.items` an ever-growing file, not to comfortably hold every source's
/// entire backlog at once.
const ITEMS_KEPT: usize = 1000;
/// However many fields an item has, each still keeps enough characters to be recognisable
/// rather than being cut to nothing.
const MIN_FIELD_CHARS: usize = 64;

#[derive(Default)]
pub struct Fetchers {
    /// A source's own schedule firing and a manual `fetchers.run` for the same source must
    /// never overlap (ADR 0028 §5/§6).
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

    // No `triggers()` override: a fixed heartbeat is replaced by each source scheduling its own
    // next run directly, via `ctx.schedules` (ADR 0028 §6) -- see `schedule_run`/
    // `cancel_schedule`/`reconcile_schedule` below.

    fn commands(&self) -> Vec<CommandSpec> {
        vec![
            CommandSpec {
                op: RUN_OP.into(),
                summary: "Fetch one source now".into(),
                params_schema: json!({
                    "type": "object", "required": ["id"],
                    "properties": {
                        "id": {"type": "string"},
                        "filter": {"type": "object"},
                    },
                }),
                // Inline, not Queued (see RUN_OP's own doc comment): it checks the source
                // exists itself, then enqueues RUN_TASK_OP for the real work.
                execution: Execution::Inline,
            },
            CommandSpec {
                op: RUN_TASK_OP.into(),
                summary: "Internal: the real fetch -- fetchers.run enqueues this after checking the source \
                          exists, and a source's own schedule fires it directly"
                    .into(),
                params_schema: json!({
                    "type": "object", "required": ["id"],
                    "properties": {
                        "id": {"type": "string"},
                        "filter": {"type": "object"},
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
                        "filter": {"type": "object"},
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
                summary: "Make a new source, from its own kind/interval or from a template's answers".into(),
                params_schema: new_input_schema(),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: TEMPLATES_OP.into(),
                summary: "Ready-made sources to make from a few answers, by category".into(),
                params_schema: json!({"type": "object", "properties": {}}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: PEEK_OP.into(),
                summary: "Everything about one template: what it does, what to know, its questions".into(),
                params_schema: json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}}),
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
            CommandSpec {
                op: HEALTH_OP.into(),
                summary: "Check every source for problems; changes nothing (ADR 0033)".into(),
                params_schema: json!({"type": "object"}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: ITEMS_OP.into(),
                summary: "Items a real run has actually found and kept, most recent first".into(),
                params_schema: json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string"},
                        "contains": {"type": "array", "items": {"type": "string"}},
                        "excludes": {"type": "array", "items": {"type": "string"}},
                    },
                }),
                execution: Execution::Inline,
            },
        ]
    }

    async fn handle(&self, op: &str, params: Value, ctx: &Ctx) -> Result<Value> {
        match op {
            RUN_OP => self.run(ctx, params).await,
            RUN_TASK_OP => {
                let p: FetchParams = decode(params)?;
                let filter = validate_filter(p.filter)?;
                self.fetch(ctx, p.id, filter).await
            }
            LIST_OP => {
                let p: ListParams = decode(params)?;
                self.list(ctx, p).await
            }
            STATUS_OP => self.status(ctx),
            TEST_OP => {
                let p: FetchParams = decode(params)?;
                let filter = validate_filter(p.filter)?;
                self.test(ctx, p.id, filter).await
            }
            RELOAD_OP => {
                self.load(ctx)?;
                let st = self.state();
                Ok(json!({"sources": st.sources.len(), "problems": problems_of(&st)}))
            }
            NEW_OP => {
                let p: NewParams = decode(params)?;
                self.create(ctx, p)
            }
            TEMPLATES_OP => templates_reply(None),
            PEEK_OP => {
                let p: Id = decode(params)?;
                templates_reply(Some(&p.id))
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
            HEALTH_OP => self.health(ctx),
            ITEMS_OP => {
                let p: ItemsParams = decode(params)?;
                Ok(items_reply(ctx, p))
            }
            _ => Err(Error::unknown_op(format!("fetchers has no op '{op}'"))),
        }
    }
}

#[derive(Deserialize)]
struct FetchParams {
    id: String,
    /// An ad-hoc `core::filter::Filter` (ADR 0028 §8/§14), replacing -- never merging with --
    /// the source's own configured filter for this one call. Absent means the configured
    /// filter (if any) applies exactly as before this existed.
    #[serde(default)]
    filter: Option<Map<String, Value>>,
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
/// `SourceConfig::parse` (one validation path for both). `kind` is required: there is no
/// fixed, compiled-in registry to fall back to (ADR 0028 §10).
#[derive(Clone, Deserialize)]
struct SourceInput {
    id: String,
    #[serde(default)]
    label: Option<String>,
    kind: String,
    #[serde(default)]
    enabled: bool,
    interval: schedule::IntervalSpec,
    #[serde(default)]
    params: Map<String, Value>,
    /// A `core::filter::Filter` (ADR 0028 §8), checked against `{"title": …}` only. `None`
    /// keeps everything.
    #[serde(default)]
    filter: Option<Map<String, Value>>,
}

/// `fetchers.edit`'s params (ADR 0028 §9, amended by #305's review): the whole replacement
/// file text, same shape as `automations.edit`'s -- not a `SourceInput`, so nothing not
/// explicitly retyped is silently reset.
#[derive(Deserialize)]
struct EditInput {
    id: String,
    text: String,
}

/// `fetchers.new`'s own params (ADR 0028 §9/§10): the same shape as [`SourceInput`] above, or
/// `{"id","template","values","preview"?}` -- the two are mutually exclusive, which
/// `Fetchers::create` is what actually enforces. `kind`/`interval` are `Option` here, unlike
/// `SourceInput`, precisely so a template-only payload (naming neither) still decodes.
#[derive(Deserialize)]
struct NewParams {
    id: String,
    #[serde(default)]
    label: Option<String>,
    kind: Option<String>,
    #[serde(default)]
    enabled: bool,
    interval: Option<schedule::IntervalSpec>,
    #[serde(default)]
    params: Map<String, Value>,
    #[serde(default)]
    filter: Option<Map<String, Value>>,
    /// The template path (ADR 0028 §10): mutually exclusive with `kind`/`interval`/`params`.
    template: Option<String>,
    #[serde(default)]
    values: Map<String, Value>,
    /// With a template: the source it would make, without making it.
    #[serde(default)]
    preview: bool,
}

fn new_input_schema() -> Value {
    json!({
        "type": "object", "required": ["id"],
        "properties": {
            "id": {"type": "string"},
            "label": {"type": "string"},
            "kind": {"type": "string"},
            "enabled": {"type": "boolean"},
            "interval": {},
            "params": {"type": "object"},
            "filter": {"type": "object"},
            "template": {"type": "string"},
            "values": {"type": "object"},
            "preview": {"type": "boolean"},
        },
    })
}

/// `fetchers.templates`/`.peek` (ADR 0028 §10): every built-in template, or (with `id`) one of
/// them -- `not_found` naming the others when there's no such template. One function for both
/// ops, the same way `automations::templates` handles its own optional id.
fn templates_reply(id: Option<&str>) -> Result<Value> {
    let all = crate::template::builtin();
    let templates: Vec<Value> = match id {
        None => all.iter().map(crate::template::Template::to_wire).collect(),
        Some(id) => {
            let names: Vec<&str> = all.iter().map(|t| t.id.as_str()).collect();
            vec![all
                .iter()
                .find(|t| t.id == id)
                .ok_or_else(|| Error::not_found(format!("no template '{id}' (there are: {})", names.join(", "))))?
                .to_wire()]
        }
    };
    let categories: Vec<Value> =
        crate::template::CATEGORIES.iter().map(|(id, label)| json!({"id": id, "label": label})).collect();
    Ok(json!({"categories": categories, "templates": templates}))
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

/// `fetchers.items` (ADR 0042): `contains`/`excludes` are a plain case-insensitive substring
/// narrowing of what's already in [`ITEMS`] -- deliberately not a `core::filter::Filter`
/// (ADR 0028 §8), the same "that's a different, simpler thing" distinction the CLI's own
/// doc comment drew before this moved server-side.
#[derive(Default, Deserialize)]
struct ItemsParams {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    excludes: Vec<String>,
}

/// `FetchParams.filter`/`SourceInput.filter`, checked the same way `SourceConfig::parse`
/// checks a source's own configured one (ADR 0028 §8) -- `fetchers.run`/`.test` reach this
/// past the CLI's own `--filter` parsing, so a broken filter sent directly as an op's params
/// (an automation, `shimmer call`) is rejected here too, not just at file-load time.
fn validate_filter(filter: Option<Map<String, Value>>) -> Result<Option<Map<String, Value>>> {
    if let Some(f) = &filter {
        schedule::only_title_keys(f).map_err(Error::invalid_params)?;
        Filter::parse(f).map_err(Error::invalid_params)?;
    }
    Ok(filter)
}

impl Fetchers {
    /// Cancels `id`'s pending schedule, if it has one (ADR 0028 §6) -- used by `disable`/
    /// `remove`, and by `rename`'s "remove the old, add a new one under the new id."
    fn cancel_schedule(&self, ctx: &Ctx, id: &str) -> Result<()> {
        if let Some(state) = schedule::load_schedule_state(ctx, id)? {
            // `false` (already gone -- it fired, or was somehow removed already) is fine; this
            // is "make sure nothing is pending," not "assert one was."
            ctx.schedules.remove(&TriggerId::new(state.trigger_id))?;
            schedule::delete_schedule_state(ctx, id)?;
        }
        Ok(())
    }

    /// Replaces `id`'s pending schedule (if any) with one due at `at` (ADR 0028 §6). The op
    /// fired is always [`RUN_TASK_OP`] with just `{"id"}` -- a scheduled run uses the source's
    /// own configured filter, never an ad-hoc override.
    fn schedule_run(&self, ctx: &Ctx, id: &str, at: chrono::DateTime<chrono::Utc>) -> Result<()> {
        self.cancel_schedule(ctx, id)?;
        let trigger_id = ctx.schedules.add_once(RUN_TASK_OP, json!({"id": id}), at, &format!("fetchers: {id}"))?;
        schedule::save_schedule_state(ctx, id, trigger_id.as_str(), at)
    }

    /// Reconciles `id`'s pending schedule after a settings change (ADR 0028 §6): cancels it if
    /// no longer enabled, schedules it if newly enabled, and recomputes its due time if the
    /// interval change moved it -- a no-op otherwise, so editing a source's label doesn't
    /// needlessly reschedule it. `was_enabled` is the state *before* this change; `cfg` is the
    /// state *after* it, already written to disk by the caller.
    fn reconcile_schedule(&self, ctx: &Ctx, id: &str, was_enabled: bool, cfg: &schedule::SourceConfig) -> Result<()> {
        if !cfg.enabled {
            if was_enabled {
                self.cancel_schedule(ctx, id)?;
            }
            return Ok(());
        }
        // 0: already validated at parse time against the right per-`Method` floor.
        let interval_s = cfg.interval.resolve(0).unwrap_or(0);
        let last_run = schedule::load_last_run(ctx)?;
        let basis = last_run.get(id).copied().unwrap_or_else(|| ctx.clock.now());
        let due_at = basis + chrono::Duration::seconds(interval_s as i64);
        if was_enabled {
            if let Some(state) = schedule::load_schedule_state(ctx, id)? {
                if state.due_at == due_at {
                    return Ok(()); // nothing that affects timing changed
                }
            }
        }
        self.schedule_run(ctx, id, due_at)
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
    /// fetchers" for a person's own `fetchers run`, rather than attributing it to them
    /// directly -- distinct from a source's own schedule firing, which shows as
    /// `Origin::Scheduler` (the trigger that fired it), since that one really does come from
    /// `ctx.schedules`. Priority and lane ordering are unaffected -- every origin shares the
    /// same arrival-order queue except `Overridden` (CLAUDE.md §6.2) -- this is a display-only
    /// cost, accepted for the correctness fix.
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

    async fn fetch(&self, ctx: &Ctx, source_id: String, filter_override: Option<Map<String, Value>>) -> Result<Value> {
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
        // retried at most once per interval (ADR 0028 §6: its next scheduled run), not
        // retried immediately. Held only across this call -- up to 8
        // fetches finish around the same time on this lane, and this file's own
        // read-modify-write must not interleave with another source's (PR #126 review).
        //
        // The one exception is cancellation (#126's review, B11): the user stopping a run
        // isn't the source having been fetched, and recording it made the next run wait a
        // full interval (a day, for `daily`) having fetched nothing.
        let _g = self.write.lock();
        if !ctx.cancel.is_cancelled() {
            match &outcome {
                Ok(_) => schedule::record_run(ctx, &source_id, ctx.clock.now())?,
                // `fetchers.health`'s own reading of this file (ADR 0028 §11) -- the same
                // classification `fetchers.fetch.failed` emits below, never a second one.
                Err(e) => {
                    schedule::record_failure(ctx, &source_id, ctx.clock.now(), &e.message, classify_failure(ctx, e))?
                }
            }
        }
        // Rescheduled regardless of cancellation (#306's review, blocking; the same root
        // issue as #299's review point 1): the one-time schedule that fired to start this
        // run is already consumed by the time this runs, so skipping this whole block on
        // cancel -- as it used to -- left the source with no pending schedule at all,
        // forever, until some unrelated write (an edit, enable/disable) happened to touch
        // it. A task's cancel token is `shutdown.child_token()` (`queue/mod.rs`), so any
        // daemon shutdown mid-fetch did this, not just an explicit `queue cancel`. Not
        // recording the run above already means a cancelled run isn't treated as fetched,
        // so the reschedule below computes the same due time as before -- due again right
        // away, not after a full extra interval.
        //
        // The consumed state is cancelled first, unconditionally, rather than left for
        // `reconcile_schedule` to notice: that function skips rescheduling when the due
        // time hasn't moved, assuming the existing state is still a *live, pending* entry --
        // true when it's reconciling a settings change, false here, where it's reconciling
        // a trigger that already fired. On a cancelled run the due time doesn't move
        // (above), so without this, `reconcile_schedule` would see its own stale, already-
        // fired state and skip, keeping the source permanently un-scheduled -- exactly the
        // bug this is fixing.
        self.cancel_schedule(ctx, &source_id)?;
        if let Some(cfg) = configured {
            self.reconcile_schedule(ctx, &source_id, cfg.enabled, cfg)?;
        }
        drop(_g);

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
        let last_failures = schedule::load_last_failures(ctx);
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
                    "kind": cfg.map(|c| c.kind.clone()),
                    "enabled": cfg.is_some_and(|c| c.enabled),
                    // 0: already validated at load time (§126 review's per-method floor);
                    // this is read-only display, not re-validation.
                    "interval_s": cfg.and_then(|c| c.interval.resolve(0).ok()),
                    "filter": cfg.and_then(|c| c.filter.clone()),
                    "last_run_at": last_run.get(id),
                    // `None` once the next run succeeds (ADR 0028 §11) -- the most recent
                    // outcome only, never a history.
                    "last_error": last_failures.get(id).map(|(_, error, reason)| json!({"error": error, "reason": reason})),
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

    /// `shimmer doctor`'s own check (ADR 0033, ADR 0028 §11): two kinds of problem, both
    /// read fresh from current state every call, never accumulated -- a fixed source simply
    /// stops appearing next time. A source file that doesn't parse (§5's `problems`) is
    /// `fail`: nothing about it can be used until it's fixed. A source whose most recent run
    /// failed is `warn`: it still has a valid, working file, and the next scheduled run may
    /// well succeed on its own.
    fn health(&self, ctx: &Ctx) -> Result<Value> {
        let st = self.state();
        let mut checks: Vec<Value> = st
            .problems
            .iter()
            .map(|(id, why)| {
                json!({"status": "fail", "subject": id,
                    "message": format!("{id} can't be used: {why}"),
                    "fix": {"summary": format!("fix or remove its file: data/fetchers/sources/{id}.toml")}})
            })
            .collect();
        let total = st.sources.len();
        drop(st);
        for (id, (at, error, reason)) in schedule::load_last_failures(ctx) {
            checks.push(json!({"status": "warn", "subject": id,
                "message": format!("{id}'s last run ({at}) failed: {error} ({reason})"),
                "fix": {"summary": "see what happened", "op": SHOW_OP, "params": {"id": id}}}));
        }
        if let Some(why) = schedule::last_run_problem(ctx) {
            checks.push(json!({"status": "warn", "message": why}));
        }
        if checks.is_empty() {
            checks.push(json!({"status": "ok",
                "message": format!("{total} {}, all fine", if total == 1 { "source" } else { "sources" })}));
        }
        Ok(json!({"checks": checks}))
    }

    /// Fetches exactly one page from a source and returns its parsed items -- never the
    /// `SeenSet`, `last_run.toml`, or any event (issue #128: "without saving or marking
    /// anything seen"). Deliberately simpler than [`Fetchers::run_source`]: a dry run has no
    /// state of its own to protect, so it skips the overlap guard too -- a `test` can run
    /// alongside a real `fetch` of the same source without conflict.
    async fn test(&self, ctx: &Ctx, source_id: String, filter_override: Option<Map<String, Value>>) -> Result<Value> {
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
        let today = local_date(ctx.clock.now(), &ctx.local_tz);
        let tz = ctx.local_tz.tz();
        let items: Vec<Value> = page
            .items
            .into_iter()
            .filter_map(RawItem::sanitized)
            .filter(|item| passes_title_filter(&item.title, filter_override.as_ref(), configured, today, tz))
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
        filter_override: Option<&Map<String, Value>>,
        configured: Option<&schedule::SourceConfig>,
    ) -> Result<usize> {
        let source_id = source.id();
        let mut seen = dedup::load(ctx, source_id)?;
        let now = ctx.clock.now();
        let today = local_date(now, &ctx.local_tz);
        let tz = ctx.local_tz.tz();
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
                    if !passes_title_filter(&item.title, filter_override, configured, today, tz) {
                        // A *configured* filter is a standing decision, so a title it drops is
                        // marked seen and never re-examined. A one-off `--filter`
                        // on `run` is not: marking those seen meant a single experiment hid the
                        // rest of a source's backlog for the full 90-day retention (#126's
                        // review, B2). An override leaves
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
        // A separate write, after the transaction above has committed: [`ITEMS`] is a
        // convenience cache (ADR 0042), not the source of truth the previous transaction's
        // events already are, so a crash between the two loses at most this cache's copy of
        // what just happened, never the record of it happening. Held only across this call --
        // up to 8 fetches finish around the same time on this lane, and this file's own
        // read-modify-write must not interleave with another source's, the same reasoning
        // `fetch`'s own `last_run.toml` update already uses this lock for.
        if !found.is_empty() {
            let _g = self.write.lock();
            let mut items = item_log(ctx);
            for item in &found {
                let mut entry = item_payload(source_id, item);
                entry["at"] = json!(now);
                items.push(entry);
            }
            let drop = items.len().saturating_sub(ITEMS_KEPT);
            items.drain(..drop);
            let text = serde_json::to_string_pretty(&items).map_err(|e| Error::internal(e.to_string()))?;
            ctx.store.write(ITEMS, text)?;
        }
        Ok(new_count)
    }
}

/// The command set (ADR 0028 §9, ADR 0038 §3/§6): every op that creates, changes, or
/// removes a source's own file, mirroring `automations`' rule commands almost verbatim --
/// the two modules store one definition per named thing the same way, so the same shape of
/// code handles it (CLAUDE.md §8).
impl Fetchers {
    /// A new source (ADR 0028 §9/§10): either its own kind/interval, validated the same way a
    /// hand-edited file is (through `schedule::render` then `SourceConfig::parse` -- one
    /// validation path for both a structured command and `fetchers.reload`), or a template's
    /// answers (§10), mirroring `automations.new`'s own dual shape almost verbatim.
    fn create(&self, ctx: &Ctx, p: NewParams) -> Result<Value> {
        if let Some(template) = &p.template {
            if p.kind.is_some()
                || p.interval.is_some()
                || p.label.is_some()
                || !p.params.is_empty()
                || p.filter.is_some()
            {
                return Err(Error::invalid_params("a source comes from a template or from its own kind, not both"));
            }
            return self.create_from_template(ctx, &p.id, template, &p.values, p.preview);
        }
        if p.preview || !p.values.is_empty() {
            return Err(Error::invalid_params("values and preview go with a template"));
        }
        let kind = p.kind.ok_or_else(|| Error::invalid_params("needs a kind (or a template)"))?;
        let interval = p.interval.ok_or_else(|| Error::invalid_params("needs an interval (or a template)"))?;
        let input = SourceInput {
            id: p.id,
            label: p.label,
            kind,
            enabled: p.enabled,
            interval,
            params: p.params,
            filter: p.filter,
        };
        let _g = self.write.lock();
        if ctx.store.read(&schedule::path(&input.id))?.is_some() {
            return Err(Error::conflict(format!("source '{}' already exists", input.id)));
        }
        self.write_source(ctx, input, false, "fetchers.source.created")
    }

    /// The source `template` makes with these answers (§10), checked like any source; with
    /// `preview`, only shown, never written.
    fn create_from_template(
        &self,
        ctx: &Ctx,
        id: &str,
        template: &str,
        values: &Map<String, Value>,
        preview: bool,
    ) -> Result<Value> {
        let t = crate::template::builtin()
            .into_iter()
            .find(|t| t.id == template)
            .ok_or_else(|| Error::not_found(format!("no template '{template}' (shimmer fetchers templates)")))?;
        let answers = t.answers(values).map_err(Error::invalid_params)?;
        let text = t.fill(&answers);
        let cfg = schedule::SourceConfig::parse(id, &text).map_err(|m| Error::invalid_params(format!("{id}: {m}")))?;
        if preview {
            return Ok(json!({"preview": true, "template": template, "source": {"id": id, "text": text}}));
        }
        let _g = self.write.lock();
        if ctx.store.read(&schedule::path(id))?.is_some() {
            return Err(Error::conflict(format!("source '{id}' already exists")));
        }
        ctx.store.transaction(|tx| {
            tx.put(&schedule::path(id), text.as_str())?;
            tx.emit("fetchers.source.created", json!({"source": id, "template": template}))
        })?;
        let mut st = self.state();
        st.problems.remove(id);
        st.sources.insert(id.to_owned(), cfg.clone());
        drop(st);
        self.reconcile_schedule(ctx, id, false, &cfg)?;
        Ok(json!({"template": template, "source": view(id, &cfg)}))
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
        // `false` for a source whose old file was broken (in `problems`, not `sources`): a
        // file that never successfully parsed never had a schedule to reconcile against.
        let was_enabled = self.state().sources.get(id).map(|c| c.enabled).unwrap_or(false);
        let cfg = schedule::SourceConfig::parse(id, text).map_err(Error::invalid_params)?;
        ctx.store.transaction(|tx| {
            tx.put(&schedule::path(id), text)?;
            tx.emit("fetchers.source.edited", json!({"source": id}))
        })?;
        let mut st = self.state();
        st.problems.remove(id);
        st.sources.insert(id.to_owned(), cfg.clone());
        drop(st);
        self.reconcile_schedule(ctx, id, was_enabled, &cfg)?;
        Ok(view(id, &cfg))
    }

    fn write_source(&self, ctx: &Ctx, p: SourceInput, was_enabled: bool, topic: &str) -> Result<Value> {
        let text = schedule::render(p.label.as_deref(), &p.kind, p.enabled, &p.interval, &p.params, p.filter.as_ref())
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
        self.reconcile_schedule(ctx, &p.id, was_enabled, &cfg)?;
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
        v["last_error"] = json!(schedule::load_last_failures(ctx)
            .get(id)
            .map(|(_, error, reason)| json!({"error": error, "reason": reason})));
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
        drop(st);
        // Nothing should fire for a removed source (ADR 0028 §6).
        self.cancel_schedule(ctx, id)?;
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
                // Re-adds a schedule the same way enabling does (ADR 0028 §6) -- `remove`
                // already cancelled whatever it had before, so this starts fresh.
                self.reconcile_schedule(ctx, id, false, &cfg)?;
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
        self.reconcile_schedule(ctx, id, !on, &cfg)?;
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
        let old_schedule = schedule::load_schedule_state(ctx, &p.id)?;
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
        // The pending schedule follows the rename at its same due time (ADR 0028 §6) -- simpler
        // than reconciling, which would recompute a (possibly different) due time from scratch.
        if let Some(state) = old_schedule {
            ctx.schedules.remove(&TriggerId::new(state.trigger_id))?;
            schedule::delete_schedule_state(ctx, &p.id)?;
            let trigger_id = ctx.schedules.add_once(
                RUN_TASK_OP,
                json!({"id": p.to}),
                state.due_at,
                &format!("fetchers: {}", p.to),
            )?;
            schedule::save_schedule_state(ctx, &p.to, trigger_id.as_str(), state.due_at)?;
        }
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
        "filter": cfg.filter,
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

/// Whether an item's title should be kept: `filter_override` (ADR 0028 §8/§14), when given,
/// completely replaces `configured`'s own filter for this one call -- the two are never
/// merged. With neither present, everything passes.
fn passes_title_filter(
    title: &str,
    filter_override: Option<&Map<String, Value>>,
    configured: Option<&schedule::SourceConfig>,
    today: NaiveDate,
    tz: Tz,
) -> bool {
    match filter_override {
        Some(f) => schedule::matches_title(Some(f), title, today, tz),
        None => configured.is_none_or(|c| c.matches_title(title, today, tz)),
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

/// [`ITEMS`], oldest first. A broken file reads as empty, the same convenience-not-truth
/// posture `automations::history` already takes -- `fetchers.item.found` events are what
/// actually happened; this is only a fast, capped way to show it.
fn item_log(ctx: &Ctx) -> Vec<Value> {
    ctx.store
        .read_string(ITEMS)
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok())
        .unwrap_or_default()
}

/// `fetchers.items` (ADR 0042): matched, most recent first. Reads [`ITEMS`] fresh on every
/// call rather than through `self.state()` -- it is small and capped, and a plain read keeps
/// this op from needing any lock a concurrent `run_source` write might contend with.
fn items_reply(ctx: &Ctx, p: ItemsParams) -> Value {
    let matching: Vec<Value> = item_log(ctx)
        .into_iter()
        .rev()
        .filter(|e| p.source.as_deref().is_none_or(|s| e["source_name"] == s))
        .filter(|e| title_matches(e["title"].as_str().unwrap_or_default(), &p.contains, &p.excludes))
        .collect();
    json!({"items": matching})
}

/// Same matching rule the CLI's own `items` used to apply client-side, before this moved
/// server-side (ADR 0042): a plain case-insensitive substring match, OR'd within each list --
/// deliberately simpler than `core::filter::Filter` (ADR 0028 §8), which is for a source's own
/// standing configuration, not a one-off "show me the ones about X" narrowing of history.
fn title_matches(title: &str, contains: &[String], excludes: &[String]) -> bool {
    let lower = title.to_lowercase();
    let included = contains.is_empty() || contains.iter().any(|w| lower.contains(&w.to_lowercase()));
    let excluded = excludes.iter().any(|w| lower.contains(&w.to_lowercase()));
    included && !excluded
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
    fn declares_no_fixed_triggers() {
        // Each source schedules its own next run directly, via `ctx.schedules` (ADR 0028 §6) --
        // no heartbeat.
        assert_eq!(Fetchers::default().triggers(), Vec::new());
    }

    #[tokio::test]
    async fn init_succeeds_with_nothing_registered() {
        let env = TestEnv::new("fetchers");
        Fetchers::default().init(&env.ctx).await.unwrap();
    }

    #[tokio::test]
    async fn init_loads_every_source_file_so_list_sees_them_with_no_reload_needed() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
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

        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
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
    fn validate_filter_is_none_only_when_nothing_was_given() {
        assert!(validate_filter(None).unwrap().is_none());
        let f = json!({"title": {"contains": "junior"}}).as_object().unwrap().clone();
        assert_eq!(validate_filter(Some(f.clone())).unwrap(), Some(f));
    }

    #[test]
    fn an_invalid_filter_from_a_caller_is_rejected_not_just_defanged() {
        // `fetchers.run`/`fetchers.test` reach `validate_filter` straight from an op's params,
        // past the CLI's own `--filter` parsing, so a broken filter sent directly by an
        // automation or `shimmer call` must hit the same rejection a broken configured filter
        // would at file-load time.
        let bad = json!({"title": {"contains": ""}}).as_object().unwrap().clone();
        let err = validate_filter(Some(bad)).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);
        assert!(err.message.contains("contains"), "{}", err.message);
    }

    #[test]
    fn a_caller_supplied_filter_on_any_field_but_title_is_also_rejected() {
        // #311's review, blocking, through the same ad-hoc override path as above -- a
        // `fetchers.run`/`.test`/`.new` filter that only names a field no one checks must be
        // caught here too, not just at file-load time (`schedule::only_title_keys`).
        let bad = json!({"company": {"contains": "Acme"}}).as_object().unwrap().clone();
        let err = validate_filter(Some(bad)).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);
        assert!(err.message.contains("company"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_source_already_fetching_is_refused_not_double_run() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
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
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring Rust engineers.");

        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
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

        // The same run also appended to fetchers.items' own log (ADR 0042) -- fetchers.items
        // answers from this, not from re-querying the events above.
        let log = item_log(&env.ctx);
        assert_eq!(log.len(), 1, "the already-seen second run must not append a duplicate");
        assert_eq!(
            (log[0]["source_name"].as_str(), log[0]["source_id"].as_str()),
            (Some("hn-whoishiring"), Some("10"))
        );
        assert!(log[0]["at"].is_string(), "each entry carries when it was found");
    }

    #[tokio::test]
    async fn the_items_log_drops_the_oldest_past_its_cap() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring Rust engineers.");

        let padding: Vec<Value> = (0..ITEMS_KEPT)
            .map(|i| json!({"source_name": "old", "title": format!("old {i}"), "url": "https://x", "at": "2020-01-01T00:00:00Z"}))
            .collect();
        env.ctx.store.write(ITEMS, serde_json::to_string(&padding).unwrap()).unwrap();

        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();

        let log = item_log(&env.ctx);
        assert_eq!(log.len(), ITEMS_KEPT, "stays capped, the oldest dropped to make room for the new one");
        assert_eq!(log.last().unwrap()["source_name"], "hn-whoishiring", "the newest item lands at the end");
        assert_eq!(log.first().unwrap()["title"], "old 1", "\"old 0\", the very oldest, was the one dropped");
    }

    #[test]
    fn title_matches_is_case_insensitive_and_excludes_win_over_contains() {
        assert!(title_matches("Junior Rust Engineer", &[], &[]), "no filter at all keeps everything");
        assert!(title_matches("Junior Rust Engineer", &["junior".into()], &[]));
        assert!(!title_matches("Senior Rust Engineer", &["junior".into()], &[]), "contains must actually match");
        assert!(
            !title_matches("Junior Rust Engineer", &["junior".into()], &["engineer".into()]),
            "an excludes match drops it even though contains would have kept it"
        );
    }

    #[test]
    fn items_reply_filters_by_source_and_title_most_recent_first() {
        let env = TestEnv::new("fetchers");
        let log = vec![
            json!({"source_name": "a", "title": "Junior Rust Engineer", "url": "https://x/1"}),
            json!({"source_name": "b", "title": "Senior Rust Engineer", "url": "https://x/2"}),
            json!({"source_name": "a", "title": "Staff Python Engineer", "url": "https://x/3"}),
        ];
        env.ctx.store.write(ITEMS, serde_json::to_string(&log).unwrap()).unwrap();

        let all = items_reply(&env.ctx, ItemsParams::default());
        let titles: Vec<&str> = all["items"].as_array().unwrap().iter().map(|i| i["title"].as_str().unwrap()).collect();
        assert_eq!(
            titles,
            vec!["Staff Python Engineer", "Senior Rust Engineer", "Junior Rust Engineer"],
            "oldest-first on disk, most recent first in the reply"
        );

        let a_only = items_reply(&env.ctx, ItemsParams { source: Some("a".into()), ..Default::default() });
        assert_eq!(a_only["items"].as_array().unwrap().len(), 2);

        let rust_only = items_reply(&env.ctx, ItemsParams { contains: vec!["rust".into()], ..Default::default() });
        assert_eq!(rust_only["items"].as_array().unwrap().len(), 2);

        let no_senior = items_reply(&env.ctx, ItemsParams { excludes: vec!["senior".into()], ..Default::default() });
        assert_eq!(no_senior["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn items_reply_with_no_log_yet_is_empty_not_an_error() {
        let env = TestEnv::new("fetchers");
        assert_eq!(items_reply(&env.ctx, ItemsParams::default()), json!({"items": []}));
    }

    #[tokio::test]
    async fn run_source_drops_an_excluded_title_and_never_retries_it() {
        let env = TestEnv::new("fetchers");
        write_source(
            &env,
            "hn-whoishiring",
            json!({
                "enabled": true, "interval": "daily", "kind": "hn-whoishiring",
                "filter": {"title": {"not_contains": "senior"}},
            }),
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
            json!({
                "enabled": true, "interval": "daily", "kind": "hn-whoishiring",
                "filter": {"title": {"contains": "junior"}},
            }),
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
            json!({
                "enabled": true, "interval": "daily", "kind": "hn-whoishiring",
                "filter": {"title": {"contains": "junior"}},
            }),
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
        let override_filter = json!({"title": {"contains": "senior"}}).as_object().unwrap().clone();
        let out = fetchers.test(&env.ctx, "hn-whoishiring".into(), Some(override_filter)).await.unwrap();
        let items = out["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(items[0]["title"].as_str().unwrap().contains("Senior"), "the override must win, not merge with config");
    }

    #[tokio::test]
    async fn an_override_filter_on_a_real_run_leaves_the_items_it_drops_findable_later() {
        let env = TestEnv::new("fetchers");
        // No configured filter at all -- the override is the only one.
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
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

        let override_filter = json!({"title": {"not_contains": "senior"}}).as_object().unwrap().clone();
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
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
    async fn list_shows_only_a_configured_source() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "my-hn", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        write_source(
            &env,
            "my-team-blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"}),
        );

        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        let out = fetchers.list(&env.ctx, ListParams::default()).await.unwrap();
        let sources = out["sources"].as_array().unwrap();
        // ADR 0028 §10 -- there is no fixed, compiled-in registry any more; only ids with
        // their own config entry show up at all.
        assert_eq!(sources.len(), 2, "{sources:?}");

        let hn = sources.iter().find(|s| s["id"] == "my-hn").unwrap();
        assert_eq!(
            (hn["method"].as_str(), hn["enabled"].as_bool(), hn["kind"].as_str()),
            (Some("api"), Some(true), Some("hn-whoishiring"))
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
        // Both must have their own entry to show up in list at all (ADR 0028 §10/§19) -- there
        // is no fixed, compiled-in registry to resolve against without one.
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        write_source(
            &env,
            "weworkremotely",
            json!({"enabled": false, "interval": schedule::MIN_INTERVAL_SCRAPE_S, "kind": "wwr", "category": "remote-full-stack-programming-jobs"}),
        );
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring.");
        fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();

        let out = fetchers.list(&env.ctx, ListParams::default()).await.unwrap();
        let sources = out["sources"].as_array().unwrap();
        let hn = sources.iter().find(|s| s["id"] == "hn-whoishiring").unwrap();
        assert_eq!(hn["last_run_at"], json!(env.ctx.clock.now()));
        assert!(hn["last_error"].is_null(), "a successful run has no last error");
        assert_eq!(hn["running"], false);

        let wwr = sources.iter().find(|s| s["id"] == "weworkremotely").unwrap();
        assert!(wwr["last_run_at"].is_null(), "never fetched -- no last run");
    }

    #[test]
    fn status_is_a_feature_wide_summary_not_per_source() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        write_source(
            &env,
            "weworkremotely",
            json!({"enabled": false, "interval": schedule::MIN_INTERVAL_SCRAPE_S, "kind": "wwr", "category": "remote-full-stack-programming-jobs"}),
        );
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();

        let out = fetchers.status(&env.ctx).unwrap();
        assert_eq!(out, json!({"sources": 2, "enabled": 1, "disabled": 1, "problems": []}));
    }

    #[test]
    fn health_is_one_ok_check_when_nothing_is_wrong() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "my-hn", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();

        let out = fetchers.health(&env.ctx).unwrap();
        let checks = out["checks"].as_array().unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0]["status"], "ok");
        assert!(checks[0]["message"].as_str().unwrap().contains("1 source"), "{}", checks[0]["message"]);
    }

    #[test]
    fn health_reports_a_broken_file_and_a_failed_run_and_a_fix_clears_each() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "my-hn", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        env.ctx.store.write(&schedule::path("broken"), "not valid toml [[[").unwrap();
        schedule::record_failure(&env.ctx, "my-hn", env.ctx.clock.now(), "boom", "network").unwrap();
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();

        let out = fetchers.health(&env.ctx).unwrap();
        let checks = out["checks"].as_array().unwrap();
        assert_eq!(checks.len(), 2);
        let broken = checks.iter().find(|c| c["subject"] == "broken").expect("the broken file is reported");
        assert_eq!(broken["status"], "fail");
        let failed = checks.iter().find(|c| c["subject"] == "my-hn").expect("the failed run is reported");
        assert_eq!(failed["status"], "warn");
        assert!(failed["message"].as_str().unwrap().contains("boom"), "{}", failed["message"]);
        assert_eq!(failed["fix"]["op"], SHOW_OP);
        assert_eq!(failed["fix"]["params"], json!({"id": "my-hn"}));

        // Fixing each -- removing the broken file, a fresh successful run -- clears it from
        // the next call; health is read fresh every time, never accumulated.
        env.ctx.store.delete(&schedule::path("broken")).unwrap();
        fetchers.load(&env.ctx).unwrap();
        schedule::record_run(&env.ctx, "my-hn", env.ctx.clock.now()).unwrap();
        let out = fetchers.health(&env.ctx).unwrap();
        assert_eq!(out["checks"][0]["status"], "ok");
    }

    #[tokio::test]
    async fn test_shows_parsed_items_without_touching_dedup_last_run_or_events() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring Rust engineers.");

        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        let out = fetchers.test(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();
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
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring.");
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
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
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();

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

        // The same classification lands in `last_run.toml` (ADR 0028 §11), for
        // `fetchers.health` to read back without a second one.
        let failures = schedule::load_last_failures(&env.ctx);
        let (_, error, reason) = failures.get("hn-whoishiring").expect("the failure is recorded");
        assert_eq!(reason, "network");
        assert_eq!(error, &failed[0].payload["error"].as_str().unwrap().to_owned());
    }

    #[tokio::test]
    async fn handle_routes_both_ops_by_name() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        stub_one_hn_comment(&env, 20, "<p>Widgets Inc<p>Hiring.");
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();

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
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        let out = fetchers
            .handle(RUN_OP, json!({"id": "hn-whoishiring", "filter": {"title": {"contains": "junior"}}}), &env.ctx)
            .await
            .unwrap();
        assert_eq!(out["queued"], true);

        let submitted = env.queue.requests.lock().unwrap();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].op, RUN_TASK_OP);
        assert_eq!(submitted[0].params, json!({"id": "hn-whoishiring", "filter": {"title": {"contains": "junior"}}}));
    }

    #[tokio::test]
    async fn run_source_bails_immediately_once_cancelled_rather_than_paging_on() {
        let env = TestEnv::new("fetchers");
        // Deliberately no stubs at all: if the cancellation check did not run before the
        // first page fetch, this would instead fail with "no stub for...", not the
        // `cancelled` classification this test is actually about.
        env.ctx.cancel.cancel();
        let fetchers = Fetchers::default();

        let source = crate::sources::hn::HnWhoIsHiring::new("hn-whoishiring".into());
        let err = fetchers.run_source(&env.ctx, &source, None, None).await.unwrap_err();
        assert!(env.ctx.cancel.is_cancelled());
        assert_eq!(classify_failure(&env.ctx, &err), "cancelled");
    }

    #[tokio::test]
    async fn a_cancelled_fetch_is_classified_and_emitted_as_cancelled_not_whatever_error_surfaced() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "hn-whoishiring", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        env.ctx.cancel.cancel();

        let err = fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap_err();
        assert_eq!(classify_failure(&env.ctx, &err), "cancelled");
        let failed: Vec<_> = env.backend.events().into_iter().filter(|e| e.topic == "fetchers.fetch.failed").collect();
        assert_eq!(failed[0].payload["reason"], "cancelled");
    }

    #[tokio::test]
    async fn a_cancelled_fetch_still_reschedules_the_next_run() {
        // #306's review, blocking (the same root issue as #299's review point 1): the
        // one-time schedule that fired to start this run is already consumed by the time
        // `fetch` runs, so skipping the reschedule entirely on cancellation -- as it used
        // to -- left the source with no pending schedule at all, forever. A task's cancel
        // token is `shutdown.child_token()`, so any daemon shutdown mid-fetch did this, not
        // just an explicit `queue cancel`.
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let mut input = rss_input("blog", "https://example.test/f.xml");
        input.interval = schedule::IntervalSpec::Seconds(3600);
        fetchers.create(&env.ctx, as_new_params(input)).unwrap();
        let first = pending_schedule(&env, "blog").expect("create schedules the first run");

        // Time passes before the (eventually cancelled) run fires. `blog` never actually
        // ran, so a reconcile computes its due time from `now`, not `last_run` -- if the
        // stale state were left untouched (the bug), its `due_at` would still read as it
        // did at creation; a genuine reschedule must move it forward by this same amount.
        env.ctx.clock.advance(chrono::Duration::seconds(10).to_std().unwrap());
        env.ctx.cancel.cancel();
        let err = fetchers.fetch(&env.ctx, "blog".into(), None).await.unwrap_err();
        assert_eq!(classify_failure(&env.ctx, &err), "cancelled");

        assert!(
            !schedule::load_last_run(&env.ctx).unwrap().contains_key("blog"),
            "a cancelled run must not be recorded as fetched"
        );
        let second = pending_schedule(&env, "blog").expect("still scheduled after a cancelled run, not abandoned");
        assert_eq!(
            second.due_at,
            first.due_at + chrono::Duration::seconds(10),
            "the stale, already-fired schedule must be replaced by a freshly computed one, not left in place"
        );
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

        let source = crate::sources::hn::HnWhoIsHiring::new("hn-whoishiring".into());
        fetchers.run_source(&ctx, &source, None, None).await.unwrap();

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
            kind: "rss".into(),
            enabled: true,
            interval: schedule::IntervalSpec::Preset("hourly".into()),
            params: json!({"url": url}).as_object().unwrap().clone(),
            filter: None,
        }
    }

    /// `fetchers.new`'s structured shape, as [`SourceInput`] -- `Fetchers::create` itself
    /// takes [`NewParams`] (ADR 0028 §10's dual shape), so every test below that built a
    /// `SourceInput` before that split now converts through this.
    fn as_new_params(i: SourceInput) -> NewParams {
        NewParams {
            id: i.id,
            label: i.label,
            kind: Some(i.kind),
            enabled: i.enabled,
            interval: Some(i.interval),
            params: i.params,
            filter: i.filter,
            template: None,
            values: Map::new(),
            preview: false,
        }
    }

    #[test]
    fn create_makes_a_new_source_and_refuses_a_duplicate_id() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let v = fetchers.create(&env.ctx, as_new_params(rss_input("blog", "https://example.test/f.xml"))).unwrap();
        assert_eq!(v["id"], "blog");
        assert_eq!(v["kind"], "rss");
        assert!(fetchers.state().sources.contains_key("blog"));
        let saved = env.ctx.store.read_string(&schedule::path("blog")).unwrap().unwrap();
        let cfg = schedule::SourceConfig::parse("blog", &saved).unwrap();
        assert_eq!(cfg.kind_params.get("url").and_then(Value::as_str), Some("https://example.test/f.xml"));

        let err =
            fetchers.create(&env.ctx, as_new_params(rss_input("blog", "https://example.test/f.xml"))).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::Conflict);
    }

    #[test]
    fn create_rejects_invalid_params_and_leaves_nothing_behind() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let mut bad = rss_input("blog", "https://example.test/f.xml");
        bad.params = Map::new(); // rss needs a url
        let err = fetchers.create(&env.ctx, as_new_params(bad)).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);
        assert!(env.ctx.store.read(&schedule::path("blog")).unwrap().is_none());
    }

    #[test]
    fn templates_lists_every_built_in_and_peek_finds_one_by_id_or_reports_the_others() {
        let all = templates_reply(None).unwrap();
        let ids: Vec<&str> = all["templates"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["hn-whoishiring", "wwr", "greenhouse", "lever", "rss"]);
        assert!(!all["categories"].as_array().unwrap().is_empty());

        let one = templates_reply(Some("rss")).unwrap();
        assert_eq!(one["templates"].as_array().unwrap().len(), 1);
        assert_eq!(one["templates"][0]["id"], "rss");

        let err = templates_reply(Some("nope")).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::NotFound);
        assert!(err.message.contains("rss"), "{}", err.message);
    }

    #[test]
    fn create_from_a_template_previews_without_writing_then_makes_the_real_source() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let values = json!({"URL": "https://example.test/feed.xml"}).as_object().unwrap().clone();
        let params = NewParams {
            id: "my-feed".into(),
            label: None,
            kind: None,
            enabled: false,
            interval: None,
            params: Map::new(),
            filter: None,
            template: Some("rss".into()),
            values: values.clone(),
            preview: true,
        };
        let preview = fetchers.create(&env.ctx, params).unwrap();
        assert_eq!(preview["preview"], true);
        assert!(env.ctx.store.read(&schedule::path("my-feed")).unwrap().is_none(), "a preview writes nothing");

        let made = fetchers
            .create(
                &env.ctx,
                NewParams {
                    id: "my-feed".into(),
                    label: None,
                    kind: None,
                    enabled: false,
                    interval: None,
                    params: Map::new(),
                    filter: None,
                    template: Some("rss".into()),
                    values,
                    preview: false,
                },
            )
            .unwrap();
        assert_eq!(made["source"]["id"], "my-feed");
        assert_eq!(made["source"]["kind"], "rss");
        assert!(env.ctx.store.read(&schedule::path("my-feed")).unwrap().is_some());

        // Mixing a template with the structured shape, or sending a template's own
        // values/preview without one, are both refused (ADR 0028 §10's dual shape, same
        // posture `automations.new` already takes).
        let mixed = NewParams {
            id: "x".into(),
            label: None,
            kind: Some("rss".into()),
            enabled: false,
            interval: None,
            params: Map::new(),
            filter: None,
            template: Some("rss".into()),
            values: Map::new(),
            preview: false,
        };
        assert!(fetchers.create(&env.ctx, mixed).unwrap_err().message.contains("not both"));
        let stray_values = NewParams {
            template: None,
            values: json!({"URL": "x"}).as_object().unwrap().clone(),
            ..as_new_params(rss_input("y", "https://e.test/f"))
        };
        assert!(fetchers.create(&env.ctx, stray_values).unwrap_err().message.contains("go with a template"));
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
        assert!(v["last_error"].is_null(), "never run yet");

        schedule::record_failure(&env.ctx, "blog", env.ctx.clock.now(), "boom", "network").unwrap();
        let v = fetchers.show(&env.ctx, "blog").unwrap();
        assert_eq!(v["last_error"], json!({"error": "boom", "reason": "network"}));

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

    /// The one `OneTime` currently pending for `id`, by inspecting the test harness's own
    /// record of everything `ctx.schedules.add_once` was ever called with -- not
    /// `schedule::load_schedule_state`, so these tests also prove the sidecar file and the
    /// scheduler's own idea of what's pending agree.
    fn pending_schedule(env: &TestEnv, id: &str) -> Option<schedule::ScheduleState> {
        let state = schedule::load_schedule_state(&env.ctx, id).unwrap()?;
        let added = env.schedules.added.lock().unwrap();
        let (_, trigger_id, once) =
            added.iter().find(|(_, t, _)| t.as_str() == state.trigger_id).expect("state.toml names a real trigger");
        assert_eq!(once.op, RUN_TASK_OP);
        assert_eq!(once.params, json!({"id": id}));
        assert_eq!(trigger_id.as_str(), state.trigger_id);
        Some(state)
    }

    #[test]
    fn create_enabled_schedules_a_run_at_now_plus_interval_create_disabled_schedules_nothing() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let mut enabled = rss_input("blog", "https://example.test/f.xml");
        enabled.interval = schedule::IntervalSpec::Seconds(3600);
        fetchers.create(&env.ctx, as_new_params(enabled)).unwrap();
        let state = pending_schedule(&env, "blog").expect("an enabled source schedules its first run");
        assert_eq!(state.due_at, env.ctx.clock.now() + chrono::Duration::seconds(3600));

        let mut disabled = rss_input("quiet-blog", "https://example.test/g.xml");
        disabled.enabled = false;
        fetchers.create(&env.ctx, as_new_params(disabled)).unwrap();
        assert!(pending_schedule(&env, "quiet-blog").is_none());
    }

    #[test]
    fn enable_schedules_and_disable_cancels() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": false, "interval": 3600, "kind": "rss", "url": "https://e.test/f"}),
        );
        fetchers.load(&env.ctx).unwrap();
        assert!(pending_schedule(&env, "blog").is_none());

        fetchers.set_enabled(&env.ctx, "blog", true).unwrap();
        assert!(pending_schedule(&env, "blog").is_some());

        fetchers.set_enabled(&env.ctx, "blog", false).unwrap();
        assert!(pending_schedule(&env, "blog").is_none(), "disabling must cancel the pending schedule");
    }

    #[test]
    fn editing_the_interval_reschedules_but_an_unrelated_edit_does_not() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let mut input = rss_input("blog", "https://example.test/f.xml");
        input.interval = schedule::IntervalSpec::Seconds(3600);
        fetchers.create(&env.ctx, as_new_params(input)).unwrap();
        let first = pending_schedule(&env, "blog").unwrap();
        let first_trigger_count = env.schedules.added.lock().unwrap().len();

        // Same interval, only the label changes: must not touch the schedule at all.
        let relabeled = source_file(&json!({
            "enabled": true, "interval": 3600, "kind": "rss", "url": "https://example.test/f.xml",
            "label": "My Blog",
        }));
        fetchers.edit(&env.ctx, "blog", &relabeled).unwrap();
        assert_eq!(pending_schedule(&env, "blog"), Some(first.clone()), "an unrelated field must not reschedule");
        assert_eq!(env.schedules.added.lock().unwrap().len(), first_trigger_count, "no new schedule was added");

        // A longer interval must move the due time.
        let longer = source_file(&json!({
            "enabled": true, "interval": 7200, "kind": "rss", "url": "https://example.test/f.xml",
            "label": "My Blog",
        }));
        fetchers.edit(&env.ctx, "blog", &longer).unwrap();
        let second = pending_schedule(&env, "blog").unwrap();
        assert_ne!(second.due_at, first.due_at);
        assert_eq!(second.due_at, env.ctx.clock.now() + chrono::Duration::seconds(7200));
    }

    #[test]
    fn remove_cancels_the_pending_schedule_and_restore_schedules_a_fresh_one() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let mut input = rss_input("blog", "https://example.test/f.xml");
        input.interval = schedule::IntervalSpec::Seconds(3600);
        fetchers.create(&env.ctx, as_new_params(input)).unwrap();
        assert!(pending_schedule(&env, "blog").is_some());

        fetchers.remove(&env.ctx, "blog").unwrap();
        assert!(pending_schedule(&env, "blog").is_none());

        fetchers.restore(&env.ctx, "blog").unwrap();
        assert!(pending_schedule(&env, "blog").is_some(), "restoring an enabled source re-schedules it");
    }

    #[test]
    fn rename_moves_the_pending_schedule_to_the_new_id_at_the_same_due_time() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        let mut input = rss_input("blog", "https://example.test/f.xml");
        input.interval = schedule::IntervalSpec::Seconds(3600);
        fetchers.create(&env.ctx, as_new_params(input)).unwrap();
        let before = pending_schedule(&env, "blog").unwrap();

        fetchers.rename(&env.ctx, MoveParams { id: "blog".into(), to: "team-blog".into() }).unwrap();
        assert!(pending_schedule(&env, "blog").is_none());
        let after = pending_schedule(&env, "team-blog").expect("the schedule follows the rename");
        assert_eq!(after.due_at, before.due_at, "rename keeps the same due time, unlike an interval edit");
    }

    #[test]
    fn copy_never_schedules_since_it_always_starts_disabled() {
        let env = TestEnv::new("fetchers");
        let fetchers = Fetchers::default();
        write_source(
            &env,
            "blog",
            json!({"enabled": true, "interval": 3600, "kind": "rss", "url": "https://e.test/f"}),
        );
        fetchers.load(&env.ctx).unwrap();

        fetchers.copy(&env.ctx, CopyParams { id: "blog".into(), to: "blog-2".into(), label: None }).unwrap();
        assert!(pending_schedule(&env, "blog-2").is_none());
    }

    #[tokio::test]
    async fn a_finished_run_schedules_the_next_one_from_its_own_last_run_time() {
        let env = TestEnv::new("fetchers");
        write_source(
            &env,
            "hn-whoishiring",
            json!({"enabled": true, "interval": schedule::MIN_INTERVAL_API_S, "kind": "hn-whoishiring"}),
        );
        let fetchers = Fetchers::default();
        fetchers.load(&env.ctx).unwrap();
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring.");

        fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();

        let state = pending_schedule(&env, "hn-whoishiring").expect("a finished run reschedules the source");
        assert_eq!(state.due_at, env.ctx.clock.now() + chrono::Duration::seconds(schedule::MIN_INTERVAL_API_S as i64));
    }
}
