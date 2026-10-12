//! `shimmer-records`: typed collections of records (CLAUDE.md §8). A LeetCode tracker, a job tracker
//! or any user-defined tracker is a collection TOML; this module is the one implementation of
//! storage, validation and filtering behind all of them. Layout and ops: ADR 0008.
//!
//! Depends on `core` only. Everything durable goes through `ctx.store`, one transaction per
//! change, with its event committed alongside (§7.1).

mod collections;
mod item;
mod lifecycle;
mod query;
mod schema;
mod templates;
mod values;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use shimmer_core::ids::is_valid_name;
use shimmer_core::params::decode;
use shimmer_core::{CommandSpec, Ctx, Error, ErrorCode, Execution, Manifest, Module, Result, WriteLock};

use crate::item::Item;
use crate::schema::Collection;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;
/// Most rows one `records.import` takes (ADR 0024 §4).
const MAX_IMPORT_ROWS: usize = 5000;

#[derive(Default)]
pub struct Records {
    /// Inline requests run concurrently. Held across each read-check-write so two `records.add`
    /// calls for one id cannot both pass the existence check. Never held across an `.await`.
    write: WriteLock,
}

#[async_trait]
impl Module for Records {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "records".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            namespace: "records".into(),
            topics: [
                "records.item.created",
                "records.item.updated",
                "records.item.completed",
                "records.item.reopened",
                "records.item.renamed",
                "records.item.restored",
                "records.field.renamed",
                "records.collection.renamed",
                "records.collection.removed",
                "records.collection.restored",
                "records.item.removed",
                "records.collection.created",
                "records.collection.copied",
                "records.trash.purged",
            ]
            .map(String::from)
            .to_vec(),
            capabilities: vec![],
        }
    }

    /// Nothing to set up: a fresh install starts with no collections, and the person creates the
    /// ones they want from templates (ADR 0023, replacing ADR 0008's seeding).
    async fn init(&self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn commands(&self) -> Vec<CommandSpec> {
        let target = json!({"collection": {"type": "string"}, "id": {"type": "string"}});
        let with = |extra: Value| {
            let mut props = target.as_object().cloned().unwrap_or_default();
            props.extend(extra.as_object().cloned().unwrap_or_default());
            json!({"type": "object", "properties": props, "required": ["collection", "id"]})
        };
        let fields = json!({"fields": {"type": "object"}});
        [
            ("records.collections", "List collection definitions", json!({"type": "object"})),
            (
                "records.add",
                "Add a record to a collection; without an id, one is made from its title",
                json!({"type": "object", "required": ["collection"], "properties": {
                    "collection": {"type": "string"}, "id": {"type": "string"}, "fields": {"type": "object"}}}),
            ),
            ("records.get", "Get one record", with(json!({}))),
            (
                "records.list",
                "List records in a collection",
                json!({"type": "object", "required": ["collection"], "properties": {
                    "collection": {"type": "string"}, "filter": {"type": "object"},
                    "search": {"type": "string"}, "sort": {"type": "array", "items": {"type": "string"}},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT},
                    "offset": {"type": "integer", "minimum": 0}}}),
            ),
            ("records.update", "Set or unset (null) fields of a record", with(fields.clone())),
            (
                "records.complete",
                "Mark a record done and stamp its completion date, optionally setting fields",
                with(fields),
            ),
            (
                "records.reopen",
                "Mark a done record todo again (the stamp is kept unless clear_stamp)",
                with(json!({"clear_stamp": {"type": "boolean"}})),
            ),
            (
                "records.rename",
                "Give a record a new id",
                json!({"type": "object", "required": ["collection", "id", "new_id"], "properties": {
                    "collection": {"type": "string"}, "id": {"type": "string"}, "new_id": {"type": "string"}}}),
            ),
            (
                "records.remove",
                "Remove a record (it goes to the trash; records.restore brings it back); force if others refer to it",
                with(json!({"force": {"type": "boolean"}})),
            ),
            ("records.restore", "Bring a removed record back from the trash", with(json!({}))),
            (
                "records.trash",
                "List a collection's removed records",
                json!({"type": "object", "required": ["collection"], "properties": {"collection": {"type": "string"}}}),
            ),
            (
                "records.templates",
                "List the built-in collection templates",
                json!({"type": "object", "properties": {"id": {"type": "string"}, "file": {"type": "boolean"}}}),
            ),
            (
                "records.create_collection",
                "Create a collection from a template",
                json!({"type": "object", "required": ["id", "template"], "properties": {
                    "id": {"type": "string"}, "template": {"type": "string"}, "label": {"type": "string"}}}),
            ),
            (
                "records.check",
                "Report records that don't fit their collection; changes nothing",
                json!({"type": "object", "required": ["collection"], "properties": {"collection": {"type": "string"}}}),
            ),
            (
                "records.rename_field",
                "Rename a field in a collection and every record",
                json!({"type": "object", "required": ["collection", "from", "to"], "properties": {
                    "collection": {"type": "string"}, "from": {"type": "string"}, "to": {"type": "string"}}}),
            ),
            (
                "records.rename_collection",
                "Give a collection a new id",
                json!({"type": "object", "required": ["id", "new_id"], "properties": {
                    "id": {"type": "string"}, "new_id": {"type": "string"}}}),
            ),
            (
                "records.copy_collection",
                "Copy a collection under a new id, empty or with its records",
                json!({"type": "object", "required": ["id", "to"], "properties": {
                    "id": {"type": "string"}, "to": {"type": "string"}, "label": {"type": "string"},
                    "with_records": {"type": "boolean"}}}),
            ),
            (
                "records.remove_collection",
                "Remove a collection and its records (they can be restored); needs confirmation",
                json!({"type": "object", "required": ["id"], "properties": {
                    "id": {"type": "string"}, "confirm": {"type": "object"}}}),
            ),
            (
                "records.import",
                "Add many records in one transaction; rows already there (by id or unique value) are skipped",
                json!({"type": "object", "required": ["collection", "rows"], "properties": {
                    "collection": {"type": "string"},
                    "rows": {"type": "array", "items": {"type": "object"}, "maxItems": MAX_IMPORT_ROWS},
                    "dry_run": {"type": "boolean"}, "skip_invalid": {"type": "boolean"}}}),
            ),
            (
                "records.purge",
                "Permanently delete removed records (one, or a collection's whole trash); needs confirmation",
                json!({"type": "object", "required": ["collection"], "properties": {
                    "collection": {"type": "string"}, "id": {"type": "string"}, "confirm": {"type": "object"}}}),
            ),
            (
                "records.restore_collection",
                "Bring a removed collection back with its records",
                json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}}),
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
            "records.collections" => self.collections(ctx),
            "records.add" => self.add(ctx, decode(params)?),
            "records.get" => {
                let t: Target = decode(params)?;
                let c = load_collection(ctx, &t.collection)?;
                Ok(load_item(ctx, &c, &t.id)?.to_wire(&c))
            }
            "records.list" => self.list(ctx, decode(params)?),
            "records.update" => self.update(ctx, decode(params)?),
            "records.complete" => self.complete(ctx, decode(params)?),
            "records.reopen" => self.reopen(ctx, decode(params)?),
            "records.rename" => self.rename(ctx, decode(params)?),
            "records.remove" => self.remove(ctx, decode(params)?),
            "records.restore" => self.restore(ctx, decode(params)?),
            "records.trash" => self.trash(ctx, decode(params)?),
            "records.check" => self.check(ctx, decode(params)?),
            "records.templates" => templates_of(decode(params)?),
            "records.create_collection" => self.create_collection(ctx, decode(params)?),
            "records.rename_field" => self.rename_field(ctx, decode(params)?),
            "records.rename_collection" => self.rename_collection(ctx, decode(params)?),
            "records.copy_collection" => self.copy_collection(ctx, decode(params)?),
            "records.remove_collection" => self.remove_collection(ctx, decode(params)?),
            "records.restore_collection" => self.restore_collection(ctx, decode(params)?),
            "records.import" => self.import(ctx, decode(params)?),
            "records.purge" => self.purge(ctx, decode(params)?),
            _ => Err(Error::unknown_op(format!("records has no op '{op}'"))),
        }
    }
}

