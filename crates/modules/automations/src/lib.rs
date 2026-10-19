//! The automations module (ADR 0035): "when this happens, do that". A rule reacts to an event
//! (`on`, `when`) by queueing ops (`[[do]]`), each in its own lane as if a person had run it, or by
//! scheduling one for later (`after`). One TOML file per rule; a loop guard follows chains of rules
//! through `Event.cause`. Depends on `core` only (CLAUDE.md §3).

pub mod builtin;
pub mod chain;
pub mod rule;
pub mod template;

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use shimmer_core::params::decode;
use shimmer_core::{local_date, CommandSpec, Ctx, Error, Event, Execution, Manifest, Module, Result, WriteLock};

use chain::{Chain, Chains};
use rule::Rule;

const RULES: &str = "rules";
/// Removed rules, until restored: `removed/<id>.toml`.
const REMOVED: &str = "removed";
const SETTINGS: &str = "settings.toml";
const HISTORY: &str = "history.json";
/// Firings kept in `history.json`, oldest dropped; the event log keeps them all (§8).
const HISTORY_KEPT: usize = 300;
/// After the loop guard reports stopping a rule, further stops of it are quiet this long.
const STOP_QUIET: chrono::Duration = chrono::Duration::minutes(10);
/// The depth limit's default and range (§3).
const DEPTH: usize = 3;
const DEPTH_RANGE: std::ops::RangeInclusive<usize> = 1..=10;
/// A one-time schedule's label is one short line (the scheduler takes up to 200 characters).
const LABEL_CHARS: usize = 150;

/// Every topic automations emits, with a line on it for `automations events` (§5.5).
const TOPICS: [(&str, &str); 9] = [
    ("automations.rule.created", "a rule was created"),
    ("automations.rule.edited", "a rule was changed"),
    ("automations.rule.removed", "a rule was removed (it can be restored)"),
    ("automations.rule.restored", "a removed rule came back"),
    ("automations.rule.enabled", "a rule was switched on"),
    ("automations.rule.disabled", "a rule was switched off"),
    ("automations.rule.fired", "a rule reacted to an event"),
    ("automations.chain.stopped", "a chain of rules was stopped: a loop, or too deep"),
    ("automations.settings.changed", "automations' settings changed"),
];

#[derive(Default)]
pub struct Automations {
    /// Held around every read-check-write of automations' files (ADR 0014).
    write: WriteLock,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    rules: BTreeMap<String, Rule>,
    /// Rule files that couldn't be used, and why: shown, never silently skipped (§1).
    problems: Vec<(String, String)>,
    chains: Chains,
    depth: usize,
    /// When the loop guard last reported stopping each rule, to report it once in a while, not
    /// every time it stops again (see `react`).
    last_stopped: HashMap<String, DateTime<Utc>>,
}

impl Automations {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Re-read every rule file and the settings.
    fn load(&self, ctx: &Ctx) -> Result<()> {
        let mut rules = BTreeMap::new();
        let mut problems = Vec::new();
        for path in ctx.store.list(RULES)? {
            let Some(id) = path.strip_prefix("rules/").and_then(|f| f.strip_suffix(".toml")) else { continue };
            let text = ctx.store.read_string(&path)?.unwrap_or_default();
            match Rule::parse(id, &text, &ctx.catalog, false) {
                Ok(r) => {
                    rules.insert(id.to_owned(), r);
                }
                Err(e) => problems.push((path.clone(), e)),
            }
        }
        let (depth, off, problem) = settings(ctx);
        if let Some(e) = problem {
            problems.push((SETTINGS.to_owned(), e));
        }
        for (id, key, text) in builtin::RULES {
            // A file of yours with a built-in rule's id is reported, not run: the built-in wins.
            if rules.remove(id).is_some() {
                problems.push((path(id), format!("'{id}' is a built-in rule's id: rename your file")));
            }
            // Not loaded when its op isn't installed (notify's, before notify is there).
            if let Ok(mut rule) = Rule::parse(id, text, &ctx.catalog, true) {
                rule.enabled = !off.iter().any(|k| k == key);
                rules.insert(id.to_owned(), rule);
            }
        }
        let mut st = self.state();
        st.rules = rules;
        st.problems = problems;
        st.depth = depth;
        Ok(())
    }

    fn rule(&self, id: &str) -> Result<Rule> {
        self.state().rules.get(id).cloned().ok_or_else(|| Error::not_found(format!("no rule '{id}'")))
    }

    // ------------------------------------------------------------------ reacting

