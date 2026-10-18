//! The `workspaces` module (CLAUDE.md §10): everything that touches `ctx`. Decisions are made
//! by the pure parts ([`crate::state`], [`crate::manifest`], [`crate::persist`]); this file
//! reads files, calls them, and writes the result with its event in one transaction (§7.1).
//!
//! Layout, inside the module's namespace `data/workspaces/` (ADR 0010, ADR 0012):
//! `<id>/workspace.toml`, `<id>/steps/…`, `<id>/cleanup.{sh,ps1}` (all written by the user)
//! and `<id>/state.toml` (written only here).
//!
//! The ops that run scripts (`activate`, `force_relaunch`, `cleanup`) live in [`crate::launch`].

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use shimmer_core::params::decode;
use shimmer_core::{CommandSpec, Ctx, Error, Execution, LaneConfig, Manifest, Module, Result, SpawnMode, WriteLock};

use crate::builtin;
use crate::manifest::{self, Workspace};
use crate::persist::{self, Saved, STATE_FILE};
use crate::state::{self, WorkspaceState};
use crate::template::{Template, CATEGORIES};

/// Launches touch shared state (a desktop, a terminal multiplexer), so they run one at a time
/// (CLAUDE.md §6.1).
pub const LANE: &str = "workspaces";

#[derive(Default)]
pub struct Workspaces {
    /// Inline requests run concurrently; this serialises each read-check-write of a
    /// `state.toml`. Never held across an `.await`.
    pub(crate) write: WriteLock,
}

