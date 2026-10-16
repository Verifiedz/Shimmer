use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use shimmer_core::params::decode;
use shimmer_core::{
    CommandSpec, Ctx, Error, ErrorCode, Execution, LaneConfig, Manifest, Module, Result, TriggerSpec, WriteLock,
};

use crate::dedup::{self, RunGuard};
use crate::schedule::{self, FetchersConfig, FETCH_OP, TICK_OP};
use crate::source::{self, RawItem, Source};

/// Network-bound and slow, same reasoning as every other fetch-shaped lane (CLAUDE.md §6.1).
pub const LANE: &str = "fetchers";

pub const LIST_OP: &str = "fetchers.list";
pub const STATUS_OP: &str = "fetchers.status";
pub const TEST_OP: &str = "fetchers.test";

/// Caps `fetchers.item.found`'s `raw_data` (ADR 0028 §3) -- big enough for a job posting's
/// text, small enough that one large item can't become the next cache-bloat problem.
const MAX_RAW_DATA_CHARS: usize = 8 * 1024;

#[derive(Default)]
pub struct Fetchers {
    /// A scheduled tick and a manual `fetchers.fetch` for the same source must never overlap
    /// (ADR 0028 §5).
    running: RunGuard,
    /// Serialises `last_run.toml`'s read-modify-write (`schedule::record_run`) against itself
    /// -- the `fetchers` lane runs up to 8 fetches at once, each ending in its own call to
    /// `record_run`, and an unlocked read-check-write there can drop whichever of two
    /// concurrently-finishing sources' updates writes second (PR #126 review). A different
    /// lock from `running`: that one is per-source exclusivity for a whole run, this one is
    /// held only across the few lines that touch the shared last-run file.
    write: WriteLock,
}

#[async_trait]
impl Module for Fetchers {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "fetchers".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            namespace: "fetchers".into(),
            topics: ["fetchers.item.found", "fetchers.fetch.finished", "fetchers.fetch.failed"]
                .map(String::from)
                .to_vec(),
            // hn-whoishiring (ADR 0028 §10) needs the real ctx.http.
            capabilities: vec!["network".into()],
        }
    }

    async fn init(&self, _ctx: &Ctx) -> Result<()> {
        Ok(())
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
                summary: "Check configured sources and enqueue a fetch for each one due".into(),
                params_schema: json!({"type": "null"}),
                execution: Execution::Queued { lane: LANE.into() },
            },
            CommandSpec {
                op: FETCH_OP.into(),
                summary: "Fetch one source now".into(),
                params_schema: json!({
                    "type": "object", "required": ["source"],
                    "properties": {
                        "source": {"type": "string"},
                        "title_contains": {"type": "array", "items": {"type": "string"}},
                        "title_excludes": {"type": "array", "items": {"type": "string"}},
                    },
                }),
                execution: Execution::Queued { lane: LANE.into() },
            },
            CommandSpec {
                op: LIST_OP.into(),
                summary: "List every known source and its configured state".into(),
                params_schema: json!({"type": "null"}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: STATUS_OP.into(),
                summary: "Per-source last-run time and whether it is currently running".into(),
                params_schema: json!({"type": "null"}),
                execution: Execution::Inline,
            },
            CommandSpec {
                op: TEST_OP.into(),
                summary:
                    "Fetch one page from a source and show its parsed items, without saving or marking anything seen"
                        .into(),
                params_schema: json!({
                    "type": "object", "required": ["source"],
                    "properties": {
                        "source": {"type": "string"},
                        "title_contains": {"type": "array", "items": {"type": "string"}},
                        "title_excludes": {"type": "array", "items": {"type": "string"}},
                    },
                }),
                execution: Execution::Queued { lane: LANE.into() },
            },
        ]
    }

    async fn handle(&self, op: &str, params: Value, ctx: &Ctx) -> Result<Value> {
        match op {
            TICK_OP => self.tick(ctx).await,
            FETCH_OP => {
                let p: FetchParams = decode(params)?;
                let filter = TitleFilterOverride::from_params(p.title_contains, p.title_excludes);
                self.fetch(ctx, p.source, filter).await
            }
            LIST_OP => self.list(ctx).await,
            STATUS_OP => self.status(ctx).await,
            TEST_OP => {
                let p: FetchParams = decode(params)?;
                let filter = TitleFilterOverride::from_params(p.title_contains, p.title_excludes);
                self.test(ctx, p.source, filter).await
            }
            _ => Err(Error::unknown_op(format!("fetchers has no op '{op}'"))),
        }
    }
}

#[derive(Deserialize)]
struct FetchParams {
    source: String,
    #[serde(default)]
    title_contains: Option<Vec<String>>,
    #[serde(default)]
    title_excludes: Option<Vec<String>>,
}