    /// Every matching rule fires (§2), unless the loop guard stops it (§3).
    async fn react(&self, ev: &Event, ctx: &Ctx) -> Result<()> {
        let today = local_date(ctx.clock.now(), &ctx.local_tz);
        let (matched, chain, depth) = {
            let st = self.state();
            let matched: Vec<Rule> = st
                .rules
                .values()
                .filter(|r| r.matches(&ev.topic, &ev.payload, today, ctx.local_tz.tz()))
                .cloned()
                .collect();
            (matched, st.chains.of(ev.cause, &ev.id.to_string()), st.depth)
        };
        // One rule failing (e.g. its history couldn't be written) never stops the others: each is
        // tried, and the failures come back together for the daemon to log (#252's review).
        let mut failures = Vec::new();
        for rule in matched {
            let done = match chain.next(&rule.id, depth) {
                Ok(next) => self.fire(&rule, ev, next, ctx).await,
                // A stop is reported once per rule per STOP_QUIET. Reporting it sets off the built-in
                // alert, whose message is a new task; a rule on `queue.task.*` or `*` reacts to that
                // task in a fresh chain and is stopped again, and a report every time would loop
                // forever, one alert per round (found testing #252's review). Stopped all the same.
                Err(_) if self.recently_stopped(&rule.id, ctx) => Ok(()),
                Err(stop) => {
                    let payload = json!({"chain": chain.id, "rules": chain.rules, "rule": rule.id,
                        "reason": stop.as_str(), "event": {"id": ev.id, "topic": ev.topic}});
                    self.record(ctx, "automations.chain.stopped", payload)
                }
            };
            if let Err(e) = done {
                failures.push(format!("rule '{}': {}", rule.id, e.message));
            }
        }
        match failures.is_empty() {
            true => Ok(()),
            false => Err(Error::internal(failures.join("; "))),
        }
    }

    /// Whether `rule`'s stop was reported in the last [`STOP_QUIET`]; if not, notes it now.
    fn recently_stopped(&self, rule: &str, ctx: &Ctx) -> bool {
        let now = ctx.clock.now();
        let mut st = self.state();
        match st.last_stopped.get(rule) {
            Some(at) if now.signed_duration_since(*at) < STOP_QUIET => true,
            _ => {
                st.last_stopped.insert(rule.to_owned(), now);
                false
            }
        }
    }

    /// Queue or schedule each of `rule`'s actions. One failing doesn't stop the others (§1).
    async fn fire(&self, rule: &Rule, ev: &Event, chain: Chain, ctx: &Ctx) -> Result<()> {
        let mut queued = Vec::new();
        let mut scheduled = Vec::new();
        let mut failed = Vec::new();
        for action in &rule.actions {
            let params = rule::fill(&action.params, &ev.payload);
            match action.after {
                Some(wait) => {
                    // Scheduled as `automations.deferred`, carrying the chain, rather than the op
                    // itself: when it fires, automations queues the op and remembers the chain, so a
                    // delay never starts a fresh chain the loop guard can't see, even across a
                    // restart (ADR 0035 §3, #238's review).
                    let at = ctx.clock.now() + wait;
                    let deferred = json!({"rule": rule.id, "op": action.op, "params": params, "chain": chain});
                    match ctx.schedules.add_once(DEFERRED, deferred, at, &label(rule)) {
                        Ok(id) => scheduled.push(json!({"op": action.op, "trigger_id": id, "at": at})),
                        Err(e) => failed.push(json!({"op": action.op, "error": e.message})),
                    }
                }
                None => match ctx.queue.enqueue(&action.op, params).await {
                    Ok(h) => {
                        self.state().chains.remember(h.task_id, chain.clone());
                        queued.push(json!({"op": action.op, "task_id": h.task_id, "lane": h.lane}));
                    }
                    Err(e) => failed.push(json!({"op": action.op, "error": e.message})),
                },
            }
        }
        let payload = json!({"rule": rule.id, "event": {"id": ev.id, "topic": ev.topic}, "chain": chain.id,
            "depth": chain.rules.len(), "queued": queued, "scheduled": scheduled, "failed": failed});
        self.record(ctx, "automations.rule.fired", payload)
    }

    /// A delayed action's time has come: queue its op, continuing the chain that scheduled it. A
    /// rule removed or switched off meanwhile has nothing left to do.
    async fn deferred(&self, ctx: &Ctx, d: Deferred) -> Result<Value> {
        let on = self.state().rules.get(&d.rule).is_some_and(|r| r.enabled);
        if !on {
            return Ok(json!({"skipped": format!("rule '{}' was removed or switched off", d.rule)}));
        }
        let h = ctx.queue.enqueue(&d.op, d.params).await?;
        self.state().chains.remember(h.task_id, d.chain);
        Ok(json!({"task_id": h.task_id, "op": d.op}))
    }