#[async_trait]
impl Module for Workspaces {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "workspaces".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            namespace: "workspaces".into(),
            topics: [
                "workspaces.session.launched",
                "workspaces.session.dirty",
                "workspaces.session.forced",
                "workspaces.session.cleaned",
                "workspaces.session.abandoned",
                "workspaces.workspace.reset",
                "workspaces.workspace.created",
                "workspaces.session.stopped",
                "workspaces.workspace.removed",
                "workspaces.workspace.restored",
                "workspaces.workspace.purged",
                "workspaces.workspace.copied",
                "workspaces.workspace.renamed",
                "workspaces.workspace.reconfigured",
            ]
            .map(String::from)
            .to_vec(),
            // Gets the real `ctx.launcher` (ADR 0010 §4).
            capabilities: vec!["process".into()],
        }
    }

    /// A workspace still saved as `launching` belongs to a launch the daemon never finished
    /// (it crashed or was killed). Mark it dirty: its earlier steps already ran.
    async fn init(&self, ctx: &Ctx) -> Result<()> {
        let _g = self.write.lock();
        for (id, folder) in folders(ctx)? {
            let Some(Ok(saved)) = folder.saved(ctx, &id) else { continue };
            let Some(dirty) = persist::recover_after_restart(&saved, ctx.clock.now()) else { continue };
            ctx.store.transaction(|tx| {
                let next = Saved::new(dirty.clone()).with_last_session(saved.last_session.clone());
                tx.put(&state_path(&id), persist::encode(&next))?;
                // `recovered`: no task failed to report this one, so notify alerts on it
                // (ADR 0032 §4); a launch that fails as it runs is reported by its task.
                let mut payload = dirty_payload(&id, &dirty);
                payload["recovered"] = json!(true);
                tx.emit("workspaces.session.dirty", payload)
            })?;
        }
        Ok(())
    }

    fn lanes(&self) -> Vec<LaneConfig> {
        vec![LaneConfig::new(LANE, 1)]
    }

    fn commands(&self) -> Vec<CommandSpec> {
        let id = json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}});
        let queued = || Execution::Queued { lane: LANE.into() };
        [
            ("workspaces.list", "List workspaces and their state", json!({"type": "object"}), Execution::Inline),
            ("workspaces.status", "Show one workspace in full", id.clone(), Execution::Inline),
            ("workspaces.activate", "Launch a workspace's steps in order", id.clone(), queued()),
            ("workspaces.cleanup", "Run a dirty workspace's cleanup script", id.clone(), queued()),
            ("workspaces.force_relaunch", "Launch a dirty workspace anyway (logged as forced)", id.clone(), queued()),
            ("workspaces.stop", "Stop an active workspace by running its cleanup script", id.clone(), queued()),
            ("workspaces.reset", "Clear dirty without running cleanup (last resort)", id.clone(), Execution::Inline),
            (
                "workspaces.remove",
                "Remove a workspace that isn't running (restore brings it back)",
                id.clone(),
                Execution::Inline,
            ),
            ("workspaces.restore", "Bring a removed workspace back", id.clone(), Execution::Inline),
            (
                "workspaces.purge",
                "Delete a removed workspace for good, or every one; asks first (ADR 0038)",
                json!({"type": "object", "properties": {"id": {"type": "string"}, "confirm": {"type": "object"}}}),
                Execution::Inline,
            ),
            (
                "workspaces.copy",
                "Make a new workspace from an existing one, optionally with some answers changed",
                json!({"type": "object", "required": ["id", "to"], "properties": {
                    "id": {"type": "string"}, "to": {"type": "string"},
                    "values": {"type": "object"}, "label": {"type": "string"}}}),
                Execution::Inline,
            ),
            (
                "workspaces.rename",
                "Give a workspace that isn't running a new id",
                json!({"type": "object", "required": ["id", "to"], "properties": {
                    "id": {"type": "string"}, "to": {"type": "string"}}}),
                Execution::Inline,
            ),
            (
                "workspaces.templates",
                "List the built-in workspace templates and their questions (one, with its files, to peek)",
                json!({"type": "object", "properties": {"id": {"type": "string"}, "files": {"type": "boolean"}}}),
                Execution::Inline,
            ),
            (
                "workspaces.create",
                "Create a workspace from a template and your answers",
                json!({"type": "object", "required": ["id", "template"], "properties": {
                    "id": {"type": "string"}, "template": {"type": "string"},
                    "values": {"type": "object"}, "label": {"type": "string"}}}),
                Execution::Inline,
            ),
            (
                "workspaces.answers",
                "A workspace's template, its questions and the current answers",
                id.clone(),
                Execution::Inline,
            ),
            (
                "workspaces.reconfigure",
                "Change a workspace's answers (only while it's ready)",
                json!({"type": "object", "required": ["id", "values"], "properties": {
                    "id": {"type": "string"}, "values": {"type": "object"}}}),
                Execution::Inline,
            ),
            (
                "workspaces.health",
                "Check every workspace for problems; changes nothing (ADR 0033)",
                json!({"type": "object"}),
                Execution::Inline,
            ),
        ]
        .into_iter()
        .map(|(op, summary, params_schema, execution)| CommandSpec {
            op: op.into(),
            summary: summary.into(),
            params_schema,
            execution,
        })
        .collect()
    }

    async fn handle(&self, op: &str, params: Value, ctx: &Ctx) -> Result<Value> {
        match op {
            "workspaces.list" => self.list(ctx),
            "workspaces.status" => self.status(ctx, &decode::<Target>(params)?.id),
            "workspaces.reset" => self.reset(ctx, &decode::<Target>(params)?.id),
            "workspaces.activate" => self.activate(ctx, &decode::<Target>(params)?.id).await,
            "workspaces.force_relaunch" => self.force_relaunch(ctx, &decode::<Target>(params)?.id).await,
            "workspaces.cleanup" => self.cleanup(ctx, &decode::<Target>(params)?.id).await,
            "workspaces.stop" => self.stop(ctx, &decode::<Target>(params)?.id).await,
            "workspaces.remove" => self.remove(ctx, &decode::<Target>(params)?.id),
            "workspaces.restore" => self.restore_removed(ctx, &decode::<Target>(params)?.id),
            "workspaces.purge" => self.purge(ctx, decode(params)?),
            "workspaces.rename" => self.rename(ctx, decode(params)?),
            "workspaces.copy" => self.copy(ctx, decode(params)?),
            "workspaces.templates" => templates(decode(params)?),
            "workspaces.create" => self.create(ctx, decode(params)?),
            "workspaces.answers" => self.answers(ctx, &decode::<Target>(params)?.id),
            "workspaces.reconfigure" => self.reconfigure(ctx, decode(params)?),
            "workspaces.health" => self.health(ctx),
            _ => Err(Error::unknown_op(format!("workspaces has no op '{op}'"))),
        }
    }
}

#[derive(Deserialize)]
struct Target {
    id: String,
}

/// `workspaces.purge` (ADR 0038): one removed workspace, or all of them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Purge {
    id: Option<String>,
    confirm: Option<Value>,
}

/// `workspaces.reconfigure` (ADR 0025 §9): new answers, by question name.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reconfigure {
    id: String,
    values: Map<String, Value>,
}

/// What new answers make of a `workspace.toml`.
struct NewAnswers {
    text: String,
    /// The questions whose answers changed, for the event.
    names: Vec<String>,
    /// `{name, from, to}` each, for the result.
    changes: Vec<Value>,
    template: String,
}

/// `workspaces.copy` (ADR 0026 §2b).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Copy {
    id: String,
    to: String,
    #[serde(default)]
    values: Map<String, Value>,
    #[serde(default)]
    label: Option<String>,
}

/// `workspaces.rename` (ADR 0026 §2a).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rename {
    id: String,
    to: String,
}

/// Where a workspace records the template it was made from (ADR 0025 §9).
const TEMPLATE_FILE: &str = ".template";

/// `workspaces.create` (ADR 0025 §7).
#[derive(Deserialize)]
struct Create {
    id: String,
    template: String,
    #[serde(default)]
    values: Map<String, Value>,
    #[serde(default)]
    label: Option<String>,
}

