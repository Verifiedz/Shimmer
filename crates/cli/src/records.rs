//! `shimmer records …`: typed commands over the `records.*` ops (ADR 0008).
//!
//! Nothing here knows about LeetCode or any other collection. Field flags (`--title`,
//! `--company`) and their types come from the daemon at runtime (`records.collections`), so a
//! collection added as a TOML file gets its own flags with no client change (§9).

use std::fmt::Write as _;

use serde_json::{json, Map, Value};
use shimmer_core::{Error, ErrorCode, Result};

use crate::client::Client;
use crate::csv;
use crate::render::{self, cell, table, wrap};
use crate::workspaces::Prompt;

pub const USAGE: &str = "usage: shimmer records <command>

commands:
  collections                          list collections and their fields
  templates [CATEGORY]                 list ready-made collections to start from, by category
  peek TEMPLATE [--file]               everything about one template: its fields, what completing
                                       does, examples; --file prints the file exactly
  new ID --from TEMPLATE [--label TEXT] create a collection from a template, e.g.
                                         shimmer records new jobs --from job-applications
  add COLLECTION [ID] [--FIELD VALUE]… add a record, e.g.
                                         shimmer records add leetcode --title \"Two Sum\" --difficulty easy
                                       without an ID, one is made from its title (two-sum)
  list COLLECTION [--FIELD VALUE]…     list records; each --FIELD filters on an exact value
       [--status todo|done] [--limit N] [--offset N]
       [--filter \"FIELD OP VALUE\"]…        OP: = != < <= > >= ~ (contains); a|b|c after = is one-of;
                                           \"FIELD has VALUE\" for list fields
       [--or \"FIELD OP VALUE\"]…            any one of these holds, e.g. --or stage=offer --or priority=dream
       [--has FIELD] [--missing FIELD]     has a value / has none
                                           dates may be today, today+7, today-30
       [--search TEXT]                     text in the id or any text field
       [--sort FIELD]… [--sort -FIELD]     order, e.g. --sort oa_deadline --sort -applied_on
  get COLLECTION/ID                    show one record
  update COLLECTION/ID [--FIELD VALUE]… [--unset FIELD]…
       [--add FIELD VALUE]… [--remove FIELD VALUE]…  change items of a list field
  complete COLLECTION/ID [--FIELD VALUE]… [--unset FIELD]…
                                       mark done and stamp the collection's date field;
                                       a value for the date field back-dates it
  reopen COLLECTION/ID [--clear-stamp] mark a done record todo again
  rename COLLECTION/ID NEW_ID          give a record a new id
  remove COLLECTION/ID [--force]       remove a record (it goes to the trash); --force even
                                       if other records refer to it
  restore COLLECTION/ID                bring a removed record back
  trash COLLECTION                     list removed records you can restore
  purge COLLECTION[/ID] [--yes]        permanently delete removed records (asks first)

moving data:
  import COLLECTION FILE               add records from a .csv (a header row of field names;
       [--dry-run] [--skip-invalid]    list items split by ';') or .json (an array of objects);
       [--yes]                         shows what it would do, then asks
  export COLLECTION [--format csv|json] [list options]
                                       write records to stdout; imports back unchanged

changing a collection:
  check COLLECTION                     list records that don't fit the collection
  rename-field COLLECTION FROM TO      rename a field in the collection and every record
  rename-collection ID NEW_ID          give a collection a new id
  remove-collection ID [--yes]         remove a collection and its records (asks first;
                                       restore-collection brings it back)
  restore-collection ID                bring a removed collection back

A record can be written COLLECTION/ID or COLLECTION ID. Field names come from the
collection: see 'shimmer records collections'. Add --json to any command for raw output.";

/// `--name value` pairs, in the order given.
pub type Flags = Vec<(String, String)>;

#[derive(Clone, Debug, PartialEq)]
pub enum RecordsCmd {
    Help,
    Collections,
    /// `id` is `None` when the daemon should make one from the title (ADR 0018).
    Add {
        collection: String,
        id: Option<String>,
        set: Flags,
    },
    Rename {
        collection: String,
        id: String,
        new_id: String,
    },
    List {
        collection: String,
        filter: Flags,
        limit: Option<usize>,
        offset: Option<usize>,
        refine: Refine,
    },
    Get {
        collection: String,
        id: String,
    },
    Update {
        collection: String,
        id: String,
        set: Flags,
        unset: Vec<String>,
        items: Vec<ItemEdit>,
    },
    Complete {
        collection: String,
        id: String,
        set: Flags,
        unset: Vec<String>,
        items: Vec<ItemEdit>,
    },
    Reopen {
        collection: String,
        id: String,
        clear_stamp: bool,
    },
    /// `force`: remove it even if other records refer to it (ADR 0024 §3).
    Remove {
        collection: String,
        id: String,
        force: bool,
    },
    Restore {
        collection: String,
        id: String,
    },
    Trash {
        collection: String,
    },
    Check {
        collection: String,
    },
    /// `which`: a category (only its templates) or a template (as `peek` shows it).
    Templates {
        which: Option<String>,
    },
    /// `file`: the template's file exactly, instead of the summary.
    Peek {
        template: String,
        file: bool,
    },
    New {
        id: String,
        template: String,
        label: Option<String>,
    },
    RenameField {
        collection: String,
        from: String,
        to: String,
    },
    RenameCollection {
        id: String,
        new_id: String,
    },
    /// `yes`: confirm without asking (`--yes`).
    RemoveCollection {
        id: String,
        yes: bool,
    },
    RestoreCollection {
        id: String,
    },
    /// `id`: one trashed record; `None`: the whole trash (ADR 0024 §5).
    Purge {
        collection: String,
        id: Option<String>,
        yes: bool,
    },
    /// ADR 0024 §4: the CLI reads `file`; the daemon checks and writes.
    Import {
        collection: String,
        file: String,
        dry_run: bool,
        skip_invalid: bool,
        yes: bool,
    },
    Export {
        collection: String,
        format: Format,
        filter: Flags,
        refine: Refine,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    Csv,
    Json,
}

/// `--add FIELD VALUE` / `--remove FIELD VALUE`: change items of a list field without resending
/// the rest (ADR 0024 §2). VALUE may hold several items, comma-separated.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemEdit {
    pub add: bool,
    pub field: String,
    pub value: String,
}

/// What `records list` asks beyond exact `--FIELD VALUE` matches (ADR 0019).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Refine {
    /// `--filter "FIELD OP VALUE"`, as written.
    pub conditions: Vec<String>,
    pub has: Vec<String>,
    pub missing: Vec<String>,
    pub search: Option<String>,
    /// `--sort FIELD` / `--sort -FIELD`, in order.
    pub sort: Vec<String>,
    /// `--or "FIELD OP VALUE"`: a record matches if any of these holds (ADR 0024 §5).
    pub or: Vec<String>,
}

// ---------------------------------------------------------------- parsing

/// `words` are what follows `shimmer records`, with global flags already removed.
pub fn parse(words: Vec<String>) -> std::result::Result<RecordsCmd, String> {
    let mut words = words.into_iter();
    let Some(sub) = words.next() else { return Ok(RecordsCmd::Help) };
    let mut rest: Vec<String> = words.collect();
    // `--clear-stamp` is the one flag here that takes no value.
    let clear_stamp = take_flag(&mut rest, sub == "reopen", "--clear-stamp");
    // `--add` and `--remove` take two words, a field and a value, so they come out before `split`.
    let mut items = Vec::new();
    if sub == "update" || sub == "complete" {
        let mut kept = Vec::new();
        let mut words = rest.into_iter();
        while let Some(word) = words.next() {
            let add = match word.as_str() {
                "--add" => true,
                "--remove" => false,
                _ => {
                    kept.push(word);
                    continue;
                }
            };
            match (words.next(), words.next()) {
                (Some(field), Some(value)) => items.push(ItemEdit { add, field, value }),
                _ => return Err(format!("{word} needs a field and a value, e.g. {word} tech_stack rust")),
            }
        }
        rest = kept;
    }
    let force = take_flag(&mut rest, sub == "remove", "--force");
    let yes = take_flag(&mut rest, matches!(sub.as_str(), "remove-collection" | "import" | "purge"), "--yes");
    let file = take_flag(&mut rest, sub == "peek", "--file");
    let dry_run = take_flag(&mut rest, sub == "import", "--dry-run");
    let skip_invalid = take_flag(&mut rest, sub == "import", "--skip-invalid");
    let (positional, flags) = split(rest)?;
    let no_flags = || match flags.first() {
        Some((f, _)) => Err(format!("'records {sub}' takes no options, got '--{f}'")),
        None => Ok(()),
    };
    match sub.as_str() {
        "help" => Ok(RecordsCmd::Help),
        "collections" => {
            no_flags()?;
            match positional.is_empty() {
                true => Ok(RecordsCmd::Collections),
                false => Err("'records collections' takes no arguments".into()),
            }
        }
        "add" => {
            // `COLLECTION` alone (no `/`) leaves the id to the daemon (ADR 0018).
            let (collection, id) = match positional.as_slice() {
                [one] if !one.contains('/') => (one.clone(), None),
                _ => target(&sub, positional).map(|(c, id)| (c, Some(id)))?,
            };
            Ok(RecordsCmd::Add { collection, id, set: flags })
        }
        "rename" => {
            no_flags()?;
            let usage = "usage: shimmer records rename COLLECTION/ID NEW_ID";
            let (target_words, new_id) = match positional.split_last() {
                Some((new_id, rest)) if !rest.is_empty() => (rest.to_vec(), new_id.clone()),
                _ => return Err(usage.into()),
            };
            let (collection, id) = target(&sub, target_words).map_err(|_| usage.to_owned())?;
            Ok(RecordsCmd::Rename { collection, id, new_id })
        }
        "purge" => {
            no_flags()?;
            let (collection, id) = match positional.as_slice() {
                [one] if !one.contains('/') => (one.clone(), None),
                _ => target(&sub, positional)
                    .map(|(c, id)| (c, Some(id)))
                    .map_err(|_| "usage: shimmer records purge COLLECTION[/ID] [--yes]".to_owned())?,
            };
            Ok(RecordsCmd::Purge { collection, id, yes })
        }
        "import" => {
            no_flags()?;
            let usage = "usage: shimmer records import COLLECTION FILE [--dry-run] [--skip-invalid] [--yes]";
            let [collection, file]: [String; 2] = positional.try_into().map_err(|_| usage)?;
            Ok(RecordsCmd::Import { collection, file, dry_run, skip_invalid, yes })
        }
        "list" | "export" => {
            let [collection]: [String; 1] = positional
                .try_into()
                .map_err(|_| format!("usage: shimmer records {sub} COLLECTION [--FIELD VALUE]…"))?;
            let (mut filter, mut limit, mut offset, mut refine) = (Vec::new(), None, None, Refine::default());
            let mut format = Format::Csv;
            for (name, value) in flags {
                match name.as_str() {
                    "format" if sub == "export" => {
                        format = match value.as_str() {
                            "csv" => Format::Csv,
                            "json" => Format::Json,
                            other => return Err(format!("--format is csv or json, not '{other}'")),
                        }
                    }
                    "limit" | "offset" if sub == "export" => {
                        return Err(
                            "'records export' writes every matching record; it takes no --limit or --offset".into()
                        )
                    }
                    "limit" => limit = Some(number("--limit", &value)?),
                    "offset" => offset = Some(number("--offset", &value)?),
                    "filter" => refine.conditions.push(value),
                    "has" => refine.has.push(value),
                    "missing" => refine.missing.push(value),
                    "search" => refine.search = Some(value),
                    "sort" => refine.sort.push(value),
                    "or" => refine.or.push(value),
                    _ => filter.push((name, value)),
                }
            }
            if sub == "export" {
                return Ok(RecordsCmd::Export { collection, format, filter, refine });
            }
            Ok(RecordsCmd::List { collection, filter, limit, offset, refine })
        }
        "update" | "complete" => {
            let (collection, id) = target(&sub, positional)?;
            let (unset, set) = flags.into_iter().partition::<Vec<_>, _>(|(name, _)| name == "unset");
            let unset: Vec<String> = unset.into_iter().map(|(_, field)| field).collect();
            if sub == "complete" {
                return Ok(RecordsCmd::Complete { collection, id, set, unset, items });
            }
            if set.is_empty() && unset.is_empty() && items.is_empty() {
                return Err("nothing to update: give --FIELD VALUE, --unset FIELD or --add/--remove FIELD VALUE".into());
            }
            Ok(RecordsCmd::Update { collection, id, set, unset, items })
        }
        "templates" => {
            no_flags()?;
            let mut words = positional.into_iter();
            match (words.next(), words.next()) {
                (which, None) => Ok(RecordsCmd::Templates { which }),
                _ => Err("usage: shimmer records templates [CATEGORY | TEMPLATE]".into()),
            }
        }
        "peek" => {
            let usage = "usage: shimmer records peek TEMPLATE [--file] (see 'shimmer records templates')";
            if let Some((f, _)) = flags.first() {
                return Err(format!("'records peek' takes --file, not '--{f}'; {usage}"));
            }
            let [template]: [String; 1] = positional.try_into().map_err(|_| usage)?;
            Ok(RecordsCmd::Peek { template, file })
        }
        "new" => {
            let usage = "usage: shimmer records new ID --from TEMPLATE [--label TEXT]";
            let [id]: [String; 1] = positional.try_into().map_err(|_| usage)?;
            let (mut template, mut label) = (None, None);
            for (name, value) in flags {
                match name.as_str() {
                    "from" => template = Some(value),
                    "label" => label = Some(value),
                    other => return Err(format!("'records new' takes --from and --label, not '--{other}'")),
                }
            }
            let template = template.ok_or("records new needs --from TEMPLATE; see 'shimmer records templates'")?;
            Ok(RecordsCmd::New { id, template, label })
        }
        "check" | "rename-field" | "rename-collection" | "remove-collection" | "restore-collection" => {
            no_flags()?;
            let usage = match sub.as_str() {
                "check" => "usage: shimmer records check COLLECTION",
                "rename-field" => "usage: shimmer records rename-field COLLECTION FROM TO",
                "rename-collection" => "usage: shimmer records rename-collection ID NEW_ID",
                "remove-collection" => "usage: shimmer records remove-collection ID [--yes]",
                _ => "usage: shimmer records restore-collection ID",
            };
            let p = positional;
            Ok(match (sub.as_str(), p.as_slice()) {
                ("check", [c]) => RecordsCmd::Check { collection: c.clone() },
                ("rename-field", [c, from, to]) => {
                    RecordsCmd::RenameField { collection: c.clone(), from: from.clone(), to: to.clone() }
                }
                ("rename-collection", [id, new_id]) => {
                    RecordsCmd::RenameCollection { id: id.clone(), new_id: new_id.clone() }
                }
                ("remove-collection", [id]) => RecordsCmd::RemoveCollection { id: id.clone(), yes },
                ("restore-collection", [id]) => RecordsCmd::RestoreCollection { id: id.clone() },
                _ => return Err(usage.into()),
            })
        }
        "trash" => {
            no_flags()?;
            let [collection]: [String; 1] =
                positional.try_into().map_err(|_| "usage: shimmer records trash COLLECTION")?;
            Ok(RecordsCmd::Trash { collection })
        }
        "get" | "remove" | "reopen" | "restore" => {
            no_flags()?;
            let (collection, id) = target(&sub, positional)?;
            Ok(match sub.as_str() {
                "get" => RecordsCmd::Get { collection, id },
                "reopen" => RecordsCmd::Reopen { collection, id, clear_stamp },
                "restore" => RecordsCmd::Restore { collection, id },
                _ => RecordsCmd::Remove { collection, id, force },
            })
        }
        other => Err(format!("unknown records command '{other}'; see 'shimmer records --help'")),
    }
}

/// Whether `flag` (one that takes no value) was given to a command it `applies` to, removing it
/// from `rest` so `split` doesn't take the next word as its value. Elsewhere it stays, and
/// `split` reports it.
fn take_flag(rest: &mut Vec<String>, applies: bool, flag: &str) -> bool {
    let given = applies && rest.iter().any(|w| w == flag);
    if given {
        rest.retain(|w| w != flag);
    }
    given
}

/// Separate positional words from `--name value` / `--name=value` pairs. Every flag here
/// takes a value, so the word after a flag is always its value, even if it starts with `-`.
fn split(words: Vec<String>) -> std::result::Result<(Vec<String>, Flags), String> {
    let (mut positional, mut flags) = (Vec::new(), Vec::new());
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let Some(flag) = word.strip_prefix("--") else {
            if word.starts_with('-') && word.len() > 1 {
                return Err(format!("unknown option '{word}'"));
            }
            positional.push(word);
            continue;
        };
        let (name, value) = match flag.split_once('=') {
            Some((n, v)) => (n.to_owned(), v.to_owned()),
            None => (flag.to_owned(), words.next().ok_or(format!("--{flag} needs a value"))?),
        };
        if name.is_empty() {
            return Err("'--' is not an option".into());
        }
        flags.push((name, value));
    }
    Ok((positional, flags))
}