#[derive(Deserialize)]
struct Target {
    collection: String,
    id: String,
}

/// `records.remove`: `force` removes a record others still refer to (ADR 0024 §3).
#[derive(Deserialize)]
struct Remove {
    collection: String,
    id: String,
    #[serde(default)]
    force: bool,
}

/// `records.purge` (ADR 0024 §5).
#[derive(Deserialize)]
struct Purge {
    collection: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    confirm: Option<Value>,
}

/// `records.import` (ADR 0024 §4).
#[derive(Deserialize)]
struct Import {
    collection: String,
    rows: Vec<Value>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    skip_invalid: bool,
}

#[derive(Deserialize)]
struct WithFields {
    collection: String,
    id: String,
    #[serde(default)]
    fields: Map<String, Value>,
}

/// `records.add`: the id is optional (ADR 0018 §1).
#[derive(Deserialize)]
struct Add {
    collection: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    fields: Map<String, Value>,
}

#[derive(Deserialize)]
struct CreateCollection {
    id: String,
    template: String,
    #[serde(default)]
    label: Option<String>,
}

/// `records.templates` (ADR 0030 §1): every template, or only `id`; with `file`, each one's
/// text exactly as `records.new` would copy it.
#[derive(Deserialize, Default)]
struct WhichTemplates {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    file: bool,
}

/// Every built-in, by category (in [`templates::CATEGORIES`]' order) then id, as
/// `records.collections` shows a collection plus its `category`, `notes` and `examples` (from its
/// header comment), and the categories' headings. Only `id` when it's given.
fn templates_of(which: WhichTemplates) -> Result<Value> {
    let mut all: Vec<_> = templates::TEMPLATES.iter().collect();
    if let Some(id) = &which.id {
        all.retain(|(t, _, _)| t == id);
        if all.is_empty() {
            return Err(no_template(id));
        }
    }
    all.sort_by_key(|(id, category, _)| (templates::rank(category), *id));
    let wire = |(id, category, text): &&(&str, &str, &str)| -> Result<Value> {
        let mut out = serde_json::to_value(Collection::parse(id, text)?).unwrap_or_default();
        let (notes, examples) = templates::header(text);
        out["category"] = json!(category);
        out["notes"] = json!(notes);
        out["examples"] = json!(examples);
        if which.file {
            out["file"] = json!(text);
        }
        Ok(out)
    };
    let categories: Vec<Value> =
        templates::CATEGORIES.iter().map(|(id, label)| json!({"id": id, "label": label})).collect();
    Ok(json!({"templates": all.iter().map(wire).collect::<Result<Vec<_>>>()?, "categories": categories}))
}

/// `invalid_params` unless `id` can name a new collection (it becomes a file name).
fn new_collection_id(id: &str) -> Result<()> {
    match is_valid_name(id) {
        true => Ok(()),
        false => Err(Error::invalid_params(format!(
            "'{id}' is not a valid collection id: lowercase letters, digits, '-' or '_', starting with a letter"
        ))),
    }
}

/// Collection file `text` with its label set to `label`, which must not be blank.
fn relabel(id: &str, text: &str, label: &str) -> Result<String> {
    if label.trim().is_empty() {
        return Err(Error::invalid_params("label must not be empty"));
    }
    collections::set_label(id, text, label)
}

/// `not_found` for a template that isn't built in, naming the ones that are.
fn no_template(id: &str) -> Error {
    Error::not_found(format!("no template '{id}' (there are: {})", templates::names().join(", ")))
}

#[derive(Deserialize)]
struct CollectionOnly {
    collection: String,
}

#[derive(Deserialize)]
struct RenameField {
    collection: String,
    from: String,
    to: String,
}

#[derive(Deserialize)]
struct CollectionId {
    id: String,
}

#[derive(Deserialize)]
struct RenameCollection {
    id: String,
    new_id: String,
}

#[derive(Deserialize)]
struct CopyCollection {
    id: String,
    to: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    with_records: bool,
}

#[derive(Deserialize)]
struct RemoveCollection {
    id: String,
    #[serde(default)]
    confirm: Option<Value>,
}

#[derive(Deserialize)]
struct Rename {
    collection: String,
    id: String,
    new_id: String,
}

#[derive(Deserialize)]
struct Reopen {
    collection: String,
    id: String,
    #[serde(default)]
    clear_stamp: bool,
}

#[derive(Deserialize)]
struct ListParams {
    collection: String,
    #[serde(default)]
    filter: Map<String, Value>,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    sort: Vec<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
}