    /// Emit `topic` and add it to the history, together (§7.1).
    fn record(&self, ctx: &Ctx, topic: &str, payload: Value) -> Result<()> {
        let _g = self.write.lock();
        let mut history = history(ctx);
        let kind = topic.rsplit('.').next().unwrap_or(topic);
        let mut entry = payload.clone();
        entry["at"] = json!(ctx.clock.now());
        entry["kind"] = json!(kind);
        history.push(entry);
        let drop = history.len().saturating_sub(HISTORY_KEPT);
        history.drain(..drop);
        let text = serde_json::to_vec_pretty(&history).map_err(|e| Error::internal(e.to_string()))?;
        ctx.store.transaction(|tx| {
            tx.put(HISTORY, text)?;
            tx.emit(topic, payload)
        })
    }

    // ------------------------------------------------------------------ ops

    fn list(&self, ctx: &Ctx) -> Value {
        let history = history(ctx);
        let rules: Vec<Value> = self.state().rules.values().map(|r| view(r, &history)).collect();
        json!({"rules": rules, "problems": self.problems(), "removed": removed(ctx)})
    }

    fn problems(&self) -> Vec<Value> {
        self.state().problems.iter().map(|(file, error)| json!({"file": file, "error": error})).collect()
    }

    fn get(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let rule = self.rule(id)?;
        let mut v = view(&rule, &history(ctx));
        let text = match builtin::find(id).filter(|_| rule.builtin) {
            Some((_, text)) => text.to_owned(),
            None => ctx.store.read_string(&path(id))?.unwrap_or_default(),
        };
        v["text"] = json!(text);
        Ok(v)
    }

    /// A new rule from `text`, or a starter rule, switched off, to edit (§8).
    fn create(&self, ctx: &Ctx, p: Create) -> Result<Value> {
        if let Some(template) = &p.template {
            if p.text.is_some() || p.label.is_some() {
                return Err(Error::invalid_params("a rule comes from a template or from its text, not both"));
            }
            return self.create_from_template(ctx, &p.id, template, &p.values, p.preview);
        }
        if p.preview || !p.values.is_empty() {
            return Err(Error::invalid_params("values and preview go with a template"));
        }
        let _g = self.write.lock();
        if ctx.store.read(&path(&p.id))?.is_some() {
            return Err(Error::conflict(format!("rule '{}' already exists", p.id)));
        }
        let text = match p.text {
            Some(t) => t,
            None => starter(ctx, p.label.as_deref().unwrap_or("")),
        };
        self.write_rule(ctx, &p.id, &text, "automations.rule.created")
    }

    /// The rules `template` makes with these answers (§9), checked like any rule, made together or
    /// not at all; with `preview`, only shown.
    fn create_from_template(
        &self,
        ctx: &Ctx,
        id: &str,
        template: &str,
        values: &serde_json::Map<String, Value>,
        preview: bool,
    ) -> Result<Value> {
        let t = template::builtin()
            .into_iter()
            .find(|t| t.id == template)
            .ok_or_else(|| Error::not_found(format!("no template '{template}' (shimmer automations templates)")))?;
        let missing: Vec<String> = t.ops().into_iter().filter(|op| ctx.catalog.op(op).is_none()).collect();
        if !missing.is_empty() {
            // Each module named once, however many of its ops the template uses (#254's review).
            let mut modules: Vec<&str> = missing.iter().filter_map(|op| op.split('.').next()).collect();
            modules.sort_unstable();
            modules.dedup();
            let (which, isnt) = if modules.len() == 1 { ("module", "isn't") } else { ("modules", "aren't") };
            return Err(Error::unavailable(format!(
                "the {template} template needs {} from the {} {which}, which {isnt} installed",
                missing.join(", "),
                modules.join(", ")
            )));
        }
        let answers = t.answers(values).map_err(Error::invalid_params)?;
        let rules = t.fill(id, &answers);
        let mut checked = Vec::new();
        for (rule_id, text) in &rules {
            let rule = Rule::parse(rule_id, text, &ctx.catalog, false)
                .map_err(|m| Error::invalid_params(format!("{rule_id}: {m}")))?;
            checked.push(rule);
        }
        let _g = self.write.lock();
        let taken: Vec<&str> = rules
            .iter()
            .map(|(rule_id, _)| rule_id.as_str())
            .filter(|rule_id| builtin::find(rule_id).is_some() || matches!(ctx.store.read(&path(rule_id)), Ok(Some(_))))
            .collect();
        if !taken.is_empty() {
            return Err(Error::conflict(format!("already a rule: {}; choose another id", taken.join(", "))));
        }
        if preview {
            let shown: Vec<Value> = rules.iter().map(|(rule_id, text)| json!({"id": rule_id, "text": text})).collect();
            return Ok(json!({"preview": true, "template": template, "rules": shown}));
        }
        ctx.store.transaction(|tx| {
            for (rule_id, text) in &rules {
                tx.put(&path(rule_id), text.as_str())?;
                tx.emit("automations.rule.created", json!({"rule": rule_id, "template": template}))?;
            }
            Ok(())
        })?;
        let history = history(ctx);
        let mut st = self.state();
        let views: Vec<Value> = checked.iter().map(|r| view(r, &history)).collect();
        for rule in checked {
            st.problems.retain(|(file, _)| *file != path(&rule.id));
            st.rules.insert(rule.id.clone(), rule);
        }
        Ok(json!({"template": template, "rules": views}))
    }