/// `COLLECTION/ID` or `COLLECTION ID`.
fn target(sub: &str, positional: Vec<String>) -> std::result::Result<(String, String), String> {
    let usage = || format!("usage: shimmer records {sub} COLLECTION/ID");
    match positional.as_slice() {
        [one] => match one.split_once('/') {
            Some((c, id)) if !c.is_empty() && !id.is_empty() => Ok((c.to_owned(), id.to_owned())),
            _ => Err(usage()),
        },
        [c, id] => Ok((c.clone(), id.clone())),
        _ => Err(usage()),
    }
}

fn number(flag: &str, value: &str) -> std::result::Result<usize, String> {
    value.parse().map_err(|_| format!("{flag} expects a whole number, got '{value}'"))
}

// ---------------------------------------------------------------- requests

impl RecordsCmd {
    fn collection(&self) -> Option<&str> {
        match self {
            Self::Help | Self::Collections | Self::Templates { .. } | Self::Peek { .. } | Self::New { .. } => None,
            Self::Add { collection, .. }
            | Self::Rename { collection, .. }
            | Self::List { collection, .. }
            | Self::Get { collection, .. }
            | Self::Update { collection, .. }
            | Self::Complete { collection, .. }
            | Self::Reopen { collection, .. }
            | Self::Remove { collection, .. }
            | Self::Restore { collection, .. }
            | Self::Trash { collection }
            | Self::Check { collection }
            | Self::RenameField { collection, .. }
            | Self::Import { collection, .. }
            | Self::Purge { collection, .. }
            | Self::Export { collection, .. } => Some(collection),
            Self::RenameCollection { .. } | Self::RemoveCollection { .. } | Self::RestoreCollection { .. } => None,
        }
    }
}

/// The op and params for `cmd`. `schema` is the collection's definition from
/// `records.collections`, or `Null` when there is none: then values go as plain strings and
/// the daemon, which owns validation, reports what is wrong.
pub fn request(cmd: &RecordsCmd, schema: &Value) -> Result<(&'static str, Value)> {
    let fields = |pairs: &[(String, String)]| -> Result<Map<String, Value>> {
        pairs.iter().map(|(name, raw)| typed(schema, name, raw)).collect()
    };
    // `--FIELD VALUE` sets, `--unset FIELD` sends `null`, `--add`/`--remove` a list patch.
    let changes = |set: &[(String, String)], unset: &[String], items: &[ItemEdit]| -> Result<Map<String, Value>> {
        let mut changes = fields(set)?;
        for name in unset {
            changes.insert(field_name(schema, name), Value::Null);
        }
        for edit in items {
            let name = field_name(schema, &edit.field);
            let patch = changes.entry(name.clone()).or_insert_with(|| json!({}));
            let Some(patch) = patch.as_object_mut() else {
                return Err(Error::invalid_params(format!("'{name}' is both set and changed by item; pick one")));
            };
            let key = if edit.add { "add" } else { "remove" };
            let list = patch.entry(key).or_insert_with(|| json!([])).as_array_mut().expect("always a list");
            list.extend(split_list(&edit.value).into_iter().map(Value::String));
        }
        Ok(changes)
    };
    Ok(match cmd {
        RecordsCmd::Help => return Err(Error::internal("help is not a request")),
        RecordsCmd::Import { .. } => return Err(Error::internal("import reads its file in run")),
        RecordsCmd::Purge { collection, id, .. } => {
            let mut params = json!({"collection": collection});
            if let Some(id) = id {
                params["id"] = json!(id);
            }
            ("records.purge", params)
        }
        // One page of `records.list`; `run` pages through them all.
        RecordsCmd::Export { collection, filter, refine, .. } => {
            let list = RecordsCmd::List {
                collection: collection.clone(),
                filter: filter.clone(),
                limit: Some(EXPORT_PAGE),
                offset: None,
                refine: refine.clone(),
            };
            return request(&list, schema);
        }
        RecordsCmd::Collections => ("records.collections", json!({})),
        RecordsCmd::Add { collection, id, set } => {
            let mut params = json!({"collection": collection, "fields": fields(set)?});
            if let Some(id) = id {
                params["id"] = json!(id);
            }
            ("records.add", params)
        }
        RecordsCmd::Rename { collection, id, new_id } => {
            ("records.rename", json!({"collection": collection, "id": id, "new_id": new_id}))
        }
        RecordsCmd::List { collection, filter, limit, offset, refine } => {
            // On a list field, `--stack rust` means "has rust": the daemon reads a plain value so.
            let exact = filter
                .iter()
                .map(|(flag, raw)| match is_list(schema, &field_name(schema, flag)) {
                    true => Ok((field_name(schema, flag), json!(raw))),
                    false => typed(schema, flag, raw),
                })
                .collect::<Result<Map<String, Value>>>()?;
            let mut params = json!({"collection": collection, "filter": query_filter(schema, &exact, refine)?});
            if let Some(text) = &refine.search {
                params["search"] = json!(text);
            }
            if !refine.sort.is_empty() {
                let keys: Vec<String> = refine
                    .sort
                    .iter()
                    .map(|k| match k.strip_prefix('-') {
                        Some(name) => format!("-{}", field_name(schema, name)),
                        None => field_name(schema, k),
                    })
                    .collect();
                params["sort"] = json!(keys);
            }
            if let Some(n) = limit {
                params["limit"] = json!(n);
            }
            if let Some(n) = offset {
                params["offset"] = json!(n);
            }
            ("records.list", params)
        }
        RecordsCmd::Get { collection, id } => ("records.get", json!({"collection": collection, "id": id})),
        RecordsCmd::Update { collection, id, set, unset, items } => {
            ("records.update", json!({"collection": collection, "id": id, "fields": changes(set, unset, items)?}))
        }
        RecordsCmd::Complete { collection, id, set, unset, items } => {
            let mut params = json!({"collection": collection, "id": id});
            let changes = changes(set, unset, items)?;
            if !changes.is_empty() {
                params["fields"] = Value::Object(changes);
            }
            ("records.complete", params)
        }
        RecordsCmd::Reopen { collection, id, clear_stamp } => {
            let mut params = json!({"collection": collection, "id": id});
            if *clear_stamp {
                params["clear_stamp"] = json!(true);
            }
            ("records.reopen", params)
        }
        RecordsCmd::Remove { collection, id, force } => {
            let mut params = json!({"collection": collection, "id": id});
            if *force {
                params["force"] = json!(true);
            }
            ("records.remove", params)
        }
        RecordsCmd::Restore { collection, id } => ("records.restore", json!({"collection": collection, "id": id})),
        RecordsCmd::Trash { collection } => ("records.trash", json!({"collection": collection})),
        RecordsCmd::Check { collection } => ("records.check", json!({"collection": collection})),
        RecordsCmd::Templates { .. } => ("records.templates", json!({})),
        RecordsCmd::Peek { template, file: true } => ("records.templates", json!({"id": template, "file": true})),
        RecordsCmd::Peek { .. } => ("records.templates", json!({})),
        RecordsCmd::New { id, template, label } => {
            let mut params = json!({"id": id, "template": template});
            if let Some(label) = label {
                params["label"] = json!(label);
            }
            ("records.create_collection", params)
        }
        RecordsCmd::RenameField { collection, from, to } => (
            "records.rename_field",
            json!({"collection": collection, "from": field_name(schema, from), "to": to.replace('-', "_")}),
        ),
        RecordsCmd::RenameCollection { id, new_id } => {
            ("records.rename_collection", json!({"id": id, "new_id": new_id}))
        }
        RecordsCmd::RemoveCollection { id, .. } => ("records.remove_collection", json!({"id": id})),
        RecordsCmd::RestoreCollection { id } => ("records.restore_collection", json!({"id": id})),
    })
}