/// `workspaces.templates` (ADR 0025 §7): every template, or only `id`; with `files`, each one's
/// files too (every script exactly as a workspace made from it gets them), for `peek --scripts`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Which {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    files: bool,
}

/// Every built-in, by category (in [`CATEGORIES`]' order) then id, and the categories' headings.
/// Only `id` when it's given (`not_found` naming the others when there's no such template).
fn templates(which: Which) -> Result<Value> {
    let mut all = match &which.id {
        Some(id) => vec![builtin::find(id)?],
        None => builtin::all()?,
    };
    let rank = |c: &str| CATEGORIES.iter().position(|(id, _)| *id == c).unwrap_or(usize::MAX);
    all.sort_by(|a, b| (rank(&a.category), &a.id).cmp(&(rank(&b.category), &b.id)));
    let categories: Vec<Value> = CATEGORIES.iter().map(|(id, label)| json!({"id": id, "label": label})).collect();
    let wire = |t: &Template| {
        let mut out = t.to_wire();
        if which.files {
            out["files"] = t.files.iter().map(|(path, text)| json!({"path": path, "text": text})).collect();
        }
        out
    };
    Ok(json!({"templates": all.iter().map(wire).collect::<Vec<_>>(), "categories": categories}))
}

impl Workspaces {
    /// Every workspace, by id. A broken one is listed as `invalid` with the reason; it never
    /// hides the others (ADR 0012 §6).
    fn list(&self, ctx: &Ctx) -> Result<Value> {
        let mut out = Vec::new();
        for (id, folder) in folders(ctx)? {
            let mut entry = Map::new();
            entry.insert("id".into(), json!(id));
            let (workspace, saved, errors) = folder.read(ctx, &id);
            if let Some(w) = &workspace {
                entry.insert("label".into(), json!(w.label));
            }
            match saved {
                Some(saved) if errors.is_empty() => add_state(&mut entry, &saved.state),
                _ => add_invalid(&mut entry, &errors),
            }
            out.push(Value::Object(entry));
        }
        Ok(json!({"workspaces": out}))
    }

    /// What `shimmer doctor` shows for workspaces (ADR 0033): ones that can't be read, dirty ones,
    /// and ones whose detached steps nothing can stop. Writes nothing.
    fn health(&self, ctx: &Ctx) -> Result<Value> {
        let all = folders(ctx)?;
        let mut checks = Vec::new();
        for (id, folder) in &all {
            let (workspace, saved, errors) = folder.read(ctx, id);
            if !errors.is_empty() {
                // Each error already starts "<id>: " when it's about the files; say the id once.
                let prefix = format!("{id}: ");
                let errors: Vec<&str> = errors.iter().map(|e| e.strip_prefix(&prefix).unwrap_or(e)).collect();
                checks.push(json!({"status": "fail", "subject": id,
                    "message": format!("{id} can't be used: {}", errors.join("; ")),
                    "fix": {"summary": "fix its workspace.toml or the step files it lists"}}));
                continue;
            }
            if let Some(WorkspaceState::Dirty { reason, failed_step, log, .. }) = saved.as_ref().map(|s| &s.state) {
                let fix = if folder.has_cleanup_script() {
                    json!({"summary": "run its cleanup script", "op": "workspaces.cleanup", "params": {"id": id}})
                } else {
                    json!({"summary": "it has no cleanup script: undo what the step left behind, then reset it",
                           "op": "workspaces.reset", "params": {"id": id}})
                };
                checks.push(json!({"status": "fail", "subject": id,
                    "message": format!("{id} is dirty: step {failed_step} failed ({reason}); log: {log}"), "fix": fix}));
            }
            let detached = workspace.iter().flat_map(|w| &w.steps).filter(|s| s.mode == SpawnMode::Detached).count();
            if detached > 0 && !folder.has_cleanup_script() {
                let steps = if detached == 1 { "a detached step" } else { "detached steps" };
                checks.push(json!({"status": "warn", "subject": id,
                    "message": format!("{id} starts {steps} but has no cleanup script, so stopping it can't close them"),
                    "fix": {"summary": "add cleanup.sh and a [cleanup] section to workspace.toml"}}));
            }
        }
        if checks.is_empty() {
            let n = all.len();
            checks.push(json!({"status": "ok",
                "message": format!("{n} {}, all fine", if n == 1 { "workspace" } else { "workspaces" })}));
        }
        Ok(json!({"checks": checks}))
    }