    fn templates(&self, p: TemplatesParams) -> Result<Value> {
        let all = template::builtin();
        let templates: Vec<Value> = match &p.id {
            None => all.iter().map(template::Template::to_wire).collect(),
            Some(id) => vec![all
                .iter()
                .find(|t| &t.id == id)
                .ok_or_else(|| Error::not_found(format!("no template '{id}'")))?
                .to_wire()],
        };
        let categories: Vec<Value> =
            template::CATEGORIES.iter().map(|(id, label)| json!({"id": id, "label": label})).collect();
        Ok(json!({"categories": categories, "templates": templates}))
    }

    /// Replace a rule's file with `text`, after checking it: a broken rule is refused and the old
    /// one kept (§8).
    fn save(&self, ctx: &Ctx, p: Save) -> Result<Value> {
        let _g = self.write.lock();
        if builtin::find(&p.id).is_none() && ctx.store.read(&path(&p.id))?.is_none() {
            return Err(Error::not_found(format!("no rule '{}'", p.id)));
        }
        self.write_rule(ctx, &p.id, &p.text, "automations.rule.edited")
    }

    fn write_rule(&self, ctx: &Ctx, id: &str, text: &str, topic: &str) -> Result<Value> {
        if builtin::find(id).is_some() {
            return Err(Error::conflict(format!(
                "'{id}' is a built-in rule: it can't be edited. Switch it off and make your own instead"
            )));
        }
        let rule = Rule::parse(id, text, &ctx.catalog, false).map_err(Error::invalid_params)?;
        ctx.store.transaction(|tx| {
            tx.put(&path(id), text)?;
            tx.emit(topic, json!({"rule": id}))
        })?;
        let mut st = self.state();
        st.problems.retain(|(file, _)| *file != path(id));
        st.rules.insert(id.to_owned(), rule.clone());
        drop(st);
        Ok(view(&rule, &history(ctx)))
    }

    fn remove(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let _g = self.write.lock();
        let known = self.state().rules.get(id).map(|r| r.builtin);
        if known == Some(true) {
            return Err(Error::conflict(format!("'{id}' is built in: switch it off instead")));
        }
        // A broken rule file can be removed too: that's one way to fix it. Moved, not deleted, so
        // `restore` can bring it back. An older removed rule with the same id is replaced.
        let Some(text) = ctx.store.read(&path(id))? else {
            return Err(Error::not_found(format!("no rule '{id}'")));
        };
        ctx.store.transaction(|tx| {
            tx.put(&removed_path(id), text)?;
            tx.delete(&path(id))?;
            tx.emit("automations.rule.removed", json!({"rule": id}))
        })?;
        let mut st = self.state();
        st.rules.remove(id);
        st.problems.retain(|(file, _)| *file != path(id));
        Ok(json!({"removed": true, "rule": id}))
    }