/// The `filter` param for `records list` (ADR 0019): exact `--FIELD VALUE` matches, then each
/// `--filter`, `--has` and `--missing` as operators. Two conditions on one field combine into one
/// operator object (`>=` and `<=` make a range).
fn query_filter(schema: &Value, exact: &Map<String, Value>, refine: &Refine) -> Result<Map<String, Value>> {
    let mut out = exact.clone();
    let mut add = |name: String, op: &str, value: Value| {
        let entry = out.entry(name).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({"eq": entry.take()});
        }
        entry[op] = value;
    };
    for raw in &refine.conditions {
        let (name, op, value) = condition(schema, raw)?;
        add(name, op, value);
    }
    for name in &refine.has {
        add(field_name(schema, name), "set", json!(true));
    }
    for name in &refine.missing {
        add(field_name(schema, name), "set", json!(false));
    }
    if !refine.or.is_empty() {
        let alternatives = refine
            .or
            .iter()
            .map(|raw| condition(schema, raw).map(|(name, op, value)| json!({name: {op: value}})))
            .collect::<Result<Vec<_>>>()?;
        out.insert("or".into(), json!(alternatives));
    }
    Ok(out)
}

/// `oa_deadline<=2026-10-10` → (`oa_deadline`, `lte`, `"2026-10-10"`). `stage=oa|interview` is
/// `in`; `company~goog` is `contains`. The value is typed from the field, like `--FIELD VALUE`.
fn condition(schema: &Value, raw: &str) -> Result<(String, &'static str, Value)> {
    const OPS: [(&str, &str); 7] =
        [("<=", "lte"), (">=", "gte"), ("!=", "ne"), ("<", "lt"), (">", "gt"), ("=", "eq"), ("~", "contains")];
    let bad = || {
        Error::invalid_params(format!(
            "--filter '{raw}': write FIELD OP VALUE with OP one of = != < <= > >= ~ has, e.g. --filter \"oa_deadline<=2026-10-10\""
        ))
    };
    // `tech_stack has rust` (ADR 0024 §2).
    if let Some((flag, value)) = raw.split_once(" has ") {
        let (flag, value) = (flag.trim(), value.trim());
        if flag.is_empty() || value.is_empty() {
            return Err(bad());
        }
        return Ok((field_name(schema, flag), "has", json!(value)));
    }
    let at = raw.find(['<', '>', '!', '=', '~']).ok_or_else(bad)?;
    let (flag, rest) = (raw[..at].trim(), &raw[at..]);
    let (symbol, op) = OPS.iter().find(|(symbol, _)| rest.starts_with(symbol)).ok_or_else(bad)?;
    let value = rest[symbol.len()..].trim();
    if flag.is_empty() || value.is_empty() {
        return Err(bad());
    }
    // A list holds several items, so `a|b` would be ambiguous: say how to ask for either one
    // (Dev A's review of #93).
    if is_list(schema, &field_name(schema, flag)) && value.contains('|') {
        let either: Vec<String> = value.split('|').map(|v| format!("--or \"{flag} has {}\"", v.trim())).collect();
        return Err(Error::invalid_params(format!(
            "--filter '{raw}': {flag} is a list; for any of these items write {}",
            either.join(" ")
        )));
    }
    if *op == "eq" && value.contains('|') {
        let items =
            value.split('|').map(|v| typed(schema, flag, v.trim()).map(|(_, v)| v)).collect::<Result<Vec<_>>>()?;
        return Ok((field_name(schema, flag), "in", json!(items)));
    }
    // `contains` always takes text, whatever the field's type; so does a list's `=` (`has`).
    if is_list(schema, &field_name(schema, flag)) && *op == "eq" {
        return Ok((field_name(schema, flag), "has", json!(value)));
    }
    let (name, value) = match *op {
        "contains" => (field_name(schema, flag), json!(value)),
        _ => typed(schema, flag, value)?,
    };
    Ok((name, op, value))
}

/// `--last-solved` finds a field named `last_solved`; an exact match wins.
fn field_name(schema: &Value, flag: &str) -> String {
    let known = |n: &str| field(schema, n).is_some();
    if known(flag) {
        return flag.to_owned();
    }
    let underscored = flag.replace('-', "_");
    if known(&underscored) {
        underscored
    } else {
        flag.to_owned()
    }
}

fn field<'a>(schema: &'a Value, name: &str) -> Option<&'a Value> {
    schema["fields"].as_array()?.iter().find(|f| f["name"] == name)
}

/// A command-line value as the JSON type its field declares. `status` and `id` are strings.
fn typed(schema: &Value, flag: &str, raw: &str) -> Result<(String, Value)> {
    let name = field_name(schema, flag);
    let kind = field(schema, &name).and_then(|f| f["type"].as_str()).unwrap_or("string");
    let bad = |want: &str| Error::invalid_params(format!("--{flag} expects {want}, got '{raw}'"));
    let value = match kind {
        "int" => Value::from(raw.parse::<i64>().map_err(|_| bad("a whole number"))?),
        "bool" => Value::Bool(match raw {
            "true" | "yes" | "y" | "1" => true,
            "false" | "no" | "n" | "0" => false,
            _ => return Err(bad("true or false")),
        }),
        // `--tech-stack "go, rust"` (ADR 0024 §2).
        "list" => Value::Array(split_list(raw).into_iter().map(Value::String).collect()),
        _ => Value::String(raw.to_owned()),
    };
    Ok((name, value))
}

/// Comma-separated items, trimmed, blanks dropped.
fn split_list(raw: &str) -> Vec<String> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

fn is_list(schema: &Value, name: &str) -> bool {
    field(schema, name).is_some_and(|f| f["type"] == "list")
}

// ---------------------------------------------------------------- running

/// `records export` pages through `records.list` this many at a time (its maximum).
const EXPORT_PAGE: usize = 500;

/// Fetch the collection's schema, send the op, render the reply.
pub async fn run(client: &mut Client, cmd: &RecordsCmd, json: bool, prompt: &mut dyn Prompt) -> Result<String> {
    let schema = match cmd.collection() {
        Some(id) => schema(client, id).await?,
        None => Value::Null,
    };
    match cmd {
        RecordsCmd::Import { collection, file, dry_run, skip_invalid, yes } => {
            let text =
                std::fs::read_to_string(file).map_err(|e| Error::invalid_params(format!("can't read {file}: {e}")))?;
            let rows = import_rows(file, &text, &schema)?;
            return import(client, prompt, collection, rows, *dry_run, *skip_invalid, *yes, json).await;
        }
        // Rendering these can fail (no such template or category), so they don't go through `show`.
        RecordsCmd::Templates { which } => {
            let data = client.call("records.templates", json!({})).await?;
            let text = templates(&data, which.as_deref())?;
            return Ok(if json { render::json(&data) } else { text });
        }
        RecordsCmd::Peek { template, file } => {
            // The whole list first, so a category's name or a typo gets a helpful message.
            let data = client.call("records.templates", json!({})).await?;
            let text = peek(&data, template)?;
            if !*file {
                let one = data["templates"].as_array().and_then(|ts| ts.iter().find(|t| t["id"] == template.as_str()));
                return Ok(if json { render::json(one.unwrap_or(&Value::Null)) } else { text });
            }
            let (op, params) = request(cmd, &schema)?;
            let full = client.call(op, params).await?;
            return Ok(if json { render::json(&full["templates"][0]) } else { show(cmd, &full, &schema) });
        }
        RecordsCmd::Export { format, .. } => {
            let (op, mut params) = request(cmd, &schema)?;
            let mut items = Vec::new();
            loop {
                params["offset"] = json!(items.len());
                let page = client.call(op, params.clone()).await?;
                let got = page["items"].as_array().cloned().unwrap_or_default();
                let total = page["total"].as_u64().unwrap_or(0) as usize;
                let done = got.is_empty();
                items.extend(got);
                if done || items.len() >= total {
                    break;
                }
            }
            return Ok(export(&items, &schema, *format));
        }
        _ => {}
    }
    let (op, mut params) = request(cmd, &schema)?;
    let data = match client.call(op, params.clone()).await {
        // ADR 0021 §5: the daemon says how many records go; ask, then send that count back.
        Err(e) if e.code == ErrorCode::ConfirmationRequired => {
            let detail = e.detail.clone().unwrap_or_default();
            match cmd {
                RecordsCmd::RemoveCollection { id, yes } => {
                    if !confirm_removal(prompt, *yes, id, &detail)? {
                        return Ok(format!("kept {id}"));
                    }
                }
                RecordsCmd::Purge { collection, yes, .. } => {
                    if !confirm_purge(prompt, *yes, collection, &e.message)? {
                        return Ok("nothing purged".into());
                    }
                }
                _ => return Err(e),
            }
            params["confirm"] = detail;
            client.call(op, params).await?
        }
        other => other?,
    };
    Ok(if json { render::json(&data) } else { show(cmd, &data, &schema) })
}

/// The rows of an import file, as `records.import` takes them: `.json` is an array of objects,
/// sent as written; anything else is CSV, typed from the collection like `--FIELD VALUE`.
fn import_rows(file: &str, text: &str, schema: &Value) -> Result<Vec<Value>> {
    let bad = |msg: String| Error::invalid_params(format!("{file}: {msg}"));
    if file.ends_with(".json") {
        let rows: Value = serde_json::from_str(text).map_err(|e| bad(e.to_string()))?;
        return match rows {
            Value::Array(rows) if rows.iter().all(Value::is_object) => Ok(rows),
            _ => Err(bad("expected an array of objects, one per record".into())),
        };
    }
    let mut lines = csv::parse(text).map_err(bad)?.into_iter();
    let header = lines.next().ok_or_else(|| bad("empty: expected a header row of field names".into()))?;
    let names: Vec<String> = header.iter().map(|h| field_name(schema, h.trim())).collect();
    let mut rows = Vec::new();
    for (n, line) in lines.enumerate() {
        if line.len() != names.len() {
            return Err(bad(format!("row {} has {} cells, the header has {}", n + 1, line.len(), names.len())));
        }
        let mut row = Map::new();
        for (name, raw) in names.iter().zip(line) {
            if raw.is_empty() {
                continue;
            }
            let value = match name.as_str() {
                "id" | "status" => json!(raw),
                // List items are split by `;`, so a comma can sit inside one.
                _ if is_list(schema, name) => {
                    json!(raw.split(';').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>())
                }
                _ => typed(schema, name, &raw).map_err(|e| bad(format!("row {}: {}", n + 1, e.message)))?.1,
            };
            row.insert(name.clone(), value);
        }
        rows.push(Value::Object(row));
    }
    Ok(rows)
}