impl Records {
    /// Every readable collection, plus `skipped` naming each malformed file, the same shape as
    /// `records.list`. One broken file must not hide the others (ADR 0008): a request that
    /// uses it still fails, naming the file.
    fn collections(&self, ctx: &Ctx) -> Result<Value> {
        let (mut out, mut skipped) = (Vec::new(), Vec::new());
        for path in ctx.store.list("collections")? {
            let Some(id) = path.strip_prefix("collections/").and_then(|p| p.strip_suffix(".toml")) else { continue };
            if !is_valid_name(id) {
                skipped.push(format!("collection file '{id}.toml': the file name must match [a-z][a-z0-9_-]*"));
                continue;
            }
            match load_collection(ctx, id) {
                Ok(c) => out.push(serde_json::to_value(c).unwrap_or_default()),
                Err(e) if e.code == ErrorCode::InvalidParams => skipped.push(e.message),
                Err(e) => return Err(e),
            }
        }
        let mut data = json!({"collections": out});
        if !skipped.is_empty() {
            data["skipped"] = json!(skipped);
        }
        Ok(data)
    }

    fn add(&self, ctx: &Ctx, mut p: Add) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        if let Some(id) = &p.id {
            item::check_id(id)?;
        }
        if let Some((k, _)) = p.fields.iter().find(|(_, v)| v.is_null()) {
            return Err(Error::invalid_params(format!("field '{k}' is null; leave it out to leave it unset")));
        }
        values::normalize(&c, &mut p.fields, &now(ctx))?;
        // An empty list is unset (ADR 0024 §2): nothing to store.
        p.fields.retain(|_, v| !v.is_null());
        c.check_values(&p.fields)?;
        c.check_required(&p.fields)?;