    /// Bring a removed rule back, as it was. One that no longer checks (an op it names is gone) is
    /// still restored, and reported like any rule file that can't be used.
    fn restore(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let _g = self.write.lock();
        let Some(text) = ctx.store.read_string(&removed_path(id))? else {
            return Err(Error::not_found(format!("no removed rule '{id}'")));
        };
        if builtin::find(id).is_some() {
            return Err(Error::conflict(format!(
                "'{id}' is a built-in rule's id: rename the file in removed/ to restore it"
            )));
        }
        if ctx.store.read(&path(id))?.is_some() {
            return Err(Error::conflict(format!("a rule '{id}' exists: remove or rename it first")));
        }
        ctx.store.transaction(|tx| {
            tx.put(&path(id), text.as_str())?;
            tx.delete(&removed_path(id))?;
            tx.emit("automations.rule.restored", json!({"rule": id}))
        })?;
        let parsed = Rule::parse(id, &text, &ctx.catalog, false);
        let mut st = self.state();
        match parsed {
            Ok(rule) => {
                st.rules.insert(id.to_owned(), rule.clone());
                drop(st);
                Ok(json!({"restored": true, "rule": view(&rule, &history(ctx)), "problem": null}))
            }
            Err(e) => {
                st.problems.push((path(id), e.clone()));
                Ok(json!({"restored": true, "rule": null, "problem": e}))
            }
        }
    }

    /// Switch a rule on or off, keeping the rest of its file as written.
    fn set_enabled(&self, ctx: &Ctx, id: &str, on: bool) -> Result<Value> {
        let _g = self.write.lock();
        let current = self.rule(id)?;
        let topic = if on { "automations.rule.enabled" } else { "automations.rule.disabled" };
        if let Some((key, _)) = builtin::find(id).filter(|_| current.builtin) {
            // A built-in rule is switched in the settings (§7): its text isn't yours to change.
            let mut doc = editable_settings(ctx)?;
            if !doc.get("builtin").is_some_and(toml_edit::Item::is_table) {
                // Missing, or hand-edited into something else (`builtin = true`): a table again,
                // rather than a panic indexing into it (#255's review).
                doc.remove("builtin");
                doc["builtin"] = toml_edit::table();
            }
            doc["builtin"][key] = toml_edit::value(on);
            ctx.store.transaction(|tx| {
                tx.put(SETTINGS, doc.to_string())?;
                tx.emit(topic, json!({"rule": id, "builtin": true}))
            })?;
            let mut rule = current;
            rule.enabled = on;
            self.state().rules.insert(id.to_owned(), rule.clone());
            return Ok(view(&rule, &history(ctx)));
        }
        let text = ctx.store.read_string(&path(id))?.unwrap_or_default();
        let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| Error::internal(e.to_string()))?;
        doc["rule"]["enabled"] = toml_edit::value(on);
        let text = doc.to_string();
        let rule = Rule::parse(id, &text, &ctx.catalog, false).map_err(Error::invalid_params)?;
        ctx.store.transaction(|tx| {
            tx.put(&path(id), text.as_str())?;
            tx.emit(topic, json!({"rule": id}))
        })?;
        self.state().rules.insert(id.to_owned(), rule.clone());
        Ok(view(&rule, &history(ctx)))
    }

    /// What `id` would do for a sample event, without doing it (§8).
    fn test(&self, ctx: &Ctx, p: Test) -> Result<Value> {
        let rule = self.rule(&p.id)?;
        let today = local_date(ctx.clock.now(), &ctx.local_tz);
        // A switched-off rule is tested as if on: you're checking what it would do.
        let mut on = rule.clone();
        on.enabled = true;
        let matched = on.matches(&p.event.topic, &p.event.payload, today, ctx.local_tz.tz());
        let actions: Vec<Value> = match matched {
            false => Vec::new(),
            true => rule
                .actions
                .iter()
                .map(|a| {
                    json!({"op": a.op, "params": rule::fill(&a.params, &p.event.payload),
                           "after_s": a.after.map(|d| d.num_seconds())})
                })
                .collect(),
        };
        Ok(json!({"rule": rule.id, "enabled": rule.enabled, "matches": matched, "actions": actions}))
    }

    fn events(&self, ctx: &Ctx) -> Value {
        let events: Vec<Value> = ctx.catalog.topics().map(|(t, d)| json!({"topic": t, "description": d})).collect();
        json!({"events": events})
    }

    fn history(&self, ctx: &Ctx, p: HistoryParams) -> Value {
        let limit = p.limit.unwrap_or(50).min(HISTORY_KEPT);
        let entries: Vec<Value> = history(ctx)
            .into_iter()
            .rev()
            .filter(|e| p.id.as_deref().is_none_or(|id| e["rule"] == id))
            .filter(|e| p.since.is_none_or(|since| at(e).is_some_and(|at| at >= since)))
            .take(limit)
            .collect();
        json!({"history": entries})
    }

    fn status(&self, ctx: &Ctx) -> Value {
        let stopped: Vec<Value> = history(ctx).into_iter().rev().filter(|e| e["kind"] == "stopped").take(5).collect();
        let st = self.state();
        let on = st.rules.values().filter(|r| r.enabled).count();
        json!({"rules": st.rules.len(), "enabled": on, "depth": st.depth, "stopped": stopped,
               "problems": st.problems.iter().map(|(f, e)| json!({"file": f, "error": e})).collect::<Vec<_>>()})
    }

    fn depth(&self, ctx: &Ctx, p: DepthParams) -> Result<Value> {
        let Some(depth) = p.depth else { return Ok(json!({"depth": self.state().depth})) };
        if !DEPTH_RANGE.contains(&depth) {
            return Err(Error::invalid_params(format!("depth is {} to {}", DEPTH_RANGE.start(), DEPTH_RANGE.end())));
        }
        let _g = self.write.lock();
        let mut doc = editable_settings(ctx)?;
        doc["depth"] = toml_edit::value(depth as i64);
        ctx.store.transaction(|tx| {
            tx.put(SETTINGS, doc.to_string())?;
            tx.emit("automations.settings.changed", json!({"depth": depth}))
        })?;
        let mut st = self.state();
        st.depth = depth;
        st.problems.retain(|(file, _)| file != SETTINGS);
        Ok(json!({"depth": depth}))
    }

    /// ADR 0033's shape: unusable files, rules naming ops that are gone, recent stopped loops.
    fn health(&self, ctx: &Ctx) -> Value {
        let mut checks = Vec::new();
        for (file, error) in &self.state().problems {
            checks.push(json!({"status": "fail", "subject": file, "message": format!("{file}: {error}"),
                "fix": {"summary": "fix the file and run `automations.reload`, or remove the rule"}}));
        }
        let stopped = history(ctx).into_iter().rev().filter(|e| e["kind"] == "stopped").take(1).next();
        if let Some(s) = stopped {
            let rule = s["rule"].as_str().unwrap_or("?").to_owned();
            checks.push(json!({"status": "warn", "subject": rule,
                "message": format!("a loop was stopped at rule '{rule}' ({})", s["reason"].as_str().unwrap_or("?")),
                "fix": {"summary": "see what happened", "op": "automations.history", "params": {"id": rule}}}));
        }
        if checks.is_empty() {
            let n = self.state().rules.len();
            checks.push(
                json!({"status": "ok", "message": format!("{n} rule{}, all fine", if n == 1 { "" } else { "s" })}),
            );
        }
        json!({"checks": checks})
    }
}