/// Check everything first (a dry run), say what would happen, then ask before writing.
#[allow(clippy::too_many_arguments)]
async fn import(
    client: &mut Client,
    prompt: &mut dyn Prompt,
    collection: &str,
    rows: Vec<Value>,
    dry_run: bool,
    skip_invalid: bool,
    yes: bool,
    json: bool,
) -> Result<String> {
    let params = json!({"collection": collection, "rows": rows, "skip_invalid": skip_invalid});
    let mut check = params.clone();
    check["dry_run"] = json!(true);
    let plan = client.call("records.import", check).await?;
    let summary = import_summary(collection, &plan, false);
    let added = plan["added"].as_array().map_or(0, Vec::len);
    let blocked = !plan["invalid"].as_array().is_none_or(Vec::is_empty) && !skip_invalid;
    if dry_run || added == 0 || blocked {
        return Ok(if json { render::json(&plan) } else { summary });
    }
    if !yes {
        if !prompt.interactive() {
            return Err(Error::invalid_params(format!(
                "refusing to import {added} record(s) into '{collection}' without confirmation; pass --yes"
            )));
        }
        eprintln!("{summary}");
        if !prompt.confirm("Import them?") {
            return Ok("nothing imported".into());
        }
    }
    let done = client.call("records.import", params).await?;
    Ok(if json { render::json(&done) } else { import_summary(collection, &done, true) })
}

fn import_summary(collection: &str, data: &Value, written: bool) -> String {
    let added = data["added"].as_array().map_or(0, Vec::len);
    let mut out = match (written && data["written"] == true, added) {
        (true, n) => format!("✓ imported {n} record(s) into {collection}"),
        (false, 0) => format!("nothing to import into {collection}"),
        (false, n) => format!("would import {n} record(s) into {collection}"),
    };
    for s in data["skipped"].as_array().into_iter().flatten() {
        let _ = write!(
            out,
            "
  skip row {}: {}",
            cell(&s["row"]),
            s["reason"].as_str().unwrap_or_default()
        );
    }
    let invalid = data["invalid"].as_array().cloned().unwrap_or_default();
    for i in &invalid {
        let _ = write!(
            out,
            "
  invalid row {}: {}",
            cell(&i["row"]),
            i["error"].as_str().unwrap_or_default()
        );
    }
    if !invalid.is_empty() && data["written"] != true {
        let _ = write!(
            out,
            "
nothing imported while rows are invalid: fix them, or pass --skip-invalid"
        );
    }
    out
}

/// Records as CSV (id, status, then fields in schema order; lists joined with `;`) or as a JSON
/// array, so `records import` reads them back unchanged.
fn export(items: &[Value], schema: &Value, format: Format) -> String {
    if format == Format::Json {
        let clean: Vec<Value> = items
            .iter()
            .map(|i| {
                let mut i = i.clone();
                if let Some(o) = i.as_object_mut() {
                    o.retain(|_, v| !v.is_null());
                }
                i
            })
            .collect();
        return render::json(&json!(clean));
    }
    let mut header = vec!["id".to_owned()];
    if has_status(schema) {
        header.push("status".to_owned());
    }
    header
        .extend(schema["fields"].as_array().into_iter().flatten().filter_map(|f| f["name"].as_str().map(String::from)));
    let mut out = csv::row(&header);
    for item in items {
        let cells: Vec<String> = header
            .iter()
            .map(|k| match &item[k.as_str()] {
                Value::Null => String::new(),
                Value::String(s) => s.clone(),
                Value::Array(list) => list
                    .iter()
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                    .collect::<Vec<_>>()
                    .join(";"),
                other => other.to_string(),
            })
            .collect();
        out.push_str(&csv::row(&cells));
    }
    out.trim_end_matches('\n').to_owned()
}

/// Whether to go ahead with purging, given the daemon's message saying what goes. There is no
/// undo, so without a person there it needs --yes.
fn confirm_purge(prompt: &mut dyn Prompt, yes: bool, collection: &str, message: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !prompt.interactive() {
        return Err(Error::invalid_params(format!(
            "refusing to purge '{collection}'s trash without confirmation; pass --yes to do it anyway"
        )));
    }
    eprintln!("{}. This can't be undone.", message.split(';').next().unwrap_or(message));
    Ok(prompt.confirm("Purge?"))
}

/// Whether to go ahead with removing a collection, given the daemon's
/// `{"records": N}`. Never waits for an answer nobody can see (CLAUDE.md §15.1).
fn confirm_removal(prompt: &mut dyn Prompt, yes: bool, id: &str, detail: &Value) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    let records = detail["records"].as_u64().unwrap_or(0);
    if !prompt.interactive() {
        return Err(Error::invalid_params(format!(
            "refusing to remove '{id}' and its {records} record(s) without confirmation; pass --yes to do it anyway"
        )));
    }
    eprintln!(
        "This removes '{id}' and its {records} record(s). They go to the trash:\n  shimmer records restore-collection {id} brings them back."
    );
    Ok(prompt.confirm("Remove it?"))
}

async fn schema(client: &mut Client, id: &str) -> Result<Value> {
    let all = client.call("records.collections", json!({})).await?;
    let found = all["collections"].as_array().and_then(|cs| cs.iter().find(|c| c["id"] == id)).cloned();
    Ok(found.unwrap_or(Value::Null))
}

// ---------------------------------------------------------------- output

/// Text for a successful reply. Everything shown comes from `data` or the schema; the client
/// computes nothing (§2).
pub fn show(cmd: &RecordsCmd, data: &Value, schema: &Value) -> String {
    match cmd {
        RecordsCmd::Help => USAGE.into(),
        RecordsCmd::Collections => collections(data),
        // The id comes from the reply: the daemon may have made it (ADR 0018).
        // In full, never cut like a table cell: it's what the person types next.
        RecordsCmd::Add { collection, .. } => format!("added {collection}/{}", data["id"].as_str().unwrap_or_default()),
        RecordsCmd::Rename { collection, id, new_id } => {
            format!("✓ {collection}/{id} renamed to {collection}/{new_id}")
        }
        RecordsCmd::Update { collection, id, .. } => format!("updated {collection}/{id}"),
        RecordsCmd::Remove { collection, id, .. } => {
            format!("removed {collection}/{id} (undo: shimmer records restore {collection}/{id})")
        }
        RecordsCmd::Restore { collection, id } => format!("✓ {collection}/{id} restored"),
        RecordsCmd::Check { collection } => check_report(collection, data),
        RecordsCmd::Templates { which } => templates(data, which.as_deref()).unwrap_or_else(|e| e.message),
        RecordsCmd::Peek { template, file: false } => peek(data, template).unwrap_or_else(|e| e.message),
        // The file exactly, as `records new` would copy it.
        RecordsCmd::Peek { file: true, .. } => {
            data["templates"][0]["file"].as_str().unwrap_or_default().trim_end().to_owned()
        }
        RecordsCmd::New { id, template, .. } => created(id, template, data),
        RecordsCmd::RenameField { collection, from, to } => format!(
            "✓ {collection}: field '{}' is now '{}' ({} record(s) updated)",
            field_name(schema, from),
            to.replace('-', "_"),
            cell(&data["updated"])
        ),
        RecordsCmd::RenameCollection { id, new_id } => format!("✓ {id} is now {new_id}"),
        RecordsCmd::RemoveCollection { id, .. } => format!(
            "removed {id} ({} record(s); undo: shimmer records restore-collection {id})",
            cell(&data["records"])
        ),
        RecordsCmd::RestoreCollection { id } => format!("✓ {id} restored"),
        RecordsCmd::Trash { collection } => match data["items"].as_array().is_none_or(Vec::is_empty) {
            true => format!("nothing in {collection}'s trash"),
            false => list(data, schema),
        },
        RecordsCmd::Complete { collection, id, .. } => {
            let mut out = format!("✓ {collection}/{id} done");
            if let Some(stamp) = schema["stamp_on_complete"].as_str() {
                let _ = write!(out, " ({stamp} {})", cell(&data[stamp]));
            }
            out
        }
        RecordsCmd::Reopen { collection, id, clear_stamp } => {
            let mut out = format!("✓ {collection}/{id} reopened (todo)");
            if let (true, Some(stamp)) = (*clear_stamp, schema["stamp_on_complete"].as_str()) {
                let _ = write!(out, "; cleared {stamp}");
            }
            out
        }
        RecordsCmd::Get { collection, id } => item(collection, id, data, schema),
        RecordsCmd::List { .. } => list(data, schema),
        RecordsCmd::Import { collection, .. } => import_summary(collection, data, true),
        RecordsCmd::Purge { collection, .. } => match data["purged"].as_u64() {
            Some(0) => format!("nothing in {collection}'s trash"),
            n => format!("✓ purged {} record(s) from {collection}'s trash", n.unwrap_or(0)),
        },
        RecordsCmd::Export { .. } => render::json(data),
    }
}

/// What completing a record does, in words, from the collection's settings (ADR 0021).
fn completing(t: &Value) -> String {
    if t["completable"] == false {
        return "Nothing to complete: it's a reference list, so records have no todo/done.".into();
    }
    let mut out = match t["stamp_on_complete"].as_str() {
        Some(stamp) => format!("Marks it done and stamps {stamp} with today's date."),
        None => "Marks it done.".to_owned(),
    };
    out.push(' ');
    out.push_str(match t["repeat_complete"].as_str() {
        Some("refuse") => "It happens once: completing it again is refused until you reopen it.",
        _ => "Completing it again counts again (and re-stamps it).",
    });
    out
}

/// `records.templates`, grouped by category; with `which`, one category or one template in full.
fn templates(data: &Value, which: Option<&str>) -> Result<String> {
    render::templates_by_category(
        data,
        which,
        "records",
        ("make a collection from it:", "shimmer records new ID --from TEMPLATE"),
        template_in_full,
    )
}

/// `peek TEMPLATE`: everything about one template before making one from it.
fn peek(data: &Value, id: &str) -> Result<String> {
    Ok(template_in_full(render::template_named(data, id, "records")?, render::categories(data)))
}

/// One template: what it's for, what completing does, its fields, what to know, examples, and
/// what goes with it. Everything comes from `records.templates` (§2).
fn template_in_full(t: &Value, categories: &[Value]) -> String {
    let id = t["id"].as_str().unwrap_or_default();
    let list = |key: &str| t[key].as_array().cloned().unwrap_or_default();
    let mut out = format!("{id}: {}", cell(&t["label"]));
    if let Some(category) = categories.iter().find(|c| c["id"] == t["category"]) {
        let _ = write!(out, "  ({})", cell(&category["label"]));
    }
    if let Some(description) = t["description"].as_str() {
        let _ = write!(out, "\n  {}", wrap(description, 88, "  "));
    }
    out.push('\n');

    let fields = list("fields");
    let _ = writeln!(out, "\nFields ({}; * = required)", fields.len());
    let width =
        fields.iter().map(|f| cell(&f["name"]).chars().count() + usize::from(f["required"] == true)).max().unwrap_or(0);
    for f in &fields {
        let mut name = cell(&f["name"]);
        if f["required"] == true {
            name.push('*');
        }
        let mut notes = field_notes(f);
        if notes.is_empty() {
            notes.push("text".into());
        }
        let _ = writeln!(out, "{}", format!("  {name:<width$}  {}", notes.join(", ")).trim_end());
    }
    if let Some(title) = t["title"].as_str() {
        let _ = writeln!(out, "  each record is shown as: {title}");
    }

    let _ = writeln!(out, "\nWhen you complete a record\n  {}", wrap(&completing(t), 88, "  "));

    let notes = list("notes");
    if !notes.is_empty() {
        out.push_str("\nGood to know\n");
        for note in &notes {
            let _ = writeln!(out, "  • {}", wrap(note.as_str().unwrap_or_default(), 86, "    "));
        }
    }
    let examples = list("examples");
    if !examples.is_empty() {
        out.push_str("\nHow it works (<collection> is the name you give it)\n");
        for line in &examples {
            let _ = writeln!(out, "{}", format!("  {}", line.as_str().unwrap_or_default()).trim_end());
        }
    }
    let related: Vec<String> = list("related").iter().map(cell).collect();
    if !related.is_empty() {
        let _ = writeln!(out, "\nGoes with: {} (shimmer records peek NAME)", related.join(", "));
    }
    let _ = write!(
        out,
        "\nmake one:      shimmer records new ID --from {id}\nsee the file:  shimmer records peek {id} --file"
    );
    out
}