        // Under the write lock, so nothing can take the id or a unique value in between.
        let _g = self.write.lock();
        check_refs(ctx, &c, &p.fields)?;
        let exists = |id: &str| Ok(ctx.store.read_string(&item_path(&c.id, id))?.is_some());
        let id = match p.id {
            Some(id) if exists(&id)? => {
                return Err(Error::conflict(format!("'{}' already has a record '{id}'", c.id)));
            }
            Some(id) => id,
            // ADR 0018 §1: from the title, else from today's date; never clashing.
            None => {
                let fields = p.fields.clone().into_iter().collect();
                match c.render_title(&fields).map(|t| item::slug(&t)).filter(|s| !s.is_empty()) {
                    Some(base) => item::first_free(&base, false, exists)?,
                    None => item::first_free(&now(ctx).today().format("%Y-%m-%d").to_string(), true, exists)?,
                }
            }
        };
        let mut item = Item::new(&id, p.fields);
        item.status_line = c.completable;
        let wire = item.to_wire(&c);
        let path = item_path(&c.id, &id);
        check_unique(ctx, &c, &item.id, &item.fields_map())?;
        ctx.store.transaction(|tx| {
            tx.put(&path, item.to_toml())?;
            tx.emit("records.item.created", payload(&c, &item, &wire))
        })?;
        Ok(wire)
    }

    fn list(&self, ctx: &Ctx, p: ListParams) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        let query = query::Query::new(&c, &p.filter, p.search.as_deref(), &p.sort, &now(ctx))?;
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(Error::invalid_params(format!("limit must be between 1 and {MAX_LIMIT}")));
        }

        let (mut matching, mut skipped) = (Vec::new(), Vec::new());
        let dir = format!("items/{}", c.id);
        for path in ctx.store.list(&dir)? {
            let Some(id) = path.strip_prefix(&format!("{dir}/")).and_then(|p| p.strip_suffix(".toml")) else {
                continue;
            };
            // One bad hand-edit must not hide every other record.
            match read_item(ctx, &path, id) {
                Some(Ok(item)) => {
                    let wire = item.to_wire(&c);
                    if query.matches(&wire) {
                        matching.push(wire);
                    }
                }
                Some(Err(e)) => skipped.push(e.message),
                None => {}
            }
        }
        // By the requested keys, then by id. Never by path: `two-sum-again.toml` sorts before
        // `two-sum.toml` ('-' < '.'), which is not id order (ADR 0019 §3).
        query.sort(&mut matching);
        let total = matching.len();
        let items: Vec<Value> = matching.into_iter().skip(p.offset).take(limit).collect();
        // What to call each listed record (ADR 0024 §5), so clients never render titles.
        let titles: Map<String, Value> = items
            .iter()
            .filter_map(|w| {
                let id = w["id"].as_str()?;
                let fields = w.as_object()?.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                Some((id.to_owned(), json!(c.title_of(id, &fields))))
            })
            .collect();
        let mut out = json!({"items": items, "total": total, "titles": titles});
        if !skipped.is_empty() {
            out["skipped"] = json!(skipped);
        }
        Ok(out)
    }

    fn update(&self, ctx: &Ctx, mut p: WithFields) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        values::normalize(&c, &mut p.fields, &now(ctx))?;

        let _g = self.write.lock();
        let before = load_item(ctx, &c, &p.id)?;
        values::patch_lists(&c, &mut p.fields, &before.fields)?;
        c.check_changes(&p.fields)?;
        check_refs(ctx, &c, &p.fields)?;
        check_unique(ctx, &c, &p.id, &p.fields)?;
        let mut item = before.clone();
        item.apply(p.fields);
        c.check_required(&item.fields_map())?;
        let wire = item.to_wire(&c);
        let mut event = payload(&c, &item, &wire);
        // Only fields whose value actually changed (ADR 0017 §8).
        event["changed"] = json!(item::changed(&before.fields, &item.fields));
        ctx.store.transaction(|tx| {
            tx.put(&item_path(&c.id, &p.id), item.to_toml())?;
            tx.emit("records.item.updated", event)
        })?;
        Ok(wire)
    }

    /// Done, stamped, and any `fields` set, in one transaction with one event (ADR 0016 §2).
    fn complete(&self, ctx: &Ctx, mut p: WithFields) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        let now = now(ctx);
        values::normalize(&c, &mut p.fields, &now)?;

        let _g = self.write.lock();
        let current = load_item(ctx, &c, &p.id)?;
        values::patch_lists(&c, &mut p.fields, &current.fields)?;
        check_refs(ctx, &c, &p.fields)?;
        check_unique(ctx, &c, &p.id, &p.fields)?;
        let item = lifecycle::complete(&c, &current, p.fields, &now)?;
        self.save(ctx, &c, &item, "records.item.completed")
    }

    /// `done → todo` (ADR 0016 §1).
    fn reopen(&self, ctx: &Ctx, p: Reopen) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        let _g = self.write.lock();
        let item = lifecycle::reopen(&c, &load_item(ctx, &c, &p.id)?, p.clear_stamp)?;
        self.save(ctx, &c, &item, "records.item.reopened")
    }

    /// Move a record to a new id: one transaction writes the new file, deletes the old one, and
    /// emits `records.item.renamed` (ADR 0018 §2). Values are untouched, so no unique check.
    fn rename(&self, ctx: &Ctx, p: Rename) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        item::check_id(&p.new_id)?;
        if p.new_id == p.id {
            return Err(Error::invalid_params(format!("'{}' is already called that", p.id)));
        }
        let _g = self.write.lock();
        let mut item = load_item(ctx, &c, &p.id)?;
        let new_path = item_path(&c.id, &p.new_id);
        if ctx.store.read_string(&new_path)?.is_some() {
            return Err(Error::conflict(format!("'{}' already has a record '{}'", c.id, p.new_id)));
        }
        item.id = p.new_id.clone();
        // Every reference to it follows, in the same transaction (ADR 0024 §3).
        let mut repointed = Vec::new();
        let mut references = 0;
        for (other, mut record) in referrers(ctx, &c.id, &p.id)? {
            references += collections::repoint(&other, &mut record, &c.id, &p.id, &p.new_id);
            // A record that points at itself is the one being moved.
            if other.id == c.id && record.id == p.id {
                collections::repoint(&c, &mut item, &c.id, &p.id, &p.new_id);
                continue;
            }
            repointed.push((item_path(&other.id, &record.id), record.to_toml()));
        }
        let wire = item.to_wire(&c);
        let mut event = payload(&c, &item, &wire);
        event["id"] = json!(p.id);
        event["new_id"] = json!(p.new_id);
        event["references"] = json!(references);
        ctx.store.transaction(|tx| {
            tx.put(&new_path, item.to_toml())?;
            tx.delete(&item_path(&c.id, &p.id))?;
            for (path, text) in repointed {
                tx.put(&path, text)?;
            }
            tx.emit("records.item.renamed", event)
        })?;
        Ok(wire)
    }

    fn create_collection(&self, ctx: &Ctx, p: CreateCollection) -> Result<Value> {
        new_collection_id(&p.id)?;
        let Some(text) = templates::find(&p.template) else {
            return Err(no_template(&p.template));
        };
        let mut text = collections::set_id(&p.template, text, &p.id)?;
        if let Some(label) = &p.label {
            text = relabel(&p.id, &text, label)?;
        }
        // A template that can't produce a valid collection is a bug: fail, don't write it.
        let c = Collection::parse(&p.id, &text)?;

        let _g = self.write.lock();
        if ctx.store.read_string(&collection_path(&p.id))?.is_some() {
            return Err(Error::conflict(format!("there is already a collection '{}'", p.id)));
        }
        ctx.store.transaction(|tx| {
            tx.put(&collection_path(&p.id), text)?;
            tx.emit("records.collection.created", json!({"collection": p.id, "template": p.template}))
        })?;
        Ok(serde_json::to_value(c).unwrap_or_default())
    }

    /// Every record that doesn't fit the collection, and why. Writes nothing (ADR 0021 §2).
    fn check(&self, ctx: &Ctx, p: CollectionOnly) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        let dir = format!("items/{}", c.id);
        let (mut checked, mut out) = (0, Vec::new());
        for path in ctx.store.list(&dir)? {
            let Some(id) = path.strip_prefix(&format!("{dir}/")).and_then(|p| p.strip_suffix(".toml")) else {
                continue;
            };
            checked += 1;
            let found = match read_item(ctx, &path, id) {
                Some(Ok(item)) => {
                    let mut found = collections::problems(&c, &item);
                    found.extend(dangling(ctx, &c, &item.fields)?);
                    found
                }
                Some(Err(e)) => vec![e.message],
                None => continue,
            };
            if !found.is_empty() {
                out.push(json!({"id": id, "problems": found}));
            }
        }
        Ok(json!({"checked": checked, "problems": out}))
    }

    /// Rename a field in the collection file (comments kept) and in every record that has it,
    /// live or trashed, in one transaction (ADR 0021 §3).
    fn rename_field(&self, ctx: &Ctx, p: RenameField) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        if c.field(&p.from).is_none() {
            return Err(Error::invalid_params(format!("'{}' has no field '{}'", c.id, p.from)));
        }
        if !is_valid_name(&p.to) || schema::RESERVED.contains(&p.to.as_str()) {
            return Err(Error::invalid_params(format!("'{}' is not a valid field name", p.to)));
        }
        if c.field(&p.to).is_some() {
            return Err(Error::conflict(format!("'{}' already has a field '{}'", c.id, p.to)));
        }
        let _g = self.write.lock();
        let text = collection_text(ctx, &c.id)?;
        let renamed = collections::rename_field(&c.id, &text, &p.from, &p.to)?;
        let new = Collection::parse(&c.id, &renamed)?;

        let mut writes = Vec::new();
        for dir in [format!("items/{}", c.id), format!("trash/{}", c.id)] {
            for path in ctx.store.list(&dir)? {
                let Some(id) = path.strip_prefix(&format!("{dir}/")).and_then(|p| p.strip_suffix(".toml")) else {
                    continue;
                };
                let Some(Ok(mut item)) = ctx.store.read_string(&path)?.map(|text| Item::from_toml(id, &text)) else {
                    continue;
                };
                let Some(value) = item.fields.remove(&p.from) else { continue };
                if item.fields.contains_key(&p.to) {
                    return Err(Error::conflict(format!(
                        "record '{id}' already has a key '{}'; rename or remove it first",
                        p.to
                    )));
                }
                item.fields.insert(p.to.clone(), value);
                writes.push((path, item.to_toml()));
            }
        }
        let updated = writes.len();
        ctx.store.transaction(|tx| {
            tx.put(&collection_path(&c.id), renamed)?;
            for (path, text) in writes {
                tx.put(&path, text)?;
            }
            tx.emit("records.field.renamed", json!({"collection": c.id, "from": p.from, "to": p.to}))
        })?;
        Ok(json!({"collection": new, "updated": updated}))
    }

    /// Move a collection, its records and its trash to a new id (ADR 0021 §4).
    fn rename_collection(&self, ctx: &Ctx, p: RenameCollection) -> Result<Value> {
        let c = load_collection(ctx, &p.id)?;
        new_collection_id(&p.new_id)?;
        let _g = self.write.lock();
        if ctx.store.read_string(&collection_path(&p.new_id))?.is_some() {
            return Err(Error::conflict(format!("there is already a collection '{}'", p.new_id)));
        }
        let text = collections::set_id(&c.id, &collection_text(ctx, &c.id)?, &p.new_id)?;
        // Ref fields pointing into it, here or in any other collection, follow (ADR 0024 §3).
        let (text, _) = collections::retarget_refs(&p.new_id, &text, &c.id, &p.new_id)?;
        let new = Collection::parse(&p.new_id, &text)?;
        let mut retargeted = Vec::new();
        for other in all_collections(ctx)?.into_iter().filter(|o| o.id != c.id) {
            let (changed_text, n) =
                collections::retarget_refs(&other.id, &collection_text(ctx, &other.id)?, &c.id, &p.new_id)?;
            if n > 0 {
                retargeted.push((collection_path(&other.id), changed_text));
            }
        }
        let moves = [
            subtree(ctx, &format!("items/{}", c.id), &format!("items/{}", p.new_id))?,
            subtree(ctx, &format!("trash/{}", c.id), &format!("trash/{}", p.new_id))?,
        ]
        .concat();
        ctx.store.transaction(|tx| {
            tx.put(&collection_path(&p.new_id), text)?;
            tx.delete(&collection_path(&c.id))?;
            apply_moves(tx, moves)?;
            for (path, text) in retargeted {
                tx.put(&path, text)?;
            }
            tx.emit("records.collection.renamed", json!({"collection": c.id, "new_id": p.new_id}))
        })?;
        Ok(serde_json::to_value(new).unwrap_or_default())
    }

    /// Copy a collection under a new id (ADR 0030 §3): its file with only the id (and label)
    /// changed, comments kept, and refs into itself pointed at the copy; with `with_records`, every
    /// live record too, as written. One transaction, one `records.item.created` per record.
    fn copy_collection(&self, ctx: &Ctx, p: CopyCollection) -> Result<Value> {
        let c = load_collection(ctx, &p.id)?;
        new_collection_id(&p.to)?;
        let mut text = collections::set_id(&c.id, &collection_text(ctx, &c.id)?, &p.to)?;
        let (retargeted, _) = collections::retarget_refs(&p.to, &text, &c.id, &p.to)?;
        text = retargeted;
        if let Some(label) = &p.label {
            text = relabel(&p.to, &text, label)?;
        }
        let copy = Collection::parse(&p.to, &text)?;

        let _g = self.write.lock();
        if ctx.store.read_string(&collection_path(&p.to))?.is_some() {
            return Err(Error::conflict(format!("there is already a collection '{}'", p.to)));
        }
        // Every record file as written, so nothing about a record changes but where it lives. One
        // that doesn't parse is copied too (`records check` reports it in both), with no event.
        let mut records = Vec::new();
        if p.with_records {
            let dir = format!("items/{}", c.id);
            for path in ctx.store.list(&dir)? {
                let Some(id) = path.strip_prefix(&format!("{dir}/")).and_then(|p| p.strip_suffix(".toml")) else {
                    continue;
                };
                let Some(text) = ctx.store.read_string(&path)? else { continue };
                let item = Item::from_toml(id, &text).ok();
                records.push((id.to_owned(), text, item));
            }
        }
        let count = records.len();
        ctx.store.transaction(|tx| {
            tx.put(&collection_path(&p.to), text)?;
            tx.emit("records.collection.copied", json!({"collection": p.to, "from": c.id, "records": count}))?;
            for (id, text, item) in &records {
                tx.put(&item_path(&p.to, id), text.as_str())?;
                if let Some(item) = item {
                    tx.emit("records.item.created", payload(&copy, item, &item.to_wire(&copy)))?;
                }
            }
            Ok(())
        })?;
        let mut out = serde_json::to_value(copy).unwrap_or_default();
        out["records"] = json!(count);
        Ok(out)
    }

    /// Move a collection and everything in it to `removed-collections/<id>/`, after the caller
    /// confirms the record count it was shown (ADR 0021 §5).
    fn remove_collection(&self, ctx: &Ctx, p: RemoveCollection) -> Result<Value> {
        if !is_valid_name(&p.id) || ctx.store.read_string(&collection_path(&p.id))?.is_none() {
            return Err(Error::not_found(format!("no collection '{}'", p.id)));
        }
        let _g = self.write.lock();
        let items = format!("items/{}", p.id);
        let records = ctx.store.list(&items)?.len();
        let expected = json!({"records": records});
        if p.confirm.as_ref() != Some(&expected) {
            return Err(Error::new(
                ErrorCode::ConfirmationRequired,
                format!("removing '{}' removes its {records} record(s); confirm with {{\"records\": {records}}}", p.id),
            )
            .with_detail(expected));
        }
        let to = format!("removed-collections/{}", p.id);
        let mut moves = vec![(collection_path(&p.id), format!("{to}/collection.toml"))];
        moves.extend(subtree(ctx, &items, &format!("{to}/items"))?);
        moves.extend(subtree(ctx, &format!("trash/{}", p.id), &format!("{to}/trash"))?);
        // A collection removed earlier under the same id is replaced: the latest wins, as in the
        // record trash (ADR 0020 §1).
        let stale = ctx.store.list(&to)?;
        ctx.store.transaction(|tx| {
            for path in &stale {
                tx.delete(path)?;
            }
            apply_moves(tx, moves)?;
            tx.emit("records.collection.removed", json!({"collection": p.id, "records": records}))
        })?;
        Ok(json!({"removed": true, "records": records}))
    }

    /// Bring a removed collection back with everything it had (ADR 0021 §5).
    fn restore_collection(&self, ctx: &Ctx, p: CollectionId) -> Result<Value> {
        let from = format!("removed-collections/{}", p.id);
        let missing = || Error::not_found(format!("no removed collection '{}'", p.id));
        if !is_valid_name(&p.id) {
            return Err(missing());
        }
        let _g = self.write.lock();
        let text = ctx.store.read_string(&format!("{from}/collection.toml"))?.ok_or_else(missing)?;
        if ctx.store.read_string(&collection_path(&p.id))?.is_some()
            || !ctx.store.list(&format!("items/{}", p.id))?.is_empty()
        {
            return Err(Error::conflict(format!("there is a collection '{}' again; rename it first", p.id)));
        }
        let c = Collection::parse(&p.id, &text)?;
        let mut moves = vec![(format!("{from}/collection.toml"), collection_path(&p.id))];
        moves.extend(subtree(ctx, &format!("{from}/items"), &format!("items/{}", p.id))?);
        moves.extend(subtree(ctx, &format!("{from}/trash"), &format!("trash/{}", p.id))?);
        ctx.store.transaction(|tx| {
            apply_moves(tx, moves)?;
            tx.emit("records.collection.restored", json!({"collection": p.id}))
        })?;
        Ok(serde_json::to_value(c).unwrap_or_default())
    }

    /// Add many records at once (ADR 0024 §4): each row checked as `records.add` checks one, and
    /// against the other rows; rows already there skipped; everything written in one transaction,
    /// or nothing when a row is invalid (unless `skip_invalid`) or on a dry run.
    fn import(&self, ctx: &Ctx, p: Import) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        if p.rows.len() > MAX_IMPORT_ROWS {
            return Err(Error::invalid_params(format!(
                "an import takes at most {MAX_IMPORT_ROWS} rows, got {}; split the file",
                p.rows.len()
            )));
        }
        let now = now(ctx);
        let _g = self.write.lock();

        // What's already there: ids, and the values unique fields hold.
        let dir = format!("items/{}", c.id);
        let mut taken_ids = std::collections::BTreeSet::new();
        let mut taken_values: Vec<(String, Value, String)> = Vec::new();
        for path in ctx.store.list(&dir)? {
            let Some(id) = path.strip_prefix(&format!("{dir}/")).and_then(|p| p.strip_suffix(".toml")) else {
                continue;
            };
            taken_ids.insert(id.to_owned());
            if let Some(Ok(record)) = ctx.store.read_string(&path)?.map(|text| Item::from_toml(id, &text)) {
                for (field, value) in c.unique_values(&record.fields_map()) {
                    taken_values.push((field.to_owned(), value.clone(), id.to_owned()));
                }
            }
        }

        let (mut added, mut skipped, mut invalid) = (Vec::new(), Vec::new(), Vec::new());
        let mut explicit = std::collections::BTreeSet::new();
        for (i, row) in p.rows.into_iter().enumerate() {
            let n = i + 1;
            let item = match import_row(ctx, &c, row, &now) {
                Ok(item) => item,
                Err(e) => {
                    invalid.push(json!({"row": n, "error": e.message}));
                    continue;
                }
            };
            let (item, given_id) = item;
            if let Some(id) = &given_id {
                if !explicit.insert(id.clone()) {
                    invalid.push(json!({"row": n, "error": format!("id '{id}' is in the import twice")}));
                    continue;
                }
                if taken_ids.contains(id) {
                    skipped.push(json!({"row": n, "reason": format!("'{}' already has a record '{id}'", c.id)}));
                    continue;
                }
            }
            let fields = item.fields_map();
            let clash = c.unique_values(&fields).into_iter().find_map(|(field, value)| {
                taken_values
                    .iter()
                    .find(|(f, v, _)| f == field && v == value)
                    .map(|(_, _, by)| (field, value, by.clone()))
            });
            if let Some((field, value, by)) = clash {
                let shown = value.as_str().map_or_else(|| value.to_string(), str::to_owned);
                skipped.push(json!({"row": n, "reason": format!("{field} '{shown}' is already used by '{by}'")}));
                continue;
            }
            let mut item = item;
            if given_id.is_none() {
                let base = c.render_title(&item.fields).map(|t| item::slug(&t)).filter(|s| !s.is_empty());
                let taken = |id: &str| Ok(taken_ids.contains(id) || explicit.contains(id));
                item.id = match base {
                    Some(base) => item::first_free(&base, false, taken)?,
                    None => item::first_free(&now.today().format("%Y-%m-%d").to_string(), true, taken)?,
                };
            }
            taken_ids.insert(item.id.clone());
            for (field, value) in c.unique_values(&fields) {
                taken_values.push((field.to_owned(), value.clone(), item.id.clone()));
            }
            added.push(item);
        }

        let ids: Vec<&str> = added.iter().map(|i| i.id.as_str()).collect();
        let write = !p.dry_run && (invalid.is_empty() || p.skip_invalid) && !added.is_empty();
        let out = json!({"added": ids, "skipped": skipped, "invalid": invalid, "written": write});
        if write {
            ctx.store.transaction(|tx| {
                for item in &added {
                    tx.put(&item_path(&c.id, &item.id), item.to_toml())?;
                    tx.emit("records.item.created", payload(&c, item, &item.to_wire(&c)))?;
                }
                Ok(())
            })?;
        }
        Ok(out)
    }

    /// Permanently delete trashed records, after the caller confirms how many it was shown, as
    /// removing a collection asks (ADR 0024 §5, ADR 0021 §5). This is the one op that can't be
    /// undone, so it never runs on a guess.
    fn purge(&self, ctx: &Ctx, p: Purge) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        let _g = self.write.lock();
        let paths = match &p.id {
            Some(id) => {
                item::check_id(id)?;
                let path = trash_path(&c.id, id);
                if ctx.store.read_string(&path)?.is_none() {
                    return Err(Error::not_found(format!("no removed record '{id}' in '{}'", c.id)));
                }
                vec![path]
            }
            None => ctx.store.list(&format!("trash/{}", c.id))?,
        };
        let records = paths.len();
        if records == 0 {
            return Ok(json!({"purged": 0}));
        }
        let expected = json!({"records": records});
        if p.confirm.as_ref() != Some(&expected) {
            let what = match &p.id {
                Some(id) => format!("'{id}' from '{}'s trash", c.id),
                None => format!("all {records} record(s) in '{}'s trash", c.id),
            };
            return Err(Error::new(
                ErrorCode::ConfirmationRequired,
                format!("purging permanently deletes {what}; confirm with {{\"records\": {records}}}"),
            )
            .with_detail(expected));
        }
        let mut event = json!({"collection": c.id, "records": records});
        if let Some(id) = &p.id {
            event["id"] = json!(id);
        }
        ctx.store.transaction(|tx| {
            for path in &paths {
                tx.delete(path)?;
            }
            tx.emit("records.trash.purged", event)
        })?;
        Ok(json!({"purged": records}))
    }

    /// Write `item` and emit `topic` with its payload, in one transaction (§7.1).
    fn save(&self, ctx: &Ctx, c: &Collection, item: &Item, topic: &str) -> Result<Value> {
        let wire = item.to_wire(c);
        ctx.store.transaction(|tx| {
            tx.put(&item_path(&c.id, &item.id), item.to_toml())?;
            tx.emit(topic, payload(c, item, &wire))
        })?;
        Ok(wire)
    }

    /// Move the record to the trash and say what went, in one transaction (ADR 0020 §1–2).
    fn remove(&self, ctx: &Ctx, t: Remove) -> Result<Value> {
        let c = load_collection(ctx, &t.collection)?;
        let _g = self.write.lock();
        let item = load_item(ctx, &c, &t.id)?;
        // Never quietly leave references pointing at nothing (ADR 0024 §3).
        let pointing: Vec<String> = referrers(ctx, &c.id, &t.id)?
            .into_iter()
            .filter(|(o, r)| !(o.id == c.id && r.id == t.id))
            .map(|(o, r)| format!("{}/{}", o.id, r.id))
            .collect();
        if !pointing.is_empty() && !t.force {
            let shown: Vec<&str> = pointing.iter().take(5).map(String::as_str).collect();
            let more = match pointing.len() > 5 {
                true => format!(" and {} more", pointing.len() - 5),
                false => String::new(),
            };
            return Err(Error::conflict(format!(
                "'{}' in '{}' is referred to by {}{more}; remove those first, or force it and leave them pointing at nothing",
                t.id,
                c.id,
                shown.join(", ")
            ))
            .with_detail(json!({"referred_by": pointing})));
        }
        let event = payload(&c, &item, &item.to_wire(&c));
        ctx.store.transaction(|tx| {
            tx.put(&trash_path(&c.id, &t.id), item.to_toml())?;
            tx.delete(&item_path(&c.id, &t.id))?;
            tx.emit("records.item.removed", event)
        })?;
        Ok(json!({"removed": true}))
    }

    /// Bring a removed record back exactly as it was (ADR 0020 §3).
    fn restore(&self, ctx: &Ctx, t: Target) -> Result<Value> {
        let c = load_collection(ctx, &t.collection)?;
        item::check_id(&t.id)?;
        let _g = self.write.lock();
        let text = ctx
            .store
            .read_string(&trash_path(&c.id, &t.id))?
            .ok_or_else(|| Error::not_found(format!("no removed record '{}' in '{}'", t.id, c.id)))?;
        if ctx.store.read_string(&item_path(&c.id, &t.id))?.is_some() {
            return Err(Error::conflict(format!(
                "'{}' already has a record '{}' again; rename one of them first",
                c.id, t.id
            )));
        }
        let item = Item::from_toml(&t.id, &text)?;
        check_unique(ctx, &c, &t.id, &item.fields_map())?;
        let wire = item.to_wire(&c);
        let event = payload(&c, &item, &wire);
        ctx.store.transaction(|tx| {
            tx.put(&item_path(&c.id, &t.id), text)?;
            tx.delete(&trash_path(&c.id, &t.id))?;
            tx.emit("records.item.restored", event)
        })?;
        Ok(wire)
    }

    /// The removed records that could be restored, by id (ADR 0020 §4).
    fn trash(&self, ctx: &Ctx, p: CollectionOnly) -> Result<Value> {
        let c = load_collection(ctx, &p.collection)?;
        let dir = format!("trash/{}", c.id);
        let mut items: Vec<Value> = Vec::new();
        for path in ctx.store.list(&dir)? {
            let Some(id) = path.strip_prefix(&format!("{dir}/")).and_then(|p| p.strip_suffix(".toml")) else {
                continue;
            };
            if let Some(Ok(item)) = ctx.store.read_string(&path)?.map(|text| Item::from_toml(id, &text)) {
                items.push(item.to_wire(&c));
            }
        }
        items.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        Ok(json!({"items": items}))
    }
}