#[async_trait]
impl Module for Automations {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "automations".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            namespace: "automations".into(),
            topics: TOPICS.iter().map(|(t, _)| t.to_string()).collect(),
            topic_descriptions: TOPICS.iter().map(|(t, d)| (t.to_string(), d.to_string())).collect(),
            capabilities: vec![],
        }
    }

    async fn init(&self, ctx: &Ctx) -> Result<()> {
        self.load(ctx)
    }

    fn commands(&self) -> Vec<CommandSpec> {
        let id = || json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}});
        [
            (
                "automations.list",
                "Every rule: on or off, what it reacts to, when it last fired",
                json!({"type": "object"}),
            ),
            ("automations.get", "One rule, and its file's text", id()),
            (
                "automations.create",
                "Make a rule from its text, from a template's answers, or a starter rule (switched off) to edit",
                json!({"type": "object", "required": ["id"], "properties": {
                    "id": {"type": "string"}, "text": {"type": "string"}, "label": {"type": "string"},
                    "template": {"type": "string"}, "values": {"type": "object"}, "preview": {"type": "boolean"}}}),
            ),
            (
                "automations.templates",
                "Ready-made rules to make from a few answers, by category",
                json!({"type": "object", "properties": {"id": {"type": "string"}}}),
            ),
            (
                "automations.save",
                "Replace a rule's text; a broken rule is refused and the old one kept",
                json!({"type": "object", "required": ["id", "text"], "properties": {
                    "id": {"type": "string"}, "text": {"type": "string"}}}),
            ),
            ("automations.remove", "Remove a rule; it can be restored", id()),
            ("automations.restore", "Bring a removed rule back", id()),
            ("automations.enable", "Switch a rule on", id()),
            ("automations.disable", "Switch a rule off", id()),
            ("automations.reload", "Re-read the rule files after editing them by hand", json!({"type": "object"})),
            (
                "automations.test",
                "What a rule would do for a sample event, without doing it",
                json!({"type": "object", "required": ["id", "event"], "properties": {
                    "id": {"type": "string"},
                    "event": {"type": "object", "required": ["topic"], "properties": {
                        "topic": {"type": "string"}, "payload": {"type": "object"}}}}}),
            ),
            ("automations.events", "Everything a rule can react to, with what it means", json!({"type": "object"})),
            (
                DEFERRED,
                "Internal: run a rule's delayed action when its time comes (rules can't call it)",
                json!({"type": "object", "required": ["rule", "op", "chain"]}),
            ),
            (
                "automations.history",
                "What rules did recently, newest first",
                json!({"type": "object", "properties": {
                    "id": {"type": "string"}, "since": {"type": "string", "format": "date-time"},
                    "limit": {"type": "integer", "minimum": 1}}}),
            ),
            ("automations.status", "Rules, the depth limit, stopped loops, broken files", json!({"type": "object"})),
            (
                "automations.depth",
                "Show or set how many rules may follow each other (1-10)",
                json!({"type": "object", "properties": {"depth": {"type": "integer", "minimum": 1, "maximum": 10}}}),
            ),
            (
                "automations.health",
                "Check the rules for problems; changes nothing (ADR 0033)",
                json!({"type": "object"}),
            ),
        ]
        .into_iter()
        .map(|(op, summary, params_schema)| CommandSpec {
            op: op.into(),
            summary: summary.into(),
            params_schema,
            execution: Execution::Inline,
        })
        .collect()
    }

    async fn handle(&self, op: &str, params: Value, ctx: &Ctx) -> Result<Value> {
        match op {
            "automations.list" => Ok(self.list(ctx)),
            "automations.get" => self.get(ctx, &decode::<Id>(params)?.id),
            "automations.create" => self.create(ctx, decode(params)?),
            "automations.save" => self.save(ctx, decode(params)?),
            "automations.remove" => self.remove(ctx, &decode::<Id>(params)?.id),
            "automations.restore" => self.restore(ctx, &decode::<Id>(params)?.id),
            "automations.enable" => self.set_enabled(ctx, &decode::<Id>(params)?.id, true),
            "automations.disable" => self.set_enabled(ctx, &decode::<Id>(params)?.id, false),
            "automations.reload" => {
                self.load(ctx)?;
                Ok(json!({"rules": self.state().rules.len(), "problems": self.problems()}))
            }
            "automations.test" => self.test(ctx, decode(params)?),
            "automations.events" => Ok(self.events(ctx)),
            "automations.templates" => self.templates(decode(params)?),
            "automations.history" => Ok(self.history(ctx, decode(params)?)),
            "automations.status" => Ok(self.status(ctx)),
            "automations.depth" => self.depth(ctx, decode(params)?),
            "automations.health" => Ok(self.health(ctx)),
            DEFERRED => self.deferred(ctx, decode(params)?).await,
            _ => Err(Error::unknown_op(op)),
        }
    }

    async fn on_event(&self, ev: &Event, ctx: &Ctx) -> Result<()> {
        self.react(ev, ctx).await
    }
}