/// What was made, how to add the first record (its required fields), and what goes with it.
/// Everything comes from the new collection the daemon returned (§2).
fn created(id: &str, template: &str, collection: &Value) -> String {
    let mut out = format!("✓ created collection {id} from {template}");
    let required: Vec<String> = collection["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["required"] == true)
        .map(|f| format!("--{} …", f["name"].as_str().unwrap_or_default().replace('_', "-")))
        .collect();
    let _ = write!(out, "\n  add one: shimmer records add {id}");
    if !required.is_empty() {
        let _ = write!(out, " {}", required.join(" "));
    }
    for related in collection["related"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let _ = write!(out, "\n  goes with: {related} (shimmer records new {related} --from {related})");
    }
    out
}

/// `checked 42 records; 2 with problems`, then each record's problems, one per line.
fn check_report(collection: &str, data: &Value) -> String {
    let checked = data["checked"].as_u64().unwrap_or(0);
    let problems = data["problems"].as_array().cloned().unwrap_or_default();
    if problems.is_empty() {
        return format!("checked {checked} record(s) in {collection}; all fit");
    }
    let mut out = format!("checked {checked} record(s) in {collection}; {} with problems", problems.len());
    let width = problems.iter().map(|p| cell(&p["id"]).chars().count()).max().unwrap_or(0);
    for p in &problems {
        let id = cell(&p["id"]);
        for (i, problem) in p["problems"].as_array().into_iter().flatten().enumerate() {
            let shown = if i == 0 { id.as_str() } else { "" };
            let _ = write!(out, "\n  {shown:width$}  {}", problem.as_str().unwrap_or_default());
        }
    }
    out
}

fn collections(data: &Value) -> String {
    let rows: Vec<Vec<String>> = data["collections"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .map(|c| {
                    let fields = c["fields"]
                        .as_array()
                        .map(|fs| fs.iter().map(describe_field).collect::<Vec<_>>().join(", "))
                        .unwrap_or_default();
                    vec![cell(&c["id"]), cell(&c["label"]), fields]
                })
                .collect()
        })
        .unwrap_or_default();
    let mut out = match rows.is_empty() {
        // A fresh install starts here (ADR 0023): say how to get one.
        true => "no collections yet: see 'shimmer records templates', then 'shimmer records new ID --from TEMPLATE'"
            .to_owned(),
        false => table(&["ID".into(), "LABEL".into(), "FIELDS".into()], &rows),
    };
    // In full, not cut like a table cell: the message is how the user finds the broken line.
    for skipped in data["skipped"].as_array().into_iter().flatten() {
        let _ = write!(out, "\nskipped: {}", skipped.as_str().unwrap_or_default());
    }
    out
}

/// `difficulty (easy|medium|hard)`, `title*` for required, `last_solved (date)`, and a field's
/// role and uniqueness (ADR 0017): `url (url, unique)`, `oa_deadline (date, deadline)`.
fn describe_field(f: &Value) -> String {
    let mut s = cell(&f["name"]);
    if f["required"] == true {
        s.push('*');
    }
    let notes = field_notes(f);
    if !notes.is_empty() {
        let _ = write!(s, " ({})", notes.join(", "));
    }
    s
}

/// A field's type (enum values, ref target), role and uniqueness, as `describe_field` and
/// `peek` show them.
fn field_notes(f: &Value) -> Vec<String> {
    let mut notes = Vec::new();
    match f["type"].as_str() {
        Some("enum") => {
            notes.push(f["values"].as_array().into_iter().flatten().map(cell).collect::<Vec<_>>().join("|"))
        }
        Some("ref") => notes.push(format!("ref to {}", cell(&f["collection"]))),
        Some("list") if f["of"] == "ref" => notes.push(format!("list of refs to {}", cell(&f["collection"]))),
        Some("list") => {
            let of = f["of"].as_str().unwrap_or("string");
            match f["values"].as_array() {
                Some(values) => {
                    notes.push(format!("list of {}", values.iter().map(cell).collect::<Vec<_>>().join("|")))
                }
                None => notes.push(format!("list of {of}")),
            }
        }
        Some("string") | None => {}
        Some(other) => notes.push(other.to_owned()),
    }
    if let Some(role) = f["role"].as_str() {
        notes.push(role.to_owned());
    }
    if f["unique"] == true {
        notes.push("unique".to_owned());
    }
    notes
}

/// Reference collections (`completable = false`, ADR 0021) have no status to show.
fn has_status(schema: &Value) -> bool {
    schema["completable"] != false
}

fn item(collection: &str, id: &str, data: &Value, schema: &Value) -> String {
    let mut out = match has_status(schema) {
        true => format!("{collection}/{id}  ({})", cell(&data["status"])),
        false => format!("{collection}/{id}"),
    };
    let keys = columns(data, schema);
    let width = keys.iter().map(|k| k.chars().count()).max().unwrap_or(0);
    for key in keys {
        let _ = write!(out, "\n  {key:width$}  {}", shown(schema, &key, &data[key.as_str()]));
    }
    out
}

fn list(data: &Value, schema: &Value) -> String {
    let items = data["items"].as_array().cloned().unwrap_or_default();
    let total = data["total"].as_u64().unwrap_or(items.len() as u64);
    let mut out = if items.is_empty() {
        "no records".to_owned()
    } else {
        // Only columns some listed record has filled in: a 20-field collection with three fields
        // set shouldn't print 17 columns of dashes. `records get` still shows every field.
        let keys: Vec<String> = columns(&items[0], schema)
            .into_iter()
            .filter(|k| items.iter().any(|i| !i[k.as_str()].is_null() && i[k.as_str()] != ""))
            .collect();
        let status = has_status(schema);
        // The daemon's titles (ADR 0024 §5), when the collection has a title other than a field.
        let titles = &data["titles"];
        let titled = schema["title"].is_string() && !keys_are_title(schema) && titles.is_object();
        let mut header = vec!["ID".to_owned()];
        if titled {
            header.push("TITLE".to_owned());
        }
        if status {
            header.push("STATUS".to_owned());
        }
        header.extend(keys.iter().map(|k| k.to_uppercase().replace('_', " ")));
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|i| {
                // The id in full: a cut id can't be typed back.
                let id = i["id"].as_str().unwrap_or_default();
                let mut row = vec![id.to_owned()];
                if titled {
                    row.push(cell(&titles[id]));
                }
                if status {
                    row.push(cell(&i["status"]));
                }
                row.extend(keys.iter().map(|k| shown(schema, k, &i[k.as_str()])));
                row
            })
            .collect();
        let mut t = table(&header, &rows);
        let count = match (items.len() as u64, total) {
            (1, 1) => "1 record".to_owned(),
            (n, t) if n == t => format!("{n} records"),
            (n, t) => format!("{n} of {t}"),
        };
        let _ = write!(t, "\n{count}");
        t
    };
    for skipped in data["skipped"].as_array().into_iter().flatten() {
        let _ = write!(out, "\nskipped: {}", cell(skipped));
    }
    out
}

/// A title that is just one field (`"{title}"`, `"{question}"`) already shows as its column.
fn keys_are_title(schema: &Value) -> bool {
    let title = schema["title"].as_str().unwrap_or_default();
    title.starts_with('{') && title.ends_with('}') && title[1..title.len() - 1].chars().all(|c| c != '{' && c != '[')
}

/// A value as a person reads it: a `datetime` in this machine's local time (ADR 0024 §1), the rest
/// as `cell` shows them. `--json` keeps the stored value.
fn shown(schema: &Value, key: &str, v: &Value) -> String {
    let datetime = field(schema, key).is_some_and(|f| f["type"] == "datetime");
    match v.as_str().map(chrono::DateTime::parse_from_rfc3339) {
        Some(Ok(at)) if datetime => at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string(),
        // A list as its items (ADR 0024 §2); `[]` is unset.
        _ => match v.as_array() {
            Some(items) if items.is_empty() => cell(&Value::Null),
            Some(items) => cell(&Value::String(items.iter().map(cell).collect::<Vec<_>>().join(", "))),
            None => cell(v),
        },
    }
}