/// An item event's payload: what changed, what to call it, and what is due (ADR 0017 §7), so a
/// listener needs nothing but the event.
fn payload(c: &Collection, item: &Item, wire: &Value) -> Value {
    json!({
        "collection": c.id,
        "id": item.id,
        "title": c.title_of(&item.id, &item.fields),
        "deadlines": c.deadlines_of(&item.fields),
        "item": wire,
    })
}

/// No other record in the collection already has a value a unique field in `values` would take
/// (ADR 0017 §4). Reads every record, like `records.list`; called under the write lock so two
/// requests can't both take a value. Unreadable files are skipped: they never block a write.
fn check_unique(ctx: &Ctx, c: &Collection, id: &str, values: &Map<String, Value>) -> Result<()> {
    let wanted = c.unique_values(values);
    if wanted.is_empty() {
        return Ok(());
    }
    let dir = format!("items/{}", c.id);
    for path in ctx.store.list(&dir)? {
        let Some(other) = path.strip_prefix(&format!("{dir}/")).and_then(|p| p.strip_suffix(".toml")) else {
            continue;
        };
        if other == id {
            continue;
        }
        let Some(Ok(record)) = ctx.store.read_string(&path)?.map(|text| Item::from_toml(other, &text)) else {
            continue;
        };
        for (field, value) in &wanted {
            if record.fields.get(*field) == Some(*value) {
                let shown = value.as_str().map_or_else(|| value.to_string(), str::to_owned);
                return Err(Error::conflict(format!("{field} '{shown}' is already used by '{other}' in '{}'", c.id)));
            }
        }
    }
    Ok(())
}