    /// One workspace in full: its definition, state, and why it is dirty or invalid.
    fn status(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let folder = folder(ctx, id)?;
        let mut out = Map::new();
        out.insert("id".into(), json!(id));
        out.insert("has_cleanup_script".into(), json!(folder.has_cleanup_script()));
        let (workspace, saved, errors) = folder.read(ctx, id);
        if let Some(w) = &workspace {
            describe(&mut out, w);
        }
        // `null` until the first launch, as in crates/mockd/fixtures/workspace-dirty.json.
        let last = saved.as_ref().and_then(|s| s.last_session.as_ref()).map(|last| {
            json!({"id": last.id, "started_at": rfc3339(&last.started_at),
                   "forced": last.forced, "outcome": last.outcome.as_str()})
        });
        out.insert("last_session".into(), json!(last));
        match saved {
            // Dirty or running details only for a workspace that can be read: an invalid one shows
            // what's wrong, not a stale reason from before it broke.
            Some(saved) if errors.is_empty() => {
                add_state(&mut out, &saved.state);
                if let WorkspaceState::Dirty { reason, failed_step, failed_at, log } = &saved.state {
                    out.insert("log".into(), json!(log));
                    out.insert(
                        "dirty".into(),
                        json!({"reason": reason, "failed_step": failed_step.to_string(),
                               "failed_at": rfc3339(failed_at), "log": log}),
                    );
                }
                if let (WorkspaceState::Launching, Some(step)) = (&saved.state, &saved.running_step) {
                    out.insert("running_step".into(), json!(step.to_string()));
                }
            }
            _ => add_invalid(&mut out, &errors),
        }
        Ok(Value::Object(out))
    }

    /// A new workspace from a template and the person's answers: the template's files, with the
    /// answers as `[env]`, checked like any workspace and written with their event in one
    /// transaction. Never overwrites (ADR 0025 §7).
    fn create(&self, ctx: &Ctx, p: Create) -> Result<Value> {
        if !manifest::valid_name(&p.id) {
            return Err(Error::invalid_params(format!(
                "'{}' is not a valid workspace id: lowercase letters, digits, '-' or '_', at most {} characters",
                p.id,
                manifest::MAX_NAME_LEN
            )));
        }
        let template = builtin::find(&p.template)?;
        if let Some(label) = &p.label {
            if label.trim().is_empty() || label.contains(['\n', '\r']) {
                return Err(Error::invalid_params("label must be one line of text"));
            }
        }
        let answers = template.answers(&p.values)?;
        let files = template.workspace_files(&answers, p.label.as_deref().map(str::trim));

        // A template that can't produce a valid workspace is a bug: fail, don't write it.
        let names: Vec<String> = files.iter().map(|(path, _)| path.clone()).collect();
        let toml = files.iter().find(|(path, _)| path == "workspace.toml").map(|(_, t)| t.as_str()).unwrap_or_default();
        manifest::parse(&p.id, toml, &names).map_err(|e| {
            Error::internal(format!("template '{}' made an invalid workspace: {}", p.template, e.message))
        })?;

        {
            let _g = self.write.lock();
            if !ctx.store.list(&p.id)?.is_empty() {
                return Err(Error::conflict(format!("there is already a workspace '{}'", p.id)));
            }
            ctx.store.transaction(|tx| {
                for (path, text) in &files {
                    tx.put(&format!("{}/{path}", p.id), text.clone())?;
                }
                tx.put(&format!("{}/{TEMPLATE_FILE}", p.id), format!("{}\n", template.id))?;
                tx.emit("workspaces.workspace.created", json!({"workspace": p.id, "template": p.template}))
            })?;
        }
        self.status(ctx, &p.id)
    }

    /// The template workspace ID was made from: its `.template` file, or for one made before that
    /// file existed, the "Created from Shimmer's X template." line every template writes into
    /// `workspace.toml`. `invalid_params` when it was made by hand.
    fn template_of(&self, ctx: &Ctx, id: &str, toml: &str) -> Result<Template> {
        let recorded = ctx.store.read_string(&format!("{id}/{TEMPLATE_FILE}"))?.map(|t| t.trim().to_owned());
        let from_comment = || {
            toml.lines()
                .find_map(|l| l.split("Created from Shimmer's ").nth(1))
                .and_then(|rest| rest.split(' ').next())
                .map(str::to_owned)
        };
        match recorded.filter(|t| !t.is_empty()).or_else(from_comment) {
            Some(template) => builtin::find(&template),
            None => Err(Error::invalid_params(format!(
                "'{id}' wasn't made from a template, so there are no questions to ask again: \
                 edit its workspace.toml instead (shimmer workspaces edit {id})"
            ))),
        }
    }