/// Field columns in schema order, then any extra keys the item carries.
fn columns(item: &Value, schema: &Value) -> Vec<String> {
    let mut keys: Vec<String> = schema["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["name"].as_str().map(String::from))
        .collect();
    if let Some(obj) = item.as_object() {
        for k in obj.keys() {
            if k != "id" && k != "status" && !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str]) -> std::result::Result<RecordsCmd, String> {
        parse(words.iter().map(|s| s.to_string()).collect())
    }

    fn pairs(p: &[(&str, &str)]) -> Flags {
        p.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    fn leetcode() -> Value {
        json!({
            "id": "leetcode", "label": "LeetCode", "stamp_on_complete": "last_solved",
            "fields": [
                {"name": "title", "type": "string", "required": true},
                {"name": "difficulty", "type": "enum", "values": ["easy", "medium", "hard"], "required": false},
                {"name": "url", "type": "string", "required": false},
                {"name": "last_solved", "type": "date", "required": false},
                {"name": "attempts", "type": "int", "required": false},
                {"name": "starred", "type": "bool", "required": false}
            ]
        })
    }

    #[test]
    fn targets_both_spellings() {
        let want = RecordsCmd::Complete {
            collection: "leetcode".into(),
            id: "two-sum".into(),
            set: vec![],
            unset: vec![],
            items: vec![],
        };
        assert_eq!(parse_words(&["complete", "leetcode/two-sum"]).unwrap(), want);
        assert_eq!(parse_words(&["complete", "leetcode", "two-sum"]).unwrap(), want);
        for bad in [&["complete"][..], &["complete", "leetcode"], &["complete", "/x"], &["complete", "a", "b", "c"]] {
            assert!(parse_words(bad).unwrap_err().contains("COLLECTION/ID"), "{bad:?}");
        }
    }

    #[test]
    fn add_and_update_take_field_flags() {
        assert_eq!(
            parse_words(&["add", "leetcode", "two-sum", "--title", "Two Sum", "--difficulty=easy"]).unwrap(),
            RecordsCmd::Add {
                collection: "leetcode".into(),
                id: Some("two-sum".into()),
                set: pairs(&[("title", "Two Sum"), ("difficulty", "easy")])
            }
        );
        assert_eq!(
            parse_words(&["update", "leetcode/two-sum", "--url", "https://x", "--unset", "difficulty"]).unwrap(),
            RecordsCmd::Update {
                collection: "leetcode".into(),
                id: "two-sum".into(),
                set: pairs(&[("url", "https://x")]),
                unset: vec!["difficulty".into()],
                items: vec![]
            }
        );
        assert!(parse_words(&["update", "leetcode/two-sum"]).unwrap_err().contains("nothing to update"));
        assert!(parse_words(&["add", "leetcode/x", "--title"]).unwrap_err().contains("needs a value"));
        // A value may look like a flag: the word after --attempts is its value.
        assert_eq!(
            parse_words(&["add", "leetcode/x", "--attempts", "-1"]).unwrap(),
            RecordsCmd::Add { collection: "leetcode".into(), id: Some("x".into()), set: pairs(&[("attempts", "-1")]) }
        );
    }

    #[test]
    fn list_filters_and_pages() {
        assert_eq!(
            parse_words(&["list", "leetcode", "--status", "todo", "--difficulty", "hard", "--limit", "5"]).unwrap(),
            RecordsCmd::List {
                collection: "leetcode".into(),
                filter: pairs(&[("status", "todo"), ("difficulty", "hard")]),
                limit: Some(5),
                offset: None,
                refine: Refine::default()
            }
        );
        assert!(parse_words(&["list"]).is_err());
        assert!(parse_words(&["list", "leetcode", "--limit", "lots"]).unwrap_err().contains("whole number"));
    }

    #[test]
    fn complete_takes_field_flags_and_reopen_takes_clear_stamp() {
        let complete =
            parse_words(&["complete", "leetcode/x", "--last-solved", "2026-10-03", "--unset", "url"]).unwrap();
        assert_eq!(
            complete,
            RecordsCmd::Complete {
                collection: "leetcode".into(),
                id: "x".into(),
                set: pairs(&[("last-solved", "2026-10-03")]),
                unset: vec!["url".into()],
                items: vec![]
            }
        );
        let (op, params) = request(&complete, &leetcode()).unwrap();
        assert_eq!(
            (op, params),
            (
                "records.complete",
                json!({"collection": "leetcode", "id": "x", "fields": {"last_solved": "2026-10-03", "url": null}})
            )
        );
        // No flags: the same request as before ADR 0016.
        let (_, params) = request(&parse_words(&["complete", "leetcode/x"]).unwrap(), &leetcode()).unwrap();
        assert_eq!(params, json!({"collection": "leetcode", "id": "x"}));

        for words in [&["reopen", "leetcode/x", "--clear-stamp"][..], &["reopen", "--clear-stamp", "leetcode", "x"]] {
            let reopen = parse_words(words).unwrap();
            assert_eq!(reopen, RecordsCmd::Reopen { collection: "leetcode".into(), id: "x".into(), clear_stamp: true });
            let (op, params) = request(&reopen, &leetcode()).unwrap();
            assert_eq!(
                (op, params),
                ("records.reopen", json!({"collection": "leetcode", "id": "x", "clear_stamp": true}))
            );
        }
        let (_, params) = request(&parse_words(&["reopen", "leetcode/x"]).unwrap(), &leetcode()).unwrap();
        assert_eq!(params, json!({"collection": "leetcode", "id": "x"}));
        assert!(parse_words(&["reopen", "leetcode/x", "--title", "t"]).unwrap_err().contains("takes no options"));
        // `--clear-stamp` means nothing to other commands.
        assert!(parse_words(&["get", "leetcode/x", "--clear-stamp"]).unwrap_err().contains("needs a value"));
    }

    #[test]
    fn reopen_output_says_what_it_cleared() {
        let item = json!({"id": "x", "status": "todo"});
        let keep = RecordsCmd::Reopen { collection: "leetcode".into(), id: "x".into(), clear_stamp: false };
        assert_eq!(show(&keep, &item, &leetcode()), "✓ leetcode/x reopened (todo)");
        let clear = RecordsCmd::Reopen { collection: "leetcode".into(), id: "x".into(), clear_stamp: true };
        assert_eq!(show(&clear, &item, &leetcode()), "✓ leetcode/x reopened (todo); cleared last_solved");
    }

    #[test]
    fn add_may_leave_the_id_to_the_daemon() {
        let add = parse_words(&["add", "leetcode", "--title", "Two Sum"]).unwrap();
        assert_eq!(
            add,
            RecordsCmd::Add { collection: "leetcode".into(), id: None, set: pairs(&[("title", "Two Sum")]) }
        );
        let (op, params) = request(&add, &leetcode()).unwrap();
        assert_eq!((op, params), ("records.add", json!({"collection": "leetcode", "fields": {"title": "Two Sum"}})));
        // The reply names the id it chose.
        assert_eq!(show(&add, &json!({"id": "two-sum"}), &leetcode()), "added leetcode/two-sum");
    }

    #[test]
    fn rename_takes_a_target_and_a_new_id() {
        let want = RecordsCmd::Rename { collection: "jobs".into(), id: "amzon".into(), new_id: "amazon".into() };
        assert_eq!(parse_words(&["rename", "jobs/amzon", "amazon"]).unwrap(), want);
        assert_eq!(parse_words(&["rename", "jobs", "amzon", "amazon"]).unwrap(), want);
        assert_eq!(
            request(&want, &Value::Null).unwrap(),
            ("records.rename", json!({"collection": "jobs", "id": "amzon", "new_id": "amazon"}))
        );
        assert_eq!(show(&want, &json!({}), &Value::Null), "✓ jobs/amzon renamed to jobs/amazon");
        for bad in [&["rename"][..], &["rename", "jobs/amzon"], &["rename", "jobs", "a", "b", "c"]] {
            assert!(parse_words(bad).unwrap_err().contains("NEW_ID"), "{bad:?}");
        }
    }

    #[test]
    fn list_builds_operator_filters_search_and_sort() {
        let cmd = parse_words(&[
            "list",
            "leetcode",
            "--filter",
            "last-solved>=2026-10-01",
            "--filter",
            "last_solved <= 2026-10-10",
            "--filter",
            "difficulty=easy|medium",
            "--filter",
            "title~two",
            "--filter",
            "attempts!=3",
            "--has",
            "url",
            "--missing",
            "starred",
            "--search",
            "sum",
            "--sort",
            "-last-solved",
            "--sort",
            "title",
        ])
        .unwrap();
        let (op, params) = request(&cmd, &leetcode()).unwrap();
        assert_eq!(op, "records.list");
        assert_eq!(
            params,
            json!({"collection": "leetcode",
                "filter": {
                    "last_solved": {"gte": "2026-10-01", "lte": "2026-10-10"},
                    "difficulty": {"in": ["easy", "medium"]},
                    "title": {"contains": "two"},
                    "attempts": {"ne": 3},
                    "url": {"set": true},
                    "starred": {"set": false}},
                "search": "sum",
                "sort": ["-last_solved", "title"]})
        );

        // An exact --FIELD and an operator on the same field combine.
        let both = parse_words(&["list", "leetcode", "--difficulty", "easy", "--has", "difficulty"]).unwrap();
        assert_eq!(
            request(&both, &leetcode()).unwrap().1["filter"],
            json!({"difficulty": {"eq": "easy", "set": true}})
        );

        for bad in ["title", "=x", "title=", "title!"] {
            let cmd = parse_words(&["list", "leetcode", "--filter", bad]).unwrap();
            let e = request(&cmd, &leetcode()).unwrap_err();
            assert!(e.message.contains("FIELD OP VALUE") || e.message.contains("expects"), "{bad}: {}", e.message);
        }
    }

    #[test]
    fn remove_says_how_to_undo_and_restore_and_trash_parse() {
        let remove = parse_words(&["remove", "jobs/acme"]).unwrap();
        assert_eq!(
            show(&remove, &json!({"removed": true}), &Value::Null),
            "removed jobs/acme (undo: shimmer records restore jobs/acme)"
        );

        let restore = parse_words(&["restore", "jobs", "acme"]).unwrap();
        assert_eq!(
            request(&restore, &Value::Null).unwrap(),
            ("records.restore", json!({"collection": "jobs", "id": "acme"}))
        );
        assert_eq!(show(&restore, &json!({}), &Value::Null), "✓ jobs/acme restored");

        let trash = parse_words(&["trash", "jobs"]).unwrap();
        assert_eq!(request(&trash, &Value::Null).unwrap(), ("records.trash", json!({"collection": "jobs"})));
        assert_eq!(show(&trash, &json!({"items": []}), &Value::Null), "nothing in jobs's trash");
        assert!(parse_words(&["trash"]).unwrap_err().contains("records trash COLLECTION"));
    }

    #[test]
    fn collection_commands_parse_and_report() {
        assert_eq!(parse_words(&["check", "jobs"]).unwrap(), RecordsCmd::Check { collection: "jobs".into() });
        let rename = parse_words(&["rename-field", "leetcode", "last-solved", "solved-on"]).unwrap();
        assert_eq!(
            request(&rename, &leetcode()).unwrap(),
            ("records.rename_field", json!({"collection": "leetcode", "from": "last_solved", "to": "solved_on"}))
        );
        assert_eq!(
            parse_words(&["remove-collection", "jobs", "--yes"]).unwrap(),
            RecordsCmd::RemoveCollection { id: "jobs".into(), yes: true }
        );
        assert_eq!(
            request(&parse_words(&["rename-collection", "jobs", "apps"]).unwrap(), &Value::Null).unwrap(),
            ("records.rename_collection", json!({"id": "jobs", "new_id": "apps"}))
        );
        assert!(parse_words(&["rename-field", "jobs", "a"]).unwrap_err().contains("FROM TO"));

        let report = json!({"checked": 3, "problems": [
            {"id": "acme", "problems": ["field 'stage' must be one of oa", "unknown key 'x' (not a field of 'jobs')"]}]});
        let out = show(&RecordsCmd::Check { collection: "jobs".into() }, &report, &Value::Null);
        assert_eq!(
            out,
            "checked 3 record(s) in jobs; 1 with problems\n  acme  field 'stage' must be one of oa\n        unknown key 'x' (not a field of 'jobs')"
        );
        let clean = show(
            &RecordsCmd::Check { collection: "jobs".into() },
            &json!({"checked": 2, "problems": []}),
            &Value::Null,
        );
        assert_eq!(clean, "checked 2 record(s) in jobs; all fit");
    }

    struct Scripted {
        interactive: bool,
        answer: bool,
    }

    impl Prompt for Scripted {
        fn interactive(&self) -> bool {
            self.interactive
        }
        fn confirm(&mut self, _: &str) -> bool {
            self.answer
        }
    }

    #[test]
    fn removing_a_collection_asks_at_a_terminal_and_refuses_without_one() {
        let detail = json!({"records": 42});
        let mut yes = Scripted { interactive: true, answer: true };
        let mut no = Scripted { interactive: true, answer: false };
        let mut script = Scripted { interactive: false, answer: true };
        assert!(confirm_removal(&mut yes, false, "jobs", &detail).unwrap());
        assert!(!confirm_removal(&mut no, false, "jobs", &detail).unwrap());
        let e = confirm_removal(&mut script, false, "jobs", &detail).unwrap_err();
        assert!(e.message.contains("its 42 record(s)") && e.message.contains("--yes"), "{}", e.message);
        assert!(confirm_removal(&mut script, true, "jobs", &detail).unwrap(), "--yes needs no terminal");
    }

    #[test]
    fn reference_collections_show_no_status() {
        let education =
            json!({"id": "education", "completable": false, "fields": [{"name": "school", "type": "string"}]});
        let data = json!({"total": 1, "items": [{"id": "uni", "school": "Uni"}]});
        let cmd = parse_words(&["list", "education"]).unwrap();
        assert_eq!(show(&cmd, &data, &education), "ID   SCHOOL\nuni  Uni\n1 record");
        let get = RecordsCmd::Get { collection: "education".into(), id: "uni".into() };
        assert!(show(&get, &data["items"][0], &education).starts_with("education/uni\n"));
    }

    #[test]
    fn templates_and_new() {
        assert_eq!(parse_words(&["templates"]).unwrap(), RecordsCmd::Templates { which: None });
        let new = parse_words(&["new", "jobs", "--from", "job-applications", "--label", "Internships"]).unwrap();
        assert_eq!(
            request(&new, &Value::Null).unwrap(),
            (
                "records.create_collection",
                json!({"id": "jobs", "template": "job-applications", "label": "Internships"})
            )
        );
        assert!(parse_words(&["new", "jobs"]).unwrap_err().contains("--from TEMPLATE"));
        assert!(parse_words(&["new"]).unwrap_err().contains("usage"));
        assert!(parse_words(&["new", "jobs", "--from", "x", "--colour", "red"])
            .unwrap_err()
            .contains("--from and --label"));

        let collection = json!({"id": "jobs", "related": ["interviews"], "fields": [
            {"name": "position", "type": "string", "required": true},
            {"name": "company", "type": "string", "required": true},
            {"name": "url", "type": "string", "required": false}]});
        assert_eq!(
            show(&new, &collection, &Value::Null),
            "✓ created collection jobs from job-applications\n  add one: shimmer records add jobs --position … --company …\n  goes with: interviews (shimmer records new interviews --from interviews)"
        );
    }

    /// What `records.templates` sends (ADR 0030 §1), cut down.
    fn listing() -> Value {
        json!({
            "categories": [{"id": "job-hunt", "label": "Job hunt"}, {"id": "practice", "label": "Practice"}],
            "templates": [
                {"id": "offers", "label": "Offers", "category": "job-hunt", "description": "Compare offers",
                 "fields": [], "completable": true, "repeat_complete": "refuse", "related": [], "notes": [], "examples": []},
                {"id": "leetcode", "label": "LeetCode", "category": "practice",
                 "description": "Problems by pattern and difficulty, with how sure you are and when to redo them, and more",
                 "title": "{title}", "stamp_on_complete": "last_solved", "repeat_complete": "restamp",
                 "completable": true, "related": ["interview-questions"],
                 "fields": [
                    {"name": "title", "type": "string", "required": true},
                    {"name": "difficulty", "type": "enum", "values": ["easy", "medium", "hard"]},
                    {"name": "url", "type": "string", "role": "url", "unique": true},
                    {"name": "last_solved", "type": "date"}],
                 "notes": ["Keep it private."],
                 "examples": ["shimmer records add <collection> --title \"Two Sum\"", "    --difficulty easy"]}
            ]
        })
    }

    #[test]
    fn templates_and_peek_parse() {
        assert_eq!(
            parse_words(&["templates", "practice"]).unwrap(),
            RecordsCmd::Templates { which: Some("practice".into()) }
        );
        assert!(parse_words(&["templates", "a", "b"]).unwrap_err().contains("usage"));
        assert!(parse_words(&["templates", "--all", "x"]).unwrap_err().contains("takes no options"));
        assert_eq!(
            parse_words(&["peek", "leetcode"]).unwrap(),
            RecordsCmd::Peek { template: "leetcode".into(), file: false }
        );
        let file = parse_words(&["peek", "--file", "leetcode"]).unwrap();
        assert_eq!(file, RecordsCmd::Peek { template: "leetcode".into(), file: true });
        assert_eq!(
            request(&file, &Value::Null).unwrap(),
            ("records.templates", json!({"id": "leetcode", "file": true}))
        );
        assert!(parse_words(&["peek"]).unwrap_err().contains("usage: shimmer records peek TEMPLATE"));
        assert!(parse_words(&["peek", "a", "b"]).unwrap_err().contains("usage"));
        assert!(parse_words(&["peek", "leetcode", "--scripts", "x"]).unwrap_err().contains("takes --file"));
    }

    #[test]
    fn templates_are_grouped_by_category() {
        let all = templates(&listing(), None).unwrap();
        assert_eq!(
            all,
            "Job hunt (job-hunt)\n  offers    Compare offers\n\n\
             Practice (practice)\n  leetcode  Problems by pattern and difficulty, with how sure you are and when to…\n\n\
             everything about one:      shimmer records peek TEMPLATE\n\
             make a collection from it: shimmer records new ID --from TEMPLATE"
        );
        let one_category = templates(&listing(), Some("practice")).unwrap();
        assert!(one_category.starts_with("Practice (practice)\n  leetcode") && !one_category.contains("offers"));
        // A template's name shows it in full; anything else names the categories.
        assert_eq!(templates(&listing(), Some("leetcode")).unwrap(), peek(&listing(), "leetcode").unwrap());
        let e = templates(&listing(), Some("games")).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        assert!(e.message.contains("no template or category 'games' (categories: job-hunt, practice"), "{}", e.message);
        assert_eq!(templates(&json!({"templates": []}), None).unwrap(), "no templates");
    }

    #[test]
    fn peek_shows_everything_about_one_template() {
        assert_eq!(
            peek(&listing(), "leetcode").unwrap(),
            "leetcode: LeetCode  (Practice)\n  Problems by pattern and difficulty, with how sure you are and when to redo them, and\n  more\n\
             \nFields (4; * = required)\n  title*       text\n  difficulty   easy|medium|hard\n  url          url, unique\n  last_solved  date\n  each record is shown as: {title}\n\
             \nWhen you complete a record\n  Marks it done and stamps last_solved with today's date. Completing it again counts again\n  (and re-stamps it).\n\
             \nGood to know\n  • Keep it private.\n\
             \nHow it works (<collection> is the name you give it)\n  shimmer records add <collection> --title \"Two Sum\"\n      --difficulty easy\n\
             \nGoes with: interview-questions (shimmer records peek NAME)\n\
             \nmake one:      shimmer records new ID --from leetcode\nsee the file:  shimmer records peek leetcode --file"
        );
        let offers = peek(&listing(), "offers").unwrap();
        assert!(offers.contains("Marks it done. It happens once: completing it again is refused"), "{offers}");
        assert!(offers.contains("Fields (0; * = required)") && !offers.contains("Good to know"), "{offers}");
        let mut reference = listing();
        reference["templates"][0]["completable"] = json!(false);
        assert!(peek(&reference, "offers").unwrap().contains("it's a reference list"));

        let e = peek(&listing(), "practice").unwrap_err();
        assert!(
            e.message.contains(
                "'practice' is a category, not a template: see its templates with shimmer records templates practice"
            ),
            "{}",
            e.message
        );
        let e = peek(&listing(), "chess").unwrap_err();
        assert_eq!(e.message, "no template 'chess' (there are: offers, leetcode)");
        // --file prints the file exactly.
        let file = json!({"templates": [{"id": "leetcode", "file": "# LeetCode\n[collection]\nid = \"leetcode\"\n"}]});
        assert_eq!(
            show(&RecordsCmd::Peek { template: "leetcode".into(), file: true }, &file, &Value::Null),
            "# LeetCode\n[collection]\nid = \"leetcode\""
        );
    }

    #[test]
    fn misuse() {
        assert_eq!(parse_words(&[]).unwrap(), RecordsCmd::Help);
        assert!(parse_words(&["frob"]).unwrap_err().contains("unknown records command"));
        assert!(parse_words(&["get", "leetcode/x", "--title", "t"]).unwrap_err().contains("takes no options"));
        assert!(parse_words(&["collections", "extra"]).is_err());
        assert!(parse_words(&["add", "leetcode/x", "-t", "T"]).unwrap_err().contains("unknown option"));
    }

    #[test]
    fn values_get_their_field_types() {
        let add = parse_words(&[
            "add",
            "leetcode/x",
            "--title",
            "7",
            "--attempts",
            "3",
            "--starred",
            "yes",
            "--last-solved",
            "2026-09-25",
        ])
        .unwrap();
        let (op, params) = request(&add, &leetcode()).unwrap();
        assert_eq!(op, "records.add");
        assert_eq!(
            params["fields"],
            json!({"title": "7", "attempts": 3, "starred": true, "last_solved": "2026-09-25"}),
            "strings stay strings, ints and bools convert, --last-solved finds last_solved"
        );

        let bad = parse_words(&["add", "leetcode/x", "--attempts", "three"]).unwrap();
        let e = request(&bad, &leetcode()).unwrap_err();
        assert_eq!(e.message, "--attempts expects a whole number, got 'three'");

        // Without a schema everything goes as a string and the daemon judges it.
        let (_, params) = request(&bad, &Value::Null).unwrap();
        assert_eq!(params["fields"], json!({"attempts": "three"}));
    }

    #[test]
    fn list_and_update_requests() {
        let list = parse_words(&["list", "leetcode", "--status", "done", "--limit", "2", "--offset", "4"]).unwrap();
        let (op, params) = request(&list, &leetcode()).unwrap();
        assert_eq!(
            (op, params),
            ("records.list", json!({"collection": "leetcode", "filter": {"status": "done"}, "limit": 2, "offset": 4}))
        );

        let update = parse_words(&["update", "leetcode/x", "--unset", "last-solved"]).unwrap();
        let (_, params) = request(&update, &leetcode()).unwrap();
        assert_eq!(params["fields"], json!({"last_solved": null}));
    }

    #[test]
    fn list_renders_a_table_in_schema_order() {
        let data = json!({"total": 3, "items": [
            {"id": "two-sum", "status": "done", "title": "Two Sum", "difficulty": "easy", "url": null,
             "last_solved": "2026-09-18", "attempts": null, "starred": null},
            {"id": "lru-cache", "status": "todo", "title": "LRU Cache", "difficulty": "medium", "url": null,
             "last_solved": null, "attempts": 2, "starred": true}
        ]});
        let cmd = parse_words(&["list", "leetcode", "--limit", "2"]).unwrap();
        let want = "\
ID         STATUS  TITLE      DIFFICULTY  LAST SOLVED  ATTEMPTS  STARRED
two-sum    done    Two Sum    easy        2026-09-18   -         -
lru-cache  todo    LRU Cache  medium      -            2         true
2 of 3";
        // URL is empty in every listed record, so its column is left out.
        assert_eq!(show(&cmd, &data, &leetcode()), want);

        let empty = json!({"items": [], "total": 0, "skipped": ["record file 'x.toml': bad"]});
        assert_eq!(show(&cmd, &empty, &leetcode()), "no records\nskipped: record file 'x.toml': bad");
    }

    #[test]
    fn list_fields_take_commas_add_remove_and_has() {
        let schema = json!({"fields": [{"name": "tech_stack", "type": "list", "of": "string"}]});
        let add = parse_words(&["add", "jobs", "--tech-stack", "go, rust,"]).unwrap();
        assert_eq!(request(&add, &schema).unwrap().1["fields"], json!({"tech_stack": ["go", "rust"]}));

        let update =
            parse_words(&["update", "jobs/x", "--add", "tech-stack", "sql, c", "--remove", "tech_stack", "go"])
                .unwrap();
        assert_eq!(
            request(&update, &schema).unwrap().1["fields"],
            json!({"tech_stack": {"add": ["sql", "c"], "remove": ["go"]}})
        );
        assert!(parse_words(&["update", "jobs/x", "--add", "tech_stack"]).unwrap_err().contains("a field and a value"));
        let both = parse_words(&["update", "jobs/x", "--tech-stack", "go", "--add", "tech_stack", "c"]).unwrap();
        assert!(request(&both, &schema).unwrap_err().message.contains("pick one"));

        for words in
            [&["list", "jobs", "--filter", "tech_stack has Rust"][..], &["list", "jobs", "--filter", "tech-stack=Rust"]]
        {
            let (_, params) = request(&parse_words(words).unwrap(), &schema).unwrap();
            assert_eq!(params["filter"], json!({"tech_stack": {"has": "Rust"}}), "{words:?}");
        }
        let e =
            request(&parse_words(&["list", "jobs", "--filter", "tech_stack=go|rust"]).unwrap(), &schema).unwrap_err();
        assert!(e.message.contains("--or \"tech_stack has go\" --or \"tech_stack has rust\""), "{}", e.message);
        let (_, params) = request(&parse_words(&["list", "jobs", "--tech-stack", "go"]).unwrap(), &schema).unwrap();
        assert_eq!(params["filter"], json!({"tech_stack": "go"}));
        assert_eq!(describe_field(&schema["fields"][0]), "tech_stack (list of string)");
        assert_eq!(shown(&schema, "tech_stack", &json!(["go", "rust"])), "go, rust");
        assert_eq!(shown(&schema, "tech_stack", &json!([])), "-");
    }

    #[test]
    fn refs_describe_their_target_and_remove_takes_force() {
        let one = json!({"name": "application", "type": "ref", "collection": "jobs"});
        let many = json!({"name": "also_for", "type": "list", "of": "ref", "collection": "jobs"});
        assert_eq!(describe_field(&one), "application (ref to jobs)");
        assert_eq!(describe_field(&many), "also_for (list of refs to jobs)");

        let remove = parse_words(&["remove", "jobs/acme", "--force"]).unwrap();
        assert_eq!(remove, RecordsCmd::Remove { collection: "jobs".into(), id: "acme".into(), force: true });
        assert_eq!(
            request(&remove, &Value::Null).unwrap().1,
            json!({"collection": "jobs", "id": "acme", "force": true})
        );
        let plain = parse_words(&["remove", "jobs/acme"]).unwrap();
        assert_eq!(request(&plain, &Value::Null).unwrap().1, json!({"collection": "jobs", "id": "acme"}));
    }

    #[test]
    fn import_and_export_parse_and_round_trip() {
        assert_eq!(
            parse_words(&["import", "jobs", "jobs.csv", "--dry-run", "--yes"]).unwrap(),
            RecordsCmd::Import {
                collection: "jobs".into(),
                file: "jobs.csv".into(),
                dry_run: true,
                skip_invalid: false,
                yes: true
            }
        );
        assert!(parse_words(&["import", "jobs"]).unwrap_err().contains("usage"));
        let cmd = parse_words(&["export", "jobs", "--format", "json", "--stage", "oa"]).unwrap();
        assert!(matches!(cmd, RecordsCmd::Export { format: Format::Json, .. }));
        assert!(parse_words(&["export", "jobs", "--limit", "5"]).unwrap_err().contains("every matching record"));
        assert!(parse_words(&["export", "jobs", "--format", "xml"]).is_err());

        let schema = json!({"fields": [
            {"name": "company", "type": "string"},
            {"name": "salary", "type": "int"},
            {"name": "tech_stack", "type": "list", "of": "string"}]});
        let csv = "id,status,company,salary,tech-stack\nacme,done,\"Acme, Inc\",120,go;rust\n,,Globex,,\n";
        let rows = import_rows("jobs.csv", csv, &schema).unwrap();
        assert_eq!(
            rows,
            vec![
                json!({"id": "acme", "status": "done", "company": "Acme, Inc", "salary": 120, "tech_stack": ["go", "rust"]}),
                json!({"company": "Globex"})
            ],
            "empty cells are unset; lists split on ';'"
        );
        let e = import_rows("jobs.csv", "company,salary\nAcme,lots\n", &schema).unwrap_err();
        assert!(e.message.contains("row 1") && e.message.contains("whole number"), "{}", e.message);
        assert!(import_rows("jobs.csv", "company\nA,B\n", &schema).unwrap_err().message.contains("2 cells"));
        assert_eq!(import_rows("j.json", r#"[{"company": "A"}]"#, &schema).unwrap(), vec![json!({"company": "A"})]);
        assert!(import_rows("j.json", r#"{"company": "A"}"#, &schema).is_err());

        // What export writes, import reads back to the same rows.
        let items = vec![
            json!({"id": "acme", "status": "done", "company": "Acme, Inc", "salary": 120, "tech_stack": ["go", "rust"]}),
            json!({"id": "globex", "status": "todo", "company": "Globex", "salary": null, "tech_stack": null}),
        ];
        let out = export(&items, &schema, Format::Csv);
        assert_eq!(
            out,
            "id,status,company,salary,tech_stack\nacme,done,\"Acme, Inc\",120,go;rust\nglobex,todo,Globex,,"
        );
        let back = import_rows("x.csv", &out, &schema).unwrap();
        assert_eq!(back[0], items[0]);
        assert_eq!(back[1], json!({"id": "globex", "status": "todo", "company": "Globex"}));
        assert!(!export(&items, &schema, Format::Json).contains("null"));
    }

    #[test]
    fn or_conditions_become_alternatives() {
        let words = ["list", "leetcode", "--or", "difficulty=hard", "--or", "attempts>=3", "--status", "todo"];
        let (_, params) = request(&parse_words(&words).unwrap(), &leetcode()).unwrap();
        assert_eq!(
            params["filter"],
            json!({"status": "todo", "or": [{"difficulty": {"eq": "hard"}}, {"attempts": {"gte": 3}}]})
        );
        let (_, params) =
            request(&parse_words(&["list", "leetcode", "--filter", "last_solved>=today-7"]).unwrap(), &leetcode())
                .unwrap();
        assert_eq!(params["filter"], json!({"last_solved": {"gte": "today-7"}}), "the daemon reads 'today'");
    }

    #[test]
    fn list_shows_the_daemons_titles() {
        let schema = json!({"title": "{company}: {position}", "fields": [
            {"name": "company", "type": "string"}, {"name": "position", "type": "string"}]});
        let data = json!({"items": [{"id": "acme", "status": "todo", "company": "Acme", "position": "SWE"}],
                          "total": 1, "titles": {"acme": "Acme: SWE"}});
        let cmd = parse_words(&["list", "jobs"]).unwrap();
        let out = show(&cmd, &data, &schema);
        assert!(out.starts_with("ID    TITLE      STATUS"), "{out}");
        assert!(out.contains("acme  Acme: SWE  todo"), "{out}");
        // A one-field title is already a column.
        let out = show(&cmd, &data, &json!({"title": "{company}", "fields": schema["fields"]}));
        assert!(!out.contains("TITLE"), "{out}");
    }

    #[test]
    fn purge_takes_one_record_or_the_whole_trash() {
        let one = parse_words(&["purge", "jobs/acme"]).unwrap();
        assert_eq!(one, RecordsCmd::Purge { collection: "jobs".into(), id: Some("acme".into()), yes: false });
        assert_eq!(
            request(&one, &Value::Null).unwrap(),
            ("records.purge", json!({"collection": "jobs", "id": "acme"}))
        );
        let all = parse_words(&["purge", "jobs", "--yes"]).unwrap();
        assert_eq!(all, RecordsCmd::Purge { collection: "jobs".into(), id: None, yes: true });
        assert_eq!(show(&all, &json!({"purged": 2}), &Value::Null), "✓ purged 2 record(s) from jobs's trash");

        struct Script;
        impl Prompt for Script {
            fn interactive(&self) -> bool {
                false
            }
            fn confirm(&mut self, _: &str) -> bool {
                unreachable!("never asked without a terminal")
            }
        }
        assert!(confirm_purge(&mut Script, false, "jobs", "x").unwrap_err().message.contains("--yes"));
        assert!(confirm_purge(&mut Script, true, "jobs", "x").unwrap());
    }

    #[test]
    fn datetimes_show_in_local_time() {
        let schema = json!({"fields": [{"name": "at", "type": "datetime"}, {"name": "note", "type": "string"}]});
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-21T06:59:00Z").unwrap();
        let local = at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string();
        assert_eq!(shown(&schema, "at", &json!("2026-10-21T06:59:00Z")), local);
        assert_eq!(shown(&schema, "at", &json!("2026-10-20")), "2026-10-20", "a plain date stays");
        assert_eq!(shown(&schema, "note", &json!("2026-10-21T06:59:00Z")), "2026-10-21T06:59:00Z", "only datetimes");
    }

    #[test]
    fn single_item_output() {
        let item = json!({"id": "two-sum", "status": "done", "title": "Two Sum", "difficulty": "easy",
            "url": null, "last_solved": "2026-09-25", "attempts": null, "starred": null});
        let complete = RecordsCmd::Complete {
            collection: "leetcode".into(),
            id: "two-sum".into(),
            set: vec![],
            unset: vec![],
            items: vec![],
        };
        assert_eq!(show(&complete, &item, &leetcode()), "✓ leetcode/two-sum done (last_solved 2026-09-25)");

        let get = RecordsCmd::Get { collection: "leetcode".into(), id: "two-sum".into() };
        assert!(show(&get, &item, &leetcode()).starts_with("leetcode/two-sum  (done)\n  title        Two Sum\n"));
    }

    #[test]
    fn collections_describe_their_fields() {
        let data = json!({"collections": [leetcode()]});
        let out = show(&RecordsCmd::Collections, &data, &Value::Null);
        assert!(
            out.contains("leetcode  LeetCode  title*, difficulty (easy|medium|hard), url, last_solved (date)"),
            "{out}"
        );
    }

    #[test]
    fn collections_show_roles_and_unique() {
        let jobs = json!({"id": "jobs", "label": "Jobs", "fields": [
            {"name": "company", "type": "string", "required": true},
            {"name": "url", "type": "string", "required": false, "role": "url", "unique": true},
            {"name": "stage", "type": "enum", "values": ["oa", "interview"], "required": false},
            {"name": "oa_deadline", "type": "date", "required": false, "role": "deadline"}
        ]});
        let out = show(&RecordsCmd::Collections, &json!({"collections": [jobs]}), &Value::Null);
        assert!(
            out.contains("company*, url (url, unique), stage (oa|interview), oa_deadline (date, deadline)"),
            "{out}"
        );
    }

    #[test]
    fn collections_list_skipped_files_in_full() {
        let message = "collection file 'broken.toml': TOML parse error at line 1, column 5, key with no value";
        let data = json!({"collections": [leetcode()], "skipped": [message]});
        let out = show(&RecordsCmd::Collections, &data, &Value::Null);
        assert!(out.starts_with("ID        LABEL"), "{out}");
        assert!(out.ends_with(&format!("\nskipped: {message}")), "{out}");

        let only_broken = json!({"collections": [], "skipped": [message]});
        let out = show(&RecordsCmd::Collections, &only_broken, &Value::Null);
        assert!(out.starts_with("no collections yet: see 'shimmer records templates'"), "{out}");
        assert!(out.ends_with(&format!("\nskipped: {message}")), "{out}");
    }

    #[test]
    fn ids_are_never_cut() {
        let id = "longest-substring-without-repeating-characters";
        let add = parse_words(&["add", "leetcode", "--title", "x"]).unwrap();
        assert_eq!(show(&add, &json!({"id": id}), &leetcode()), format!("added leetcode/{id}"));
        let list = parse_words(&["list", "leetcode"]).unwrap();
        let data = json!({"total": 1, "items": [{"id": id, "status": "todo", "title": "Longest Substring Without Repeating Characters"}]});
        let out = show(&list, &data, &leetcode());
        assert!(out.contains(id), "{out}");
        assert!(out.contains("Longest Substring Without Repeating Cha…"), "other long cells are still cut: {out}");
    }

    #[test]
    fn long_values_are_cut() {
        let long = "x".repeat(60);
        let c = cell(&json!(long));
        assert_eq!(c.chars().count(), crate::render::MAX_CELL);
        assert!(c.ends_with('…'));
    }
}