/// One import row as a record (with an empty id when it gave none), checked as `records.add`
/// checks: `id` and `status` are optional keys, everything else a field; `null` is unset.
fn import_row(ctx: &Ctx, c: &Collection, row: Value, now: &values::Now) -> Result<(Item, Option<String>)> {
    let Value::Object(mut fields) = row else {
        return Err(Error::invalid_params("a row must be an object of field values"));
    };
    let id = match fields.remove("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if id.is_empty() => None,
        Some(Value::String(id)) => {
            item::check_id(&id)?;
            Some(id)
        }
        Some(other) => return Err(Error::invalid_params(format!("id must be text, got {other}"))),
    };
    let status = match fields.remove("status") {
        None | Some(Value::Null) => item::Status::Todo,
        Some(v) if v == "todo" && c.completable => item::Status::Todo,
        Some(v) if v == "done" && c.completable => item::Status::Done,
        Some(v) => {
            return Err(Error::invalid_params(match c.completable {
                true => format!("status must be \"todo\" or \"done\", got {v}"),
                false => format!("'{}' has no status (completable = false)", c.id),
            }))
        }
    };
    fields.retain(|_, v| !v.is_null() && v.as_str() != Some(""));
    values::normalize(c, &mut fields, now)?;
    fields.retain(|_, v| !v.is_null());
    c.check_values(&fields)?;
    c.check_required(&fields)?;
    check_refs(ctx, c, &fields)?;
    let mut item = Item::new(id.as_deref().unwrap_or_default(), fields);
    item.status = status;
    item.status_line = c.completable;
    Ok((item, id))
}