// ---------------------------------------------------------------------- params

/// Rules can't name `automations.*` ops, so only automations itself schedules this one.
const DEFERRED: &str = "automations.deferred";

/// What a delayed action carries until it runs. `scheduled_for` is the scheduler's stamp.
#[derive(Deserialize)]
struct Deferred {
    rule: String,
    op: String,
    #[serde(default)]
    params: Value,
    chain: Chain,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Id {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    id: String,
    text: Option<String>,
    label: Option<String>,
    template: Option<String>,
    #[serde(default)]
    values: serde_json::Map<String, Value>,
    /// With a template: the rules it would make, without making them.
    #[serde(default)]
    preview: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplatesParams {
    id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Save {
    id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Test {
    id: String,
    event: SampleEvent,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SampleEvent {
    topic: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryParams {
    id: Option<String>,
    since: Option<DateTime<Utc>>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DepthParams {
    depth: Option<usize>,
}

// ---------------------------------------------------------------------- helpers

fn path(id: &str) -> String {
    format!("{RULES}/{id}.toml")
}

fn removed_path(id: &str) -> String {
    format!("{REMOVED}/{id}.toml")
}

/// The ids of removed rules, to restore.
fn removed(ctx: &Ctx) -> Vec<String> {
    let files = ctx.store.list(REMOVED).unwrap_or_default();
    files
        .iter()
        .filter_map(|f| f.strip_prefix("removed/").and_then(|f| f.strip_suffix(".toml")))
        .map(str::to_owned)
        .collect()
}

/// `settings.toml`: the depth limit (the default when there's no file or no `depth`), and the
/// `[builtin]` keys switched off (§7).
/// The depth limit, the built-ins switched off, and what's wrong with the file, if anything. A bad
/// `depth` falls back alone: the switched-off built-ins are still read (#255's review).
fn settings(ctx: &Ctx) -> (usize, Vec<String>, Option<String>) {
    let text = match ctx.store.read_string(SETTINGS) {
        Ok(Some(text)) => text,
        Ok(None) => return (DEPTH, Vec::new(), None),
        Err(e) => return (DEPTH, Vec::new(), Some(e.message)),
    };
    let table: toml::Table = match toml::from_str(&text) {
        Ok(t) => t,
        Err(e) => return (DEPTH, Vec::new(), Some(format!("not a settings file: {}", e.message()))),
    };
    let off: Vec<String> = table
        .get("builtin")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
        .filter(|(_, on)| on.as_bool() == Some(false))
        .map(|(k, _)| k.clone())
        .collect();
    match table.get("depth") {
        None => (DEPTH, off, None),
        Some(v) => match v.as_integer().and_then(|n| usize::try_from(n).ok()) {
            Some(n) if DEPTH_RANGE.contains(&n) => (n, off, None),
            _ => {
                (DEPTH, off, Some(format!("depth is {} to {}, using {DEPTH}", DEPTH_RANGE.start(), DEPTH_RANGE.end())))
            }
        },
    }
}

/// `settings.toml` to change, comments kept. One that doesn't parse is refused, never replaced:
/// it's hand-editable, and writing a fresh one would lose the person's other settings (#255's
/// review).
fn editable_settings(ctx: &Ctx) -> Result<toml_edit::DocumentMut> {
    let text = ctx.store.read_string(SETTINGS)?.unwrap_or_default();
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::invalid_params(format!("{SETTINGS} doesn't parse: {}; fix it first", e.to_string().trim())))
}

/// The history, oldest first. A broken file reads as empty: it's a convenience, the event log
/// has everything.
fn history(ctx: &Ctx) -> Vec<Value> {
    ctx.store
        .read_string(HISTORY)
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok())
        .unwrap_or_default()
}

fn at(entry: &Value) -> Option<DateTime<Utc>> {
    entry["at"].as_str()?.parse().ok()
}

/// A rule as `list`, `get` and the rest return it.
fn view(r: &Rule, history: &[Value]) -> Value {
    let mine = |kind: &str| history.iter().rev().find(|e| e["rule"] == r.id && e["kind"] == kind).and_then(at);
    let actions: Vec<Value> = r
        .actions
        .iter()
        .map(|a| json!({"op": a.op, "params": a.params, "after_s": a.after.map(|d| d.num_seconds())}))
        .collect();
    json!({"id": r.id, "label": r.label, "enabled": r.enabled, "builtin": r.builtin, "on": r.on,
           "actions": actions, "last_fired": mine("fired"), "loop_stopped_at": mine("stopped")})
}

/// What `scheduler list` shows for a delayed action: the rule it came from.
fn label(r: &Rule) -> String {
    let text = format!("rule {}: {}", r.id, r.label);
    match text.char_indices().nth(LABEL_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// A starter rule (§8), switched off, that checks: it names an event and an op that exist.
fn starter(ctx: &Ctx, label: &str) -> String {
    let topic = ["records.item.completed"]
        .into_iter()
        .find(|t| ctx.catalog.has_topic(t))
        .map(str::to_owned)
        .or_else(|| ctx.catalog.topics().map(|(t, _)| t.to_owned()).find(|t| !t.starts_with("automations.")))
        .unwrap_or_else(|| "*".into());
    let op = ["notify.send"]
        .into_iter()
        .find(|o| ctx.catalog.op(o).is_some())
        .map(str::to_owned)
        .or_else(|| ctx.catalog.ops().map(|c| c.op.clone()).find(|o| !o.starts_with("automations.")))
        .unwrap_or_default();
    let label = if label.trim().is_empty() { "My rule" } else { label.trim() };
    format!(
        r#"# A rule: when an event happens (on, when), run ops (each [[do]]). Switched off until you
# turn it on: shimmer automations enable <id>. Events: shimmer automations events.
[rule]
label = {label:?}
enabled = false
on = "{topic}"                     # a topic or a pattern: records.item.*, *
# when = {{ collection = "jobs" }}  # conditions on the event, as in records' filters

[[do]]
op = "{op}"                        # any op except automations.*
params = {{ title = "Done: {{title}}" }}   # {{field}} is filled from the event
# after = "3d"                     # wait first: 30m, 2h, 3d, 1w
"#
    )
}