    /// The `[env]` of workspace ID's `workspace.toml`, in file order, and the file's text.
    fn current_env(&self, ctx: &Ctx, id: &str) -> Result<(Vec<(String, String)>, String)> {
        folder(ctx, id)?;
        let text = ctx
            .store
            .read_string(&format!("{id}/workspace.toml"))?
            .ok_or_else(|| Error::invalid_params(format!("{id}: there is no workspace.toml")))?;
        let doc: toml::Table =
            toml::from_str(&text).map_err(|e| Error::invalid_params(format!("{id}/workspace.toml: {e}")))?;
        let env = doc
            .get("env")
            .and_then(toml::Value::as_table)
            .map(|t| t.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())).collect())
            .unwrap_or_default();
        Ok((env, text))
    }

    /// `workspaces.answers`: the template's questions, as `workspaces.templates` gives them, and
    /// the workspace's current answer to each (null when its `[env]` doesn't have one yet).
    fn answers(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let (env, text) = self.current_env(ctx, id)?;
        let template = self.template_of(ctx, id, &text)?;
        let answers: Map<String, Value> = template
            .questions
            .iter()
            .map(|q| {
                let now = env.iter().find(|(n, _)| *n == q.name).map(|(_, v)| json!(v));
                (q.name.clone(), now.unwrap_or(Value::Null))
            })
            .collect();
        Ok(json!({"id": id, "template": template.id, "questions": template.to_wire()["questions"], "answers": answers}))
    }

    /// `workspaces.reconfigure`: new answers into `[env]`, only while the workspace is ready, and
    /// only that table rewritten. The result lists each change; the event only the names.
    fn reconfigure(&self, ctx: &Ctx, p: Reconfigure) -> Result<Value> {
        let _g = self.write.lock();
        let f = folder(ctx, &p.id)?;
        let saved = f.saved(ctx, &p.id).unwrap_or_else(|| Ok(Saved::ready()))?;
        state::start_reconfigure(&saved.state)?;
        let (_, text) = self.current_env(ctx, &p.id)?;
        let new = self.new_answers(ctx, &p.id, &text, &p.values)?;
        if !new.names.is_empty() {
            // Never write a workspace the next activate would reject.
            manifest::parse(&p.id, &new.text, &f.files)?;
            ctx.store.transaction(|tx| {
                tx.put(&format!("{}/workspace.toml", p.id), new.text.clone())?;
                tx.emit("workspaces.workspace.reconfigured", json!({"workspace": p.id, "changed": new.names}))
            })?;
        }
        Ok(json!({"id": p.id, "template": new.template, "changed": new.changes}))
    }

    /// Workspace ID's `workspace.toml` (TEXT) with VALUES as its new answers, checked as `create`
    /// checks them (ADR 0025 §9). Shared by reconfigure and copy.
    fn new_answers(&self, ctx: &Ctx, id: &str, text: &str, values: &Map<String, Value>) -> Result<NewAnswers> {
        let (env, _) = self.current_env(ctx, id)?;
        let template = self.template_of(ctx, id, text)?;
        let (new_env, names) = template.reconfigured(&env, values)?;
        let before = |name: &str| env.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
        let changes = names
            .iter()
            .map(|name| {
                let to = new_env.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()).unwrap_or_default();
                json!({"name": name, "from": before(name), "to": to})
            })
            .collect();
        let text = if names.is_empty() { text.to_owned() } else { crate::template::with_env(text, &new_env) };
        Ok(NewAnswers { text, names, changes, template: template.id })
    }

    /// Copy workspace ID to TO, with VALUES as new answers and LABEL as its label (ADR 0026 §2b):
    /// every file but its state, in one transaction, so the copy starts `ready` with no history.
    /// The original may be in any state: it is only read.
    fn copy(&self, ctx: &Ctx, p: Copy) -> Result<Value> {
        let (id, to) = (p.id.as_str(), p.to.trim());
        let _g = self.write.lock();
        let f = folder(ctx, id)?;
        if !manifest::valid_name(to) {
            return Err(Error::invalid_params(format!(
                "'{to}' is not a valid workspace id: lowercase letters, digits, '-' or '_', at most {} characters",
                manifest::MAX_NAME_LEN
            )));
        }
        if !ctx.store.list(to)?.is_empty() {
            return Err(Error::conflict(format!("there is already a workspace '{to}'")));
        }
        let (_, text) = self.current_env(ctx, id)?;
        let mut new = if p.values.is_empty() {
            NewAnswers { text: text.clone(), names: Vec::new(), changes: Vec::new(), template: String::new() }
        } else {
            self.new_answers(ctx, id, &text, &p.values)?
        };
        if let Some(label) = p.label.as_deref().map(str::trim) {
            if label.is_empty() || label.contains(['\n', '\r']) {
                return Err(Error::invalid_params("label must be one line of text"));
            }
            new.text = crate::template::with_label(&new.text, label);
        }
        let files: Vec<&String> = f.files.iter().filter(|file| *file != STATE_FILE).collect();
        let names: Vec<String> = files.iter().map(|f| (*f).clone()).collect();
        manifest::parse(to, &new.text, &names)?;
        ctx.store.transaction(|tx| {
            for file in &files {
                let bytes = if *file == "workspace.toml" {
                    new.text.clone().into_bytes()
                } else {
                    tx.read(&format!("{id}/{file}"))?.unwrap_or_default()
                };
                tx.put(&format!("{to}/{file}"), bytes)?;
            }
            tx.emit("workspaces.workspace.copied", json!({"from": id, "to": to, "changed": new.names}))
        })?;
        Ok(json!({"from": id, "to": to, "changed": new.changes}))
    }

    /// Move a workspace that isn't running to `.removed/<id>/`, in one transaction (ADR 0026 §2).
    /// The latest removal of an id replaces an older one.
    fn remove(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let _g = self.write.lock();
        let folder = folder(ctx, id)?;
        // A state file that can't be read doesn't stop removing a broken workspace.
        if let Some(Ok(saved)) = folder.saved(ctx, id) {
            state::start_remove(&saved.state).map_err(|e| Error::new(e.code, format!("{id}: {}", e.message)))?;
        }
        let to = removed_path(id);
        let stale = ctx.store.list(&to)?;
        ctx.store.transaction(|tx| {
            for path in &stale {
                tx.delete(path)?;
            }
            for file in &folder.files {
                let from = format!("{id}/{file}");
                let bytes = tx.read(&from)?.unwrap_or_default();
                tx.put(&format!("{to}/{file}"), bytes)?;
                tx.delete(&from)?;
            }
            tx.emit("workspaces.workspace.removed", json!({"workspace": id}))
        })?;
        Ok(json!({"removed": true}))
    }

    /// Move a workspace that isn't running to a new id, every file in one transaction (ADR 0026
    /// §3). The result's `notes` name what stays under the old id outside the Shimmer folder,
    /// which the daemon can't move: a separate browser window's profile.
    fn rename(&self, ctx: &Ctx, p: Rename) -> Result<Value> {
        let (id, to) = (p.id.as_str(), p.to.trim());
        let _g = self.write.lock();
        let f = folder(ctx, id)?;
        if !manifest::valid_name(to) {
            return Err(Error::invalid_params(format!(
                "'{to}' is not a valid workspace id: lowercase letters, digits, '-' or '_', at most {} characters",
                manifest::MAX_NAME_LEN
            )));
        }
        if to == id {
            return Err(Error::invalid_params(format!("'{id}' is already called that")));
        }
        // Fail closed: unlike remove (which restore undoes), a rename moves everything stop and
        // cleanup find by the id, so an unreadable state file -- which may hide a running or
        // dirty workspace -- refuses it (ADR 0026 §2a).
        match f.saved(ctx, id) {
            Some(Ok(saved)) => {
                state::start_rename(&saved.state).map_err(|e| Error::new(e.code, format!("{id}: {}", e.message)))?
            }
            Some(Err(e)) => {
                return Err(Error::conflict(format!(
                    "{id}: its state file can't be read ({}), so it may still be running: \
                     fix or reset it first (workspaces.reset)",
                    e.message
                )))
            }
            None => {}
        }
        if !ctx.store.list(to)?.is_empty() {
            return Err(Error::conflict(format!("there is already a workspace '{to}'")));
        }
        let mut notes = Vec::new();
        if let Ok((env, _)) = self.current_env(ctx, id) {
            if env.iter().any(|(k, v)| k == "BROWSER_WINDOW" && v == "separate") {
                notes.push(format!(
                    "its browser window's profile (where you logged in to sites) stays under the old name; \
                     to keep it: mv ~/.local/state/shimmer/browser-profiles/{id} ~/.local/state/shimmer/browser-profiles/{to}"
                ));
            }
        }
        ctx.store.transaction(|tx| {
            for file in &f.files {
                let from = format!("{id}/{file}");
                let bytes = tx.read(&from)?.unwrap_or_default();
                tx.put(&format!("{to}/{file}"), bytes)?;
                tx.delete(&from)?;
            }
            tx.emit("workspaces.workspace.renamed", json!({"from": id, "to": to}))
        })?;
        Ok(json!({"from": id, "to": to, "notes": notes}))
    }

    /// Bring a removed workspace back exactly as it was (ADR 0026 §2).
    fn restore_removed(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let missing = || Error::not_found(format!("no removed workspace '{id}'"));
        if !manifest::valid_name(id) {
            return Err(missing());
        }
        {
            let _g = self.write.lock();
            let from = removed_path(id);
            let files = ctx.store.list(&from)?;
            if files.is_empty() {
                return Err(missing());
            }
            if !ctx.store.list(id)?.is_empty() {
                return Err(Error::conflict(format!("there is a workspace '{id}' again; remove or rename it first")));
            }
            ctx.store.transaction(|tx| {
                for path in &files {
                    let rest = path.strip_prefix(&format!("{from}/")).unwrap_or(path);
                    let bytes = tx.read(path)?.unwrap_or_default();
                    tx.put(&format!("{id}/{rest}"), bytes)?;
                    tx.delete(path)?;
                }
                tx.emit("workspaces.workspace.restored", json!({"workspace": id}))
            })?;
        }
        self.status(ctx, id)
    }

    /// Delete removed workspaces for good: `p.id`'s, or every one, after the caller confirms how
    /// many it was shown, as `records.purge` asks (ADR 0038 §1). The one workspaces op that can't
    /// be undone, so it never runs on a guess.
    fn purge(&self, ctx: &Ctx, p: Purge) -> Result<Value> {
        let _g = self.write.lock();
        let ids: Vec<String> = match &p.id {
            Some(id) => {
                if !manifest::valid_name(id) || ctx.store.list(&removed_path(id))?.is_empty() {
                    return Err(Error::not_found(format!("no removed workspace '{id}'")));
                }
                vec![id.clone()]
            }
            None => removed_ids(ctx)?,
        };
        let workspaces = ids.len();
        if workspaces == 0 {
            return Ok(json!({"purged": 0}));
        }
        let expected = json!({"workspaces": workspaces});
        if p.confirm.as_ref() != Some(&expected) {
            let what = match &p.id {
                Some(id) => format!("the removed workspace '{id}'"),
                None => format!("all {workspaces} removed workspace(s)"),
            };
            return Err(Error::new(
                shimmer_core::ErrorCode::ConfirmationRequired,
                format!("purging permanently deletes {what}; confirm with {{\"workspaces\": {workspaces}}}"),
            )
            .with_detail(expected));
        }
        let mut files = Vec::new();
        for id in &ids {
            files.extend(ctx.store.list(&removed_path(id))?);
        }
        let mut event = json!({"workspaces": workspaces});
        if let Some(id) = &p.id {
            event["workspace"] = json!(id);
        }
        ctx.store.transaction(|tx| {
            for path in &files {
                tx.delete(path)?;
            }
            tx.emit("workspaces.workspace.purged", event)
        })?;
        Ok(json!({"purged": workspaces}))
    }

    /// `dirty → ready` without running cleanup (§10.3: the last resort when there is no
    /// cleanup script). Also clears a `state.toml` Shimmer can't read, which is the one way to
    /// recover from a hand-edit gone wrong.
    fn reset(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        let _g = self.write.lock();
        let folder = folder(ctx, id)?;
        let (prior, last_session) = match folder.saved(ctx, id) {
            Some(Err(e)) => (json!({"unreadable_state": e.message}), None),
            saved => {
                let saved = saved.unwrap_or_else(|| Ok(Saved::ready()))?;
                state::reset(&saved.state)?;
                (dirty_payload(id, &saved.state), saved.last_session)
            }
        };
        ctx.store.transaction(|tx| {
            tx.put(&state_path(id), persist::encode(&Saved::ready().with_last_session(last_session)))?;
            tx.emit("workspaces.workspace.reset", json!({"workspace": id, "prior": prior}))
        })?;
        Ok(json!({"id": id, "state": "ready"}))
    }
}