/// Every ref in `values` points at a record that exists (ADR 0024 §3).
fn check_refs(ctx: &Ctx, c: &Collection, values: &Map<String, Value>) -> Result<()> {
    match dangling(ctx, c, values)?.into_iter().next() {
        Some(problem) => Err(Error::invalid_params(problem)),
        None => Ok(()),
    }
}

/// What's wrong with the refs in `values`: a target collection that doesn't exist, or a record
/// that isn't in it.
fn dangling<'a>(
    ctx: &Ctx,
    c: &Collection,
    values: impl IntoIterator<Item = (&'a String, &'a Value)>,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (field, target, id) in c.refs_in(values) {
        if ctx.store.read_string(&collection_path(target))?.is_none() {
            out.push(format!(
                "field '{field}' points into collection '{target}', but there is no {}",
                collection_path(target)
            ));
        } else if item::check_id(id).is_err() || ctx.store.read_string(&item_path(target, id))?.is_none() {
            out.push(format!("field '{field}': no record '{id}' in '{target}'"));
        }
    }
    Ok(out)
}

/// Every record, in any collection, with a ref to `id` in `target`, with its collection.
/// Unreadable collections and records are skipped, as in `records.collections` and `list`.
fn referrers(ctx: &Ctx, target: &str, id: &str) -> Result<Vec<(Collection, Item)>> {
    let mut out = Vec::new();
    for c in all_collections(ctx)? {
        if c.ref_fields_to(target).next().is_none() {
            continue;
        }
        let dir = format!("items/{}", c.id);
        for path in ctx.store.list(&dir)? {
            let Some(rid) = path.strip_prefix(&format!("{dir}/")).and_then(|p| p.strip_suffix(".toml")) else {
                continue;
            };
            let Some(Ok(record)) = ctx.store.read_string(&path)?.map(|text| Item::from_toml(rid, &text)) else {
                continue;
            };
            if c.refs_in(&record.fields).iter().any(|(_, t, i)| *t == target && *i == id) {
                out.push((c.clone(), record));
            }
        }
    }
    Ok(out)
}