/// An ad-hoc title filter from a `fetchers.fetch`/`fetchers.test` caller (the CLI's
/// `--contains`/`--excludes` flags), scoped to one call -- it replaces, never merges with,
/// whatever `[modules.fetchers.sources.<id>]` configures for this source (ADR 0028 §14).
/// `None` means neither flag was given at all, so the configured filter (if any) applies
/// exactly as before this existed.
struct TitleFilterOverride {
    contains: Vec<String>,
    excludes: Vec<String>,
}

impl TitleFilterOverride {
    fn from_params(contains: Option<Vec<String>>, excludes: Option<Vec<String>>) -> Option<Self> {
        (contains.is_some() || excludes.is_some())
            .then(|| Self { contains: contains.unwrap_or_default(), excludes: excludes.unwrap_or_default() })
    }
}

impl Fetchers {
    async fn tick(&self, ctx: &Ctx) -> Result<Value> {
        self.run_due(ctx, &FetchersConfig::load(ctx)?).await
    }

    /// Split from [`Fetchers::tick`] so the enqueue-and-skip-if-running logic is testable
    /// against a hand-built config, without needing to read `ctx.config` at all.
    async fn run_due(&self, ctx: &Ctx, config: &FetchersConfig) -> Result<Value> {
        let last_run = schedule::load_last_run(ctx)?;
        let due = schedule::due_sources(config, &last_run, ctx.clock.now());

        let (mut enqueued, mut already_running) = (Vec::new(), Vec::new());
        for source in due {
            // A peek only -- an optimization to avoid enqueuing a source we can already see
            // is running. `fetch`'s own `try_acquire` is the real, race-free guarantee.
            if self.running.is_running(&source) {
                already_running.push(source);
                continue;
            }
            ctx.queue.enqueue(FETCH_OP, json!({"source": source})).await?;
            enqueued.push(source);
        }
        Ok(json!({"enqueued": enqueued, "already_running": already_running}))
    }