// ---------------------------------------------------------------- reading the folders

/// One workspace folder's file list, relative to the folder (`workspace.toml`,
/// `steps/01-setup.sh`, `state.toml`, …), as `manifest::parse` takes it.
pub(crate) struct Folder {
    pub(crate) files: Vec<String>,
}

impl Folder {
    pub(crate) fn has(&self, name: &str) -> bool {
        self.files.iter().any(|f| f == name)
    }

    pub(crate) fn has_cleanup_script(&self) -> bool {
        self.has("cleanup.sh") || self.has("cleanup.ps1")
    }

    /// The checked `workspace.toml`, or why it isn't valid.
    pub(crate) fn load(&self, ctx: &Ctx, id: &str) -> Result<Workspace> {
        let text = ctx
            .store
            .read_string(&format!("{id}/workspace.toml"))?
            .ok_or_else(|| Error::invalid_params(format!("{id}: there is no workspace.toml")))?;
        manifest::parse(id, &text, &self.files)
    }

    /// Both files at once, with every problem found: `workspace.toml` and `state.toml` can each
    /// be broken, and the user should see both rather than fix one only to meet the other.
    /// `saved` is `Ready` when there's no `state.toml` yet, and `None` only if it can't be read.
    pub(crate) fn read(&self, ctx: &Ctx, id: &str) -> (Option<Workspace>, Option<Saved>, Vec<String>) {
        let mut errors = Vec::new();
        let workspace = self.load(ctx, id).map_err(|e| errors.push(e.message)).ok();
        let saved = match self.saved(ctx, id) {
            None => Some(Saved::ready()),
            Some(Ok(saved)) => Some(saved),
            Some(Err(e)) => {
                errors.push(e.message);
                None
            }
        };
        (workspace, saved, errors)
    }