/// Every collection that parses.
fn all_collections(ctx: &Ctx) -> Result<Vec<Collection>> {
    let mut out = Vec::new();
    for path in ctx.store.list("collections")? {
        let Some(id) = path.strip_prefix("collections/").and_then(|p| p.strip_suffix(".toml")) else { continue };
        if let Ok(c) = load_collection(ctx, id) {
            out.push(c);
        }
    }
    Ok(out)
}

/// The current moment in the configured timezone (ADR 0009, ADR 0024 §1). "Today" comes from
/// here, so a late-evening action west of UTC never counts as tomorrow. The zone is looked up by
/// the name `core` already parsed, so it can't fail; UTC is only a formality.
fn now(ctx: &Ctx) -> values::Now {
    let tz = ctx.local_tz.name().parse().unwrap_or(chrono_tz::UTC);
    values::Now { at: ctx.clock.now(), tz }
}

fn load_collection(ctx: &Ctx, id: &str) -> Result<Collection> {
    let missing = || Error::not_found(format!("no collection '{id}'"));
    if !is_valid_name(id) {
        return Err(missing());
    }
    let text = ctx.store.read_string(&collection_path(id))?.ok_or_else(missing)?;
    Collection::parse(id, &text)
}

fn load_item(ctx: &Ctx, c: &Collection, id: &str) -> Result<Item> {
    item::check_id(id)?;
    match ctx.store.read_string(&item_path(&c.id, id))? {
        Some(text) => Item::from_toml(id, &text),
        None => Err(Error::not_found(format!("no record '{id}' in '{}'", c.id))),
    }
}

/// A record file as an item, for the ops that scan a collection (`list`, `check`). A file that
/// can't be read at all (not UTF-8, an I/O error) is an `Err` for that one record, like one that
/// doesn't parse, so it never hides the rest. `None` when it's gone.
fn read_item(ctx: &Ctx, path: &str, id: &str) -> Option<Result<Item>> {
    match ctx.store.read_string(path) {
        Ok(text) => text.map(|text| Item::from_toml(id, &text)),
        Err(e) => Some(Err(Error::invalid_params(format!("record file '{id}.toml' can't be read: {}", e.message)))),
    }
}

fn collection_path(id: &str) -> String {
    format!("collections/{id}.toml")
}

/// The collection file as written, for edits that keep its comments (ADR 0021).
fn collection_text(ctx: &Ctx, id: &str) -> Result<String> {
    ctx.store.read_string(&collection_path(id))?.ok_or_else(|| Error::not_found(format!("no collection '{id}'")))
}

/// Every file under `from`, paired with where it goes under `to`, contents read now.
fn subtree(ctx: &Ctx, from: &str, to: &str) -> Result<Vec<(String, String)>> {
    let prefix = format!("{from}/");
    Ok(ctx
        .store
        .list(from)?
        .into_iter()
        .filter_map(|path| path.strip_prefix(&prefix).map(|rest| (path.clone(), format!("{to}/{rest}"))))
        .collect())
}

/// Move files inside a transaction: write each at its new path, delete the old one.
fn apply_moves(tx: &mut shimmer_core::Tx<'_>, moves: Vec<(String, String)>) -> Result<()> {
    for (from, to) in moves {
        let bytes = tx.read(&from)?.unwrap_or_default();
        tx.put(&to, bytes)?;
        tx.delete(&from)?;
    }
    Ok(())
}

/// Where a removed record waits to be restored (ADR 0020 §1).
fn trash_path(collection: &str, id: &str) -> String {
    format!("trash/{collection}/{id}.toml")
}

fn item_path(collection: &str, id: &str) -> String {
    format!("items/{collection}/{id}.toml")
}