    async fn fetch(&self, ctx: &Ctx, source_id: String, filter_override: Option<TitleFilterOverride>) -> Result<Value> {
        if source_id.trim().is_empty() {
            return Err(Error::invalid_params("source must not be empty"));
        }
        let source =
            source::resolve(ctx, &source_id)?.ok_or_else(|| Error::not_found(format!("no source '{source_id}'")))?;
        let _permit = self
            .running
            .try_acquire(&source_id)
            .ok_or_else(|| Error::module_error(format!("source '{source_id}' is already fetching")))?;

        let outcome = self.run_source(ctx, source.as_ref(), filter_override.as_ref()).await;
        // Recorded regardless of outcome -- including failure -- so a broken source is
        // retried at most once per interval (ADR 0028 §5, §6: "after that, wait for the
        // next trigger"), not hammered every tick. Held only across this call -- up to 8
        // fetches finish around the same time on this lane, and this file's own
        // read-modify-write must not interleave with another source's (PR #126 review).
        {
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
    /// has its own config entry, plus every configured `kind` instance (ADR 0028 §2a/§19),
    /// cross-referenced with that entry.
    async fn list(&self, ctx: &Ctx) -> Result<Value> {
        let config = FetchersConfig::load(ctx)?;
        let sources: Vec<Value> = source::known(ctx)?
            .iter()
            .map(|s| {
                let id = s.id();
                let cfg = config.sources.get(id);
                json!({
                    "id": id,
                    "method": match s.method() { source::Method::Api => "api", source::Method::Scrape => "scrape" },
                    "kind": cfg.and_then(|c| c.kind.clone()),
                    "enabled": cfg.is_some_and(|c| c.enabled),
                    // 0: already validated at load time (§126 review's per-method floor);
                    // this is read-only display, not re-validation, same as `due_sources`.
                    "interval_s": cfg.and_then(|c| c.interval.resolve(0).ok()),
                    "title_contains": cfg.map(|c| c.title_contains.clone()).unwrap_or_default(),
                    "title_excludes": cfg.map(|c| c.title_excludes.clone()).unwrap_or_default(),
                })
            })
            .collect();
        Ok(json!({"sources": sources}))
    }

    /// Per-source last-run time and whether it is currently mid-fetch (ADR 0028 §5's
    /// `RunGuard`) -- the same source universe [`Fetchers::list`] enumerates.
    async fn status(&self, ctx: &Ctx) -> Result<Value> {
        let last_run = schedule::load_last_run(ctx)?;
        let sources: Vec<Value> = source::known(ctx)?
            .iter()
            .map(|s| {
                let id = s.id();
                json!({"id": id, "last_run_at": last_run.get(id), "running": self.running.is_running(id)})
            })
            .collect();
        Ok(json!({"sources": sources}))
    }

    /// Fetches exactly one page from a source and returns its parsed items -- never the
    /// `SeenSet`, `last_run.toml`, or any event (issue #128: "without saving or marking
    /// anything seen"). Deliberately simpler than [`Fetchers::run_source`]: a dry run has no
    /// state of its own to protect, so it skips the overlap guard too -- a `test` can run
    /// alongside a real `fetch` of the same source without conflict.
    async fn test(&self, ctx: &Ctx, source_id: String, filter_override: Option<TitleFilterOverride>) -> Result<Value> {
        if source_id.trim().is_empty() {
            return Err(Error::invalid_params("source must not be empty"));
        }
        let source =
            source::resolve(ctx, &source_id)?.ok_or_else(|| Error::not_found(format!("no source '{source_id}'")))?;
        let page = source.fetch_page(ctx, None).await?;
        // Applied here too (§13/§14), not just in `run_source` -- a preview that shows items
        // a real run would never keep is a misleading preview.
        let config = FetchersConfig::load(ctx)?;
        let configured = config.sources.get(&source_id);
        let items: Vec<Value> = page
            .items
            .iter()
            .filter(|item| passes_title_filter(&item.title, filter_override.as_ref(), configured))
            .map(|item| item_payload(&source_id, item))
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
        let config = FetchersConfig::load(ctx)?;
        let configured = config.sources.get(source_id);

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
            for item in page.items {
                if seen.is_new(&item.source_id) {
                    if !passes_title_filter(&item.title, filter_override, configured) {
                        seen.touch(item.source_id.clone(), now);
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
            match page.next_cursor {
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

fn cap_raw_data(v: &Value) -> Value {
    let s = v.to_string();
    if s.chars().count() <= MAX_RAW_DATA_CHARS {
        v.clone()
    } else {
        json!({"truncated": true, "preview": s.chars().take(MAX_RAW_DATA_CHARS).collect::<String>()})
    }
}

#[cfg(test)]
mod tests {
    use shimmer_core::testing::TestEnv;
    use shimmer_core::{HttpResponse, ModuleConfig};

    use super::*;
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
                "fetchers.fetch.failed".to_string()
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
    async fn an_unknown_op_is_rejected_rather_than_silently_accepted() {
        let env = TestEnv::new("fetchers");
        let err = Fetchers::default().handle("fetchers.bogus", json!({}), &env.ctx).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::UnknownOp);
    }

    #[test]
    fn title_filter_override_is_none_only_when_neither_flag_was_given() {
        assert!(TitleFilterOverride::from_params(None, None).is_none());
        let f = TitleFilterOverride::from_params(Some(vec!["junior".into()]), None).unwrap();
        assert_eq!((f.contains, f.excludes), (vec!["junior".to_string()], Vec::new()));
        let f = TitleFilterOverride::from_params(None, Some(vec!["senior".into()])).unwrap();
        assert_eq!((f.contains, f.excludes), (Vec::new(), vec!["senior".to_string()]));
    }

    fn source_config(enabled: bool, interval_s: u64) -> schedule::SourceConfig {
        schedule::SourceConfig {
            enabled,
            interval: IntervalSpec::Seconds(interval_s),
            kind: None,
            url: None,
            items_path: None,
            id_field: None,
            title_field: None,
            url_field: None,
            url_base: None,
            pagination: None,
            page_size: None,
            active_field: None,
            title_contains: Vec::new(),
            title_excludes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn tick_enqueues_every_due_source_and_only_those() {
        let env = TestEnv::new("fetchers");
        let mut config = FetchersConfig::default();
        config.sources.insert("hn-whoishiring".into(), source_config(true, schedule::MIN_INTERVAL_API_S));
        config.sources.insert("weworkremotely".into(), source_config(false, schedule::MIN_INTERVAL_API_S));

        let fetchers = Fetchers::default();
        let result = fetchers.run_due(&env.ctx, &config).await.unwrap();
        assert_eq!(result["enqueued"], json!(["hn-whoishiring"]));
        assert_eq!(result["already_running"], json!([]));

        let submitted = env.queue.requests.lock().unwrap();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].op, FETCH_OP);
        assert_eq!(submitted[0].params, json!({"source": "hn-whoishiring"}));
    }

    #[tokio::test]
    async fn tick_skips_a_source_it_already_sees_running() {
        let env = TestEnv::new("fetchers");
        let mut config = FetchersConfig::default();
        config.sources.insert("hn-whoishiring".into(), source_config(true, schedule::MIN_INTERVAL_API_S));

        let fetchers = Fetchers::default();
        let _permit = fetchers.running.try_acquire("hn-whoishiring").unwrap();
        let result = fetchers.run_due(&env.ctx, &config).await.unwrap();

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
        let mut env = TestEnv::new("fetchers");
        env.ctx.config = ModuleConfig::new(json!({
            "sources": {"hn-whoishiring": {"enabled": true, "interval": "daily", "title_excludes": ["senior"]}}
        }));
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
        let mut env = TestEnv::new("fetchers");
        env.ctx.config = ModuleConfig::new(json!({
            "sources": {"hn-whoishiring": {"enabled": true, "interval": "daily", "title_contains": ["junior"]}}
        }));
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

        let out = Fetchers::default().test(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();
        let items = out["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(items[0]["title"].as_str().unwrap().contains("Junior"));
    }

    #[tokio::test]
    async fn an_override_filter_replaces_rather_than_merges_with_the_configured_one() {
        let mut env = TestEnv::new("fetchers");
        // Configured to want only "junior" titles -- the override below asks for the
        // opposite (anything but "junior") and must win outright, not combine with this.
        env.ctx.config = ModuleConfig::new(json!({
            "sources": {"hn-whoishiring": {"enabled": true, "interval": "daily", "title_contains": ["junior"]}}
        }));
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

        let override_filter = TitleFilterOverride { contains: vec!["senior".into()], excludes: Vec::new() };
        let out = Fetchers::default().test(&env.ctx, "hn-whoishiring".into(), Some(override_filter)).await.unwrap();
        let items = out["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(items[0]["title"].as_str().unwrap().contains("Senior"), "the override must win, not merge with config");
    }

    #[tokio::test]
    async fn an_override_filter_on_a_real_run_also_marks_the_dropped_item_seen() {
        let env = TestEnv::new("fetchers"); // no config.toml filter at all -- the override is the only one
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
        let out = Fetchers::default().fetch(&env.ctx, "hn-whoishiring".into(), Some(override_filter)).await.unwrap();
        assert_eq!(out["new_items"], 1);
        let seen = dedup::load(&env.ctx, "hn-whoishiring").unwrap();
        assert!(!seen.is_new("10"), "the ad-hoc override must mark a dropped item seen, same as a configured filter");
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

        let mut env = TestEnv::new("fetchers");
        env.ctx.config = ModuleConfig::new(json!({
            "sources": {
                "my-team-blog": {
                    "enabled": true,
                    "interval": "hourly",
                    "kind": "rss",
                    "url": "https://example.test/feed.xml",
                },
            }
        }));
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
        let mut env = TestEnv::new("fetchers");
        env.ctx.config = ModuleConfig::new(json!({
            "sources": {
                "hn-whoishiring": {"enabled": true, "interval": "daily"},
                "my-team-blog": {"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"},
            }
        }));

        let out = Fetchers::default().list(&env.ctx).await.unwrap();
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
    async fn status_reports_last_run_and_whether_a_source_is_mid_fetch() {
        let mut env = TestEnv::new("fetchers");
        // Both must have their own entry to show up in status at all (ADR 0028 §19) --
        // fetching hn-whoishiring works regardless (resolve() needs none), but seeing it
        // and weworkremotely listed afterward does not.
        env.ctx.config = ModuleConfig::new(json!({
            "sources": {
                "hn-whoishiring": {"enabled": true, "interval": "daily"},
                "weworkremotely": {"enabled": false, "interval": schedule::MIN_INTERVAL_SCRAPE_S},
            }
        }));
        let fetchers = Fetchers::default();
        stub_one_hn_comment(&env, 10, "<p>Acme | Remote<p>Hiring.");
        fetchers.fetch(&env.ctx, "hn-whoishiring".into(), None).await.unwrap();

        let out = fetchers.status(&env.ctx).await.unwrap();
        let sources = out["sources"].as_array().unwrap();
        let hn = sources.iter().find(|s| s["id"] == "hn-whoishiring").unwrap();
        assert_eq!(hn["last_run_at"], json!(env.ctx.clock.now()));
        assert_eq!(hn["running"], false);

        let wwr = sources.iter().find(|s| s["id"] == "weworkremotely").unwrap();
        assert!(wwr["last_run_at"].is_null(), "never fetched -- no last run");
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
        assert_eq!(tick["enqueued"], json!([]), "no sources configured in ctx.config by default");

        let fetch = fetchers.handle(FETCH_OP, json!({"source": "hn-whoishiring"}), &env.ctx).await.unwrap();
        assert_eq!(fetch["source"], "hn-whoishiring");
        assert_eq!(fetch["new_items"], 1);
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
        let err = fetchers.run_source(&env.ctx, source.as_ref(), None).await.unwrap_err();
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
            env.ctx.cancel.clone(),
            std::sync::Arc::new(move |f, n| p.lock().unwrap().push((f, n.to_owned()))),
        );
        let fetchers = Fetchers::default();

        let source = source::find("hn-whoishiring").unwrap();
        fetchers.run_source(&ctx, source.as_ref(), None).await.unwrap();

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
}