    /// `None` when there is no `state.toml` yet.
    pub(crate) fn saved(&self, ctx: &Ctx, id: &str) -> Option<Result<Saved>> {
        if !self.has(STATE_FILE) {
            return None;
        }
        Some(ctx.store.read_string(&state_path(id)).and_then(|text| persist::decode(id, &text.unwrap_or_default())))
    }
}

/// Every workspace folder under the namespace, by id. Loose files at the top, and folders whose
/// name starts with `.` (`.removed/`, ADR 0026 §2), are not workspaces.
fn folders(ctx: &Ctx) -> Result<BTreeMap<String, Folder>> {
    let mut out: BTreeMap<String, Folder> = BTreeMap::new();
    for path in ctx.store.list("")? {
        if let Some((id, rest)) = path.split_once('/').filter(|(id, _)| !id.starts_with('.')) {
            out.entry(id.to_owned()).or_insert_with(|| Folder { files: Vec::new() }).files.push(rest.to_owned());
        }
    }
    Ok(out)
}

/// The folder for `id`, listing only that folder. The id is checked against the workspace-id
/// charset first, so a request can never name a path outside it: anything else (`../records`,
/// `deep-work/steps`, empty) is simply `not_found`, as is an id with no folder.
pub(crate) fn folder(ctx: &Ctx, id: &str) -> Result<Folder> {
    let missing = || Error::not_found(format!("no workspace '{id}'"));
    if !manifest::valid_name(id) {
        return Err(missing());
    }
    let prefix = format!("{id}/");
    let files: Vec<String> =
        ctx.store.list(id)?.into_iter().filter_map(|p| p.strip_prefix(&prefix).map(str::to_owned)).collect();
    if files.is_empty() {
        return Err(missing());
    }
    Ok(Folder { files })
}

/// Where a removed workspace waits to be restored (ADR 0026 §2).
fn removed_path(id: &str) -> String {
    format!(".removed/{id}")
}

/// Every removed workspace's id, sorted.
fn removed_ids(ctx: &Ctx) -> Result<Vec<String>> {
    let mut ids: Vec<String> = ctx
        .store
        .list(".removed")?
        .iter()
        .filter_map(|p| p.strip_prefix(".removed/")?.split('/').next().map(str::to_owned))
        .collect();
    ids.dedup();
    Ok(ids)
}

pub(crate) fn state_path(id: &str) -> String {
    format!("{id}/{STATE_FILE}")
}

// ---------------------------------------------------------------- output

/// `state: "invalid"` and every reason why, one per line.
fn add_invalid(out: &mut Map<String, Value>, errors: &[String]) {
    out.insert("state".into(), json!("invalid"));
    out.insert("error".into(), json!(errors.join("\n")));
}

fn add_state(out: &mut Map<String, Value>, state: &WorkspaceState) {
    if let WorkspaceState::Dirty { reason, failed_step, .. } = state {
        out.insert("dirty_reason".into(), json!(format!("step {failed_step}: {reason}")));
    }
    out.insert("state".into(), json!(state_name(state)));
}

/// The state's name on the wire (docs/protocol.md, "Workspace ops").
pub(crate) fn state_name(state: &WorkspaceState) -> &'static str {
    match state {
        WorkspaceState::Ready => "ready",
        WorkspaceState::Launching => "launching",
        WorkspaceState::Active => "active",
        WorkspaceState::Dirty { .. } => "dirty",
    }
}

fn describe(out: &mut Map<String, Value>, w: &Workspace) {
    out.insert("label".into(), json!(w.label));
    if let Some(d) = &w.description {
        out.insert("description".into(), json!(d));
    }
    let steps: Vec<Value> = w
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut step = json!({"index": i + 1, "name": s.name});
            match s.mode {
                SpawnMode::Detached => step["mode"] = json!("detached"),
                SpawnMode::Supervised { timeout } => {
                    step["mode"] = json!("supervised");
                    step["timeout_s"] = json!(timeout.as_secs());
                }
            }
            if let Some(d) = &s.description {
                step["description"] = json!(d);
            }
            step
        })
        .collect();
    out.insert("steps".into(), json!(steps));
    if let Some(t) = w.cleanup_timeout {
        out.insert("cleanup_timeout_s".into(), json!(t.as_secs()));
    }
}

/// `workspaces.session.dirty`'s payload, and `prior` in `workspaces.workspace.reset`.
pub(crate) fn dirty_payload(id: &str, state: &WorkspaceState) -> Value {
    match state {
        WorkspaceState::Dirty { reason, failed_step, failed_at, log } => json!({
            "workspace": id, "reason": reason, "failed_step": failed_step.to_string(),
            "failed_at": rfc3339(failed_at), "log": log,
        }),
        _ => json!({"workspace": id}),
    }
}

pub(crate) fn rfc3339(t: &chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
