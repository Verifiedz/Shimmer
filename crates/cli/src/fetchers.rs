//! `shimmer fetchers …`: sources that reach outward for new items (ADR 0028). Every command is
//! one of the `fetchers` module's own ops; nothing here decides anything (CLAUDE.md §2).

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use shimmer_core::{Error, ErrorCode, Result};

use crate::client::Client;
use crate::flags::Flags;
use crate::render::{self, table};
use crate::style::{paint, Role};
use crate::when;
use crate::workspaces::Prompt;

pub const USAGE: &str = "usage: shimmer fetchers <command>

commands:
  list [--removed]      every known source and its configured state; --removed: the ones you
                        removed, to restore or purge
  show SOURCE           one source in full, with its file text
  new SOURCE --interval SPEC [--kind KIND] [--label TEXT] [--enabled] [--set KEY=VALUE]…
                        [--filter JSON]    make a new source
  edit SOURCE [--file FILE]   replace a source's whole file text; interactively, opens $EDITOR
                        on a copy of the current file; a broken edit is refused and the old
                        one kept
  remove SOURCE         remove a source (restore brings it back)
  restore SOURCE        bring a removed source back
  purge [SOURCE] [--yes]      delete a removed source for good, or every removed one (asks first)
  enable SOURCE         switch a source on
  disable SOURCE        switch a source off
  rename SOURCE NEW     give a source a new id; its dedup state and last-run time follow it
  copy SOURCE NEW [--label TEXT]
                        a new source like SOURCE, switched off until you enable it
  status               the whole feature at a glance: how many sources, how many enabled, any problems
  test SOURCE [--filter JSON]   fetch one page and show its parsed items; never saves or marks
                        anything seen
  run SOURCE [--wait] [--filter JSON]  fetch one source now
  items [SOURCE] [--contains W,W] [--excludes W,W]  items a real run has actually found and kept,
                        most recent first

--filter JSON (new, test and run) is a core::filter::Filter (ADR 0028 §8), e.g.
  --filter '{\"title\": {\"contains\": \"senior\"}}'
On test/run, it replaces -- never merges with -- whatever the source's own file configures,
for that one call only. On new, it's the source's own setting, same as --label/--interval/etc.
(edit replaces the whole file, the filter included.)

items' own --contains/--excludes are a separate, client-side narrowing of what's already
been found and saved -- a plain case-insensitive substring match, not a filter.

A source is a file in your data folder, data/fetchers/sources/SOURCE.toml:

  [source]
  enabled = true
  interval = \"daily\"             # a preset (hourly, daily, weekly) or a number of seconds
  kind = \"rss\"                   # absent: SOURCE must be one of the built-in ones
  filter = { title = { contains = \"senior\" } }
  [kind]
  url = \"https://example.test/feed.xml\"

new's --set KEY=VALUE fills in [kind]'s own fields (e.g. --set url=https://example.test/feed.xml);
--kind omitted means SOURCE must already be one of the built-in ones. --interval takes a preset
(hourly, daily, weekly) or a number of seconds; --set's value is read as JSON when it parses as
a number or true/false, otherwise as plain text.

Add --json to any command for raw output.";

/// `core.events.list`'s own query limit for `items` -- generous enough to cover several runs
/// across every source before filtering down to one, well under the daemon's own cap
/// (`MAX_EVENTS_LIMIT`, `crates/daemon/src/core.rs`).
const ITEMS_QUERY_LIMIT: usize = 200;

/// `fetchers new`'s own flags, turned into `fetchers.new`'s params (ADR 0028 §9) by
/// [`source_params`]. `interval` stays raw text here (not yet known to be a preset or a
/// number of seconds) -- that's for the daemon's own `IntervalSpec` to decide.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceInput {
    pub id: String,
    pub kind: Option<String>,
    pub label: Option<String>,
    pub interval: String,
    pub enabled: bool,
    pub params: Map<String, Value>,
    pub filter: Option<Map<String, Value>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FetchersCmd {
    Help,
    /// `removed`: the removed sources instead (ADR 0038 §6).
    List {
        removed: bool,
    },
    Show {
        id: String,
    },
    /// `fetchers.new`'s structured params (ADR 0028 §9): there's no existing file to lose
    /// anything from, so flags are fine.
    New(SourceInput),
    /// A whole-file replace (ADR 0028 §9, amended by #305's review): `--file FILE`, or
    /// (interactively) `$EDITOR` on a copy of the source's current text -- same shape as
    /// `automations edit`/`workspaces edit`, not `new`'s flags, so nothing you don't retype
    /// is silently reset.
    Edit {
        id: String,
        file: Option<std::path::PathBuf>,
    },
    Remove {
        id: String,
    },
    Restore {
        id: String,
    },
    /// `id`: one removed source; `None` is every one (ADR 0038).
    Purge {
        id: Option<String>,
        yes: bool,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Rename {
        id: String,
        to: String,
    },
    /// A new source from another, switched off; `label` its own.
    Copy {
        id: String,
        to: String,
        label: Option<String>,
    },
    Status,
    Test {
        source: String,
        filter: Option<Map<String, Value>>,
    },
    Run {
        source: String,
        wait: bool,
        filter: Option<Map<String, Value>>,
    },
    Items {
        source: Option<String>,
        title_contains: Option<Vec<String>>,
        title_excludes: Option<Vec<String>>,
    },
}

/// `--contains a, b ,c` (or `--excludes`) as a term list -- comma-separated, each term
/// trimmed. `None` means the flag was not given at all, not "given but empty" (`Flags`
/// itself already rejects an empty value, e.g. `--contains=`). Only `items`' own
/// `--contains`/`--excludes` use this now -- a plain client-side substring narrowing, not a
/// `core::filter::Filter` (ADR 0028 §8).
///
/// An empty term is dropped rather than passed on: `--excludes senior,` used to send
/// `["senior", ""]`, and `""` is a substring of every title, so a single stray comma excluded
/// everything.
fn terms(value: Option<&str>) -> Option<Vec<String>> {
    value.map(|s| s.split(',').map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).collect())
}

/// `--filter JSON` (ADR 0028 §8, on `new`/`edit`/`test`/`run`): a `core::filter::Filter` as
/// JSON text, e.g. `--filter '{"title": {"contains": "senior"}}'`. `None` when the flag was
/// not given at all -- sent as nothing, not an empty object, so the daemon falls back to the
/// source's own configured filter (on `test`/`run`) rather than overriding it with "nothing
/// allowed."
fn parse_filter(raw: Option<&str>) -> std::result::Result<Option<Map<String, Value>>, String> {
    let Some(raw) = raw else { return Ok(None) };
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(m)) => Ok(Some(m)),
        Ok(_) => Err("--filter takes a JSON object, e.g. '{\"title\": {\"contains\": \"senior\"}}'".into()),
        Err(e) => Err(format!("--filter: invalid JSON: {e}")),
    }
}

/// `items`' own `--contains`/`--excludes`, narrowing what's already been found rather than
/// what a `Source` fetches. Same matching rule as `fetchers`' own `SourceConfig::matches_title`
/// (case-insensitive substring, OR'd within each list) -- duplicated, not imported, since a
/// client crate must not depend on a module crate (CLAUDE.md §3).
fn title_matches(title: &str, contains: &[String], excludes: &[String]) -> bool {
    let lower = title.to_lowercase();
    let included = contains.is_empty() || contains.iter().any(|w| lower.contains(&w.to_lowercase()));
    let excluded = excludes.iter().any(|w| lower.contains(&w.to_lowercase()));
    included && !excluded
}

/// `words` are what follows `shimmer fetchers`.
pub fn parse(words: Vec<String>) -> std::result::Result<FetchersCmd, String> {
    let mut words = words.into_iter();
    let Some(sub) = words.next() else { return Ok(FetchersCmd::List { removed: false }) };
    let rest: Vec<String> = words.collect();
    match sub.as_str() {
        "help" => Ok(FetchersCmd::Help),
        "new" => parse_source_input(rest),
        "edit" => {
            let flags = Flags::split(rest, &["--file"], &[]).map_err(|e| format!("fetchers edit: {e}"))?;
            let id = match flags.positional.as_slice() {
                [id] => id.clone(),
                _ => return Err("usage: shimmer fetchers edit SOURCE [--file FILE]".into()),
            };
            Ok(FetchersCmd::Edit { id, file: flags.value("--file").map(std::path::PathBuf::from) })
        }
        "list" | "show" | "remove" | "restore" | "purge" | "enable" | "disable" | "rename" | "copy" => {
            parse_source_command(&sub, rest)
        }
        "status" if rest.is_empty() => Ok(FetchersCmd::Status),
        "status" => Err("'fetchers status' takes no arguments".into()),
        "test" => {
            let flags = Flags::split(rest, &["--filter"], &[]).map_err(|e| format!("fetchers test: {e}"))?;
            let source = match flags.positional.as_slice() {
                [s] => s.clone(),
                _ => return Err("usage: shimmer fetchers test SOURCE [--filter JSON]".into()),
            };
            Ok(FetchersCmd::Test { source, filter: parse_filter(flags.value("--filter"))? })
        }
        "run" => {
            let flags = Flags::split(rest, &["--filter"], &["--wait"]).map_err(|e| format!("fetchers run: {e}"))?;
            let source = match flags.positional.as_slice() {
                [s] => s.clone(),
                _ => return Err("usage: shimmer fetchers run SOURCE [--wait] [--filter JSON]".into()),
            };
            Ok(FetchersCmd::Run { source, wait: flags.has("--wait"), filter: parse_filter(flags.value("--filter"))? })
        }
        "items" => {
            let flags =
                Flags::split(rest, &["--contains", "--excludes"], &[]).map_err(|e| format!("fetchers items: {e}"))?;
            let source = match flags.positional.as_slice() {
                [] => None,
                [source] => Some(source.clone()),
                _ => return Err("usage: shimmer fetchers items [SOURCE] [--contains W,W] [--excludes W,W]".into()),
            };
            Ok(FetchersCmd::Items {
                source,
                title_contains: terms(flags.value("--contains")),
                title_excludes: terms(flags.value("--excludes")),
            })
        }
        other => Err(format!("unknown fetchers command '{other}'; see 'shimmer fetchers --help'")),
    }
}

/// `list`/`show`/`remove`/`restore`/`purge`/`enable`/`disable`/`rename`/`copy` -- the
/// source-definition commands (ADR 0038 §3/§6) that take no more than an id, a `to`, and a
/// couple of simple flags. `new`/`edit` are structured ([`parse_source_input`]) and parsed
/// separately.
fn parse_source_command(sub: &str, rest: Vec<String>) -> std::result::Result<FetchersCmd, String> {
    let options: &[&str] = match sub {
        "copy" => &["--label"],
        _ => &[],
    };
    let switches: &[&str] = match sub {
        "purge" => &["--yes"],
        "list" => &["--removed"],
        _ => &[],
    };
    let flags = Flags::split(rest, options, switches).map_err(|e| format!("fetchers {sub}: {e}"))?;
    let usage = |what: &str| format!("usage: shimmer fetchers {sub} {what}");
    let id = |what: &str| match flags.positional.as_slice() {
        [id] => Ok(id.clone()),
        _ => Err(usage(what)),
    };
    Ok(match sub {
        "list" => match flags.positional.as_slice() {
            [] => FetchersCmd::List { removed: flags.has("--removed") },
            _ => return Err("'fetchers list' takes no arguments".into()),
        },
        "show" => FetchersCmd::Show { id: id("SOURCE")? },
        "remove" => FetchersCmd::Remove { id: id("SOURCE")? },
        "restore" => FetchersCmd::Restore { id: id("REMOVED")? },
        "purge" => match flags.positional.as_slice() {
            [] => FetchersCmd::Purge { id: None, yes: flags.has("--yes") },
            [id] => FetchersCmd::Purge { id: Some(id.clone()), yes: flags.has("--yes") },
            _ => return Err(usage("[REMOVED] [--yes]")),
        },
        "enable" => FetchersCmd::Enable { id: id("SOURCE")? },
        "disable" => FetchersCmd::Disable { id: id("SOURCE")? },
        "rename" | "copy" => {
            let [id, to]: [String; 2] = flags.positional.clone().try_into().map_err(|_| match sub {
                "rename" => usage("SOURCE NEW"),
                _ => usage("SOURCE NEW [--label TEXT]"),
            })?;
            match sub {
                "rename" => FetchersCmd::Rename { id, to },
                _ => FetchersCmd::Copy { id, to, label: flags.value("--label").map(str::to_owned) },
            }
        }
        _ => unreachable!("parse_source_command only called for its own listed subcommands"),
    })
}

/// `--set KEY=VALUE`'s value: read as JSON when it parses as one (a number, `true`/`false`,
/// a quoted string), otherwise kept as plain text -- the same convention `--set` flags
/// elsewhere in the CLI avoid needing by only ever taking strings, but a kind's own fields
/// (`page_size`, `active_field`) are a mix of numbers, bools and strings, and there is no
/// single type to assume.
fn set_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| json!(raw))
}

/// `new SOURCE --interval SPEC [--kind KIND] [--label TEXT] [--enabled] [--set KEY=VALUE]…
/// [--filter JSON]`.
fn parse_source_input(words: Vec<String>) -> std::result::Result<FetchersCmd, String> {
    let usage = "usage: shimmer fetchers new SOURCE --interval SPEC [--kind KIND] [--label TEXT] [--enabled] \
         [--set KEY=VALUE]… [--filter JSON]"
        .to_owned();
    let mut words = words.into_iter();
    let id = words.next().ok_or_else(|| usage.clone())?;
    if id.starts_with('-') {
        return Err(usage);
    }
    let (mut kind, mut label, mut interval, mut filter) = (None, None, None, None);
    let mut enabled = false;
    let mut params = Map::new();
    while let Some(word) = words.next() {
        let mut value = |flag: &str| words.next().ok_or_else(|| format!("{flag} needs a value"));
        match word.as_str() {
            "--kind" => kind = Some(value("--kind")?),
            "--label" => label = Some(value("--label")?),
            "--interval" => interval = Some(value("--interval")?),
            "--enabled" => enabled = true,
            "--filter" => filter = Some(value("--filter")?),
            "--set" => {
                let kv = value("--set")?;
                let (k, v) = kv.split_once('=').ok_or_else(|| "--set needs KEY=VALUE".to_owned())?;
                params.insert(k.to_owned(), set_value(v));
            }
            other => return Err(format!("unknown option '{other}' for 'fetchers new'")),
        }
    }
    let interval = interval.ok_or(usage)?;
    Ok(FetchersCmd::New(SourceInput {
        id,
        kind,
        label,
        interval,
        enabled,
        params,
        filter: parse_filter(filter.as_deref())?,
    }))
}

pub async fn run(client: &mut Client, cmd: &FetchersCmd, json_out: bool, prompt: &mut impl Prompt) -> Result<String> {
    let one = |id: &str| json!({ "id": id });
    match cmd {
        FetchersCmd::Help => Ok(USAGE.into()),
        FetchersCmd::List { removed: false } => {
            let data = client.call("fetchers.list", json!({})).await?;
            Ok(if json_out { render::json(&data) } else { list(&data, Utc::now()) })
        }
        FetchersCmd::List { removed: true } => {
            let data = client.call("fetchers.list", json!({"removed": true})).await?;
            Ok(if json_out { render::json(&data) } else { removed_list(&data) })
        }
        FetchersCmd::Show { id } => {
            let data = client.call("fetchers.show", one(id)).await?;
            Ok(if json_out { render::json(&data) } else { show(&data) })
        }
        FetchersCmd::New(input) => {
            let data = client.call("fetchers.new", source_params(input)).await?;
            Ok(if json_out { render::json(&data) } else { format!("✓ made source {}", source_summary(&data)) })
        }
        FetchersCmd::Edit { id, file: Some(file) } => {
            let data = client.call("fetchers.edit", json!({"id": id, "text": read_file(file)?})).await?;
            Ok(if json_out { render::json(&data) } else { format!("✓ saved source {}", source_summary(&data)) })
        }
        FetchersCmd::Edit { id, file: None } => {
            if !prompt.interactive() {
                return Err(Error::invalid_params(format!(
                    "fetchers edit needs a terminal; replace the source from a file instead: \
                     shimmer fetchers edit {id} --file FILE"
                )));
            }
            let text = source_text(client, id).await?;
            edit_loop(client, id, text, prompt).await.map(|saved| match saved {
                true => format!("✓ saved source {id}"),
                false => format!("no changes to {id}"),
            })
        }
        FetchersCmd::Remove { id } => {
            let data = client.call("fetchers.remove", one(id)).await?;
            Ok(if json_out {
                render::json(&data)
            } else {
                format!("✓ removed source {id}\n  bring it back with: shimmer fetchers restore {id}")
            })
        }
        FetchersCmd::Restore { id } => {
            let data = client.call("fetchers.restore", one(id)).await?;
            Ok(if json_out {
                render::json(&data)
            } else {
                match data["problem"].as_str() {
                    None => format!("✓ restored source {}", source_summary(&data["source"])),
                    Some(problem) => format!(
                        "✓ restored {id}, but it can't run: {problem}\n  fix it with: shimmer fetchers edit {id}"
                    ),
                }
            })
        }
        FetchersCmd::Purge { id, yes } => {
            let mut params = json!({});
            if let Some(id) = id {
                params["id"] = json!(id);
            }
            let data = match client.call("fetchers.purge", params.clone()).await {
                Err(e) if e.code == ErrorCode::ConfirmationRequired => {
                    let what = match id {
                        Some(id) => format!("the removed source '{id}'"),
                        None => "every removed source".to_owned(),
                    };
                    if !crate::records::confirm_purge(prompt, *yes, &what, &e.message)? {
                        return Ok("nothing purged".into());
                    }
                    params["confirm"] = e.detail.unwrap_or_default();
                    client.call("fetchers.purge", params).await?
                }
                other => other?,
            };
            Ok(if json_out {
                render::json(&data)
            } else {
                match (data["purged"].as_u64().unwrap_or(0), id) {
                    (0, _) => "no removed sources to purge".to_owned(),
                    (_, Some(id)) => format!("✓ {id} is deleted for good"),
                    (n, None) => format!("✓ {n} removed source(s) deleted for good"),
                }
            })
        }
        FetchersCmd::Enable { id } | FetchersCmd::Disable { id } => {
            let on = matches!(cmd, FetchersCmd::Enable { .. });
            let op = if on { "fetchers.enable" } else { "fetchers.disable" };
            let data = client.call(op, one(id)).await?;
            Ok(if json_out {
                render::json(&data)
            } else if on {
                format!("✓ {id} is on")
            } else {
                format!("✓ {id} is off: it won't run until enabled")
            })
        }
        FetchersCmd::Rename { id, to } => {
            let data = client.call("fetchers.rename", json!({"id": id, "to": to})).await?;
            Ok(if json_out { render::json(&data) } else { format!("✓ {id} is now {to}") })
        }
        FetchersCmd::Copy { id, to, label } => {
            let mut params = json!({"id": id, "to": to});
            if let Some(label) = label {
                params["label"] = json!(label);
            }
            let data = client.call("fetchers.copy", params).await?;
            Ok(if json_out {
                render::json(&data)
            } else {
                format!(
                    "✓ made {to} from {id}, switched off so the same schedule doesn't fire both\n  \
                     turn it on when it's ready: shimmer fetchers enable {to}"
                )
            })
        }
        FetchersCmd::Status => {
            let data = client.call("fetchers.status", json!({})).await?;
            Ok(if json_out { render::json(&data) } else { status(&data) })
        }
        FetchersCmd::Test { source, filter } => {
            // Always follows: a dry run that returned immediately with just a task id would
            // defeat its own point, unlike `run`, which has something useful to say either way.
            client.subscribe(&["queue.task.*"]).await?;
            let params = filter_params(source, filter);
            let handle = client.call("fetchers.test", params).await?;
            let task = task_id(&handle)?;
            let result = follow(client, task).await?;
            Ok(if json_out { render::json(&result) } else { test_items(source, &result) })
        }
        FetchersCmd::Run { source, wait, filter } => {
            if *wait {
                // Before the request, so not even the first event can be missed.
                client.subscribe(&["queue.task.*"]).await?;
            }
            let params = filter_params(source, filter);
            let handle = client.call("fetchers.run", params).await?;
            let task = task_id(&handle)?;
            if !*wait {
                return Ok(if json_out {
                    render::json(&handle)
                } else {
                    format!("queued a fetch of {source} (task {task})\ncheck on it with: shimmer fetchers status")
                });
            }
            let result = follow(client, task).await?;
            Ok(if json_out { render::json(&result) } else { fetched(source, &result) })
        }
        FetchersCmd::Items { source, title_contains, title_excludes } => {
            // fetchers.item.found is a plain string literal, not an import from shimmer-fetchers
            // -- a client crate must not depend on a module crate (CLAUDE.md §3's dependency
            // rules), same reason every other op name in this file is also a literal.
            let data = client
                .call("core.events.list", json!({"topic": "fetchers.item.found", "limit": ITEMS_QUERY_LIMIT}))
                .await?;
            let events = data["events"].as_array().cloned().unwrap_or_default();
            let contains = title_contains.clone().unwrap_or_default();
            let excludes = title_excludes.clone().unwrap_or_default();
            let matching: Vec<Value> = events
                .into_iter()
                .filter(|e| match source {
                    None => true,
                    Some(s) => e["payload"]["source_name"].as_str() == Some(s.as_str()),
                })
                .filter(|e| title_matches(e["payload"]["title"].as_str().unwrap_or_default(), &contains, &excludes))
                .collect();
            Ok(if json_out { render::json(&json!({"items": matching})) } else { items(source.as_deref(), &matching) })
        }
    }
}

/// Read a local file for `fetchers edit --file FILE`; a missing or unreadable one is
/// `invalid_params`, not a panic.
fn read_file(file: &std::path::Path) -> Result<String> {
    std::fs::read_to_string(file).map_err(|e| Error::invalid_params(format!("can't read {}: {e}", file.display())))
}

async fn source_text(client: &mut Client, id: &str) -> Result<String> {
    let data = client.call("fetchers.show", json!({"id": id})).await?;
    Ok(data["text"].as_str().unwrap_or_default().to_owned())
}

/// Edit a copy of the source's file in `$EDITOR` and send it with `fetchers.edit`; a refused
/// edit can be tried again with your text kept, same as `automations edit` (ADR 0035 §8).
/// True when something was saved.
async fn edit_loop(client: &mut Client, id: &str, text: String, prompt: &mut impl Prompt) -> Result<bool> {
    let path = new_private_file(id, &text)?;
    loop {
        crate::editor::open(&path)?;
        let edited = read_file(&path)?;
        if edited == text {
            let _ = std::fs::remove_file(&path);
            return Ok(false);
        }
        match client.call("fetchers.edit", json!({"id": id, "text": edited})).await {
            Ok(_) => {
                let _ = std::fs::remove_file(&path);
                return Ok(true);
            }
            Err(e) if e.code == ErrorCode::InvalidParams => {
                eprintln!("{} {}", paint(Role::Error, "✗ not saved:"), e.message);
                if !prompt.confirm("Edit it again?") {
                    return Err(Error::invalid_params(format!(
                        "{id} not saved (the old source is unchanged); your text is in {}",
                        path.display()
                    )));
                }
            }
            // Whatever went wrong (the source removed meanwhile, the daemon restarted), the
            // edit isn't lost: say where it is (#253's review).
            Err(e) => {
                return Err(Error::new(
                    e.code,
                    format!("{}; {id} not saved, your text is in {}", e.message, path.display()),
                ))
            }
        }
    }
}

/// A new file in the temp folder holding `text`, readable only by you, at a name nobody else
/// can have made first: it's created with `create_new`, so a file or link someone planted
/// there is refused rather than written through (#253's review).
fn new_private_file(id: &str, text: &str) -> Result<std::path::PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("shimmer-source-{id}-{}-{stamp}.toml", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| Error::internal(format!("can't create {}: {e}", path.display())))?;
    f.write_all(text.as_bytes()).map_err(|e| Error::internal(format!("can't write {}: {e}", path.display())))?;
    Ok(path)
}

/// `fetchers.new`'s params from a [`SourceInput`] (ADR 0028 §9): structured, not raw file
/// text -- there's no existing file for `new` to merge onto, so flags are fine (unlike
/// `edit`, which takes the whole file text, see [`FetchersCmd::Edit`]). The daemon validates
/// `kind`'s own fields through `Kind::parse`, the same as a hand-edited file. `interval` is
/// sent as a JSON number when it parses as one, otherwise as a string (a preset name) --
/// `IntervalSpec`'s own untagged `Preset(String) | Seconds(u64)`.
fn source_params(input: &SourceInput) -> Value {
    let mut p = json!({
        "id": input.id,
        "enabled": input.enabled,
        "interval": interval_value(&input.interval),
        "params": input.params,
    });
    if let Some(k) = &input.kind {
        p["kind"] = json!(k);
    }
    if let Some(l) = &input.label {
        p["label"] = json!(l);
    }
    if let Some(f) = &input.filter {
        p["filter"] = json!(f);
    }
    p
}

fn interval_value(s: &str) -> Value {
    match s.parse::<u64>() {
        Ok(n) => json!(n),
        Err(_) => json!(s),
    }
}

/// `id` (kind): enabled/disabled, for a one-line confirmation after `new`/`edit`/`restore`.
fn source_summary(source: &Value) -> String {
    format!(
        "{} ({}): {}",
        full(&source["id"]),
        source["kind"].as_str().unwrap_or("built in"),
        if source["enabled"] == true { "on" } else { "off" }
    )
}

/// A value in full, for sentences; `render::cell` shortens, for table cells.
fn full(v: &Value) -> String {
    match v {
        Value::Null => "-".to_owned(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// One source in full: its state and its file text.
fn show(s: &Value) -> String {
    let mut out =
        format!("{}  ({})\n", paint(Role::Name, &full(&s["id"])), if s["enabled"] == true { "on" } else { "off" });
    let mut line = |label: &str, value: String| out.push_str(&format!("  {}{value}\n", crate::style::label(label, 12)));
    line("kind", s["kind"].as_str().unwrap_or("built in").to_owned());
    line("label", full(&s["label"]));
    line("interval", s["interval_s"].as_u64().map(when::format_duration).unwrap_or_else(|| "-".into()));
    out.push('\n');
    out.push_str(s["text"].as_str().unwrap_or_default().trim_end());
    out
}

/// `list --removed`: each removed source's id, and how to bring it back or delete it for good.
fn removed_list(data: &Value) -> String {
    let ids = data["removed"].as_array().cloned().unwrap_or_default();
    if ids.is_empty() {
        return "no removed sources".into();
    }
    let rows: Vec<Vec<String>> = ids.iter().map(|id| vec![render::cell(id)]).collect();
    let mut out = table(&["SOURCE".into()], &rows);
    out.push_str("\nbring one back: shimmer fetchers restore SOURCE; delete for good: shimmer fetchers purge [SOURCE]");
    out
}

/// `fetchers.run`/`fetchers.test`'s params: `id`, plus `filter` only when `--filter` was
/// actually given -- omitted entirely, not sent as an empty object, so the module falls back
/// to the source's own configured filter (ADR 0028 §8/§14) rather than overriding it with
/// "nothing allowed."
fn filter_params(source: &str, filter: &Option<Map<String, Value>>) -> Value {
    let mut params = json!({"id": source});
    if let Some(f) = filter {
        params["filter"] = json!(f);
    }
    params
}

fn task_id(handle: &Value) -> Result<&str> {
    handle["task_id"].as_str().ok_or_else(|| Error::internal("no task_id in the queued response"))
}

fn fetched(source: &str, result: &Value) -> String {
    let n = result["new_items"].as_u64().unwrap_or(0);
    format!("✓ fetched {source}: {n} new item{}", if n == 1 { "" } else { "s" })
}

/// Follow a queued task over the subscription stream until it ends, the same pattern
/// `workspaces::queued`'s `follow` uses for a launch -- `queue.task.finished`'s own event
/// payload carries the handler's full return value, so this never needs the task's
/// persisted record, which keeps no result at all for a succeeded task (only its error, for
/// a failed one; see `finished_task`'s own note on the one path where that matters).
async fn follow(client: &mut Client, task: &str) -> Result<Value> {
    loop {
        let ev = client.next_event().await?;
        if ev.topic == "core.stream.lagged" {
            // Events were dropped, maybe the one we wait for: ask directly instead.
            if let Some(outcome) = finished_task(client, task).await? {
                return outcome;
            }
            continue;
        }
        if ev.payload["task_id"] != task {
            continue;
        }
        match ev.topic.as_str() {
            "queue.task.finished" => return Ok(ev.payload["result"].clone()),
            "queue.task.failed" => return Err(task_error(&ev.payload)),
            "queue.task.cancelled" => return Err(Error::new(ErrorCode::ModuleError, "the task was cancelled")),
            _ => {}
        }
    }
}

/// After `core.stream.lagged`: has the task ended? `None` while it is still queued or
/// running. `queue.show`'s response has no `result` field for a succeeded task -- only
/// `error` for a failed one (`crates/daemon/src/queue/mod.rs`'s `view`) -- so a succeeded
/// outcome here is reported as a (rare) miss, honestly, rather than as a silent empty result.
async fn finished_task(client: &mut Client, task: &str) -> Result<Option<Result<Value>>> {
    let t = client.call("queue.show", json!({ "task_id": task })).await?;
    Ok(match t["status"].as_str() {
        Some("succeeded") => Some(Err(Error::new(
            ErrorCode::Unavailable,
            "the fetch finished, but its result was missed (events were dropped); see 'shimmer fetchers status'",
        ))),
        Some("failed") => {
            Some(Err(Error::new(ErrorCode::ModuleError, t["error"].as_str().unwrap_or("the task failed").to_owned())))
        }
        Some("cancelled") => Some(Err(Error::new(ErrorCode::ModuleError, "the task was cancelled"))),
        _ => None,
    })
}

fn task_error(payload: &Value) -> Error {
    let code: ErrorCode = serde_json::from_value(payload["code"].clone()).unwrap_or(ErrorCode::ModuleError);
    Error::new(code, payload["error"].as_str().unwrap_or("the task failed").to_owned())
}

/// One table of every known source and its configured state, or a line saying there are none.
/// One table of every known source, its configured state, and its last run (ADR 0028 §9 merges
/// the prior design's separate per-source `list`/`status` into one).
fn list(data: &Value, now: DateTime<Utc>) -> String {
    let sources = data["sources"].as_array().cloned().unwrap_or_default();
    if sources.is_empty() {
        return "no sources known".into();
    }
    let rows: Vec<Vec<String>> = sources
        .iter()
        .map(|s| {
            let last_run = when::timestamp(&s["last_run_at"]);
            vec![
                render::cell(&s["id"]),
                render::cell(&s["method"]),
                s["kind"].as_str().unwrap_or("-").to_string(),
                // on/off under STATE, coloured like every other feature's (ADR 0038 §6).
                if s["enabled"].as_bool().unwrap_or(false) { "on".into() } else { "off".into() },
                s["interval_s"].as_u64().map(when::format_duration).unwrap_or_else(|| "-".into()),
                last_run.map_or("never".into(), |t| when::relative(t, now)),
                if s["running"].as_bool().unwrap_or(false) { "running".into() } else { "idle".into() },
            ]
        })
        .collect();
    let header = ["ID", "METHOD", "KIND", "STATE", "INTERVAL", "LAST RUN", ""].map(String::from);
    let n = sources.len();
    format!("{}\n{n} source{}", table(&header, &rows), if n == 1 { "" } else { "s" })
}

/// The whole feature at a glance (ADR 0028 §9): not per-source -- that's `fetchers.list`/
/// `fetchers.show` now -- the same "feature-wide summary" shape `automations.status` has.
fn status(data: &Value) -> String {
    let total = data["sources"].as_u64().unwrap_or(0);
    let enabled = data["enabled"].as_u64().unwrap_or(0);
    let disabled = data["disabled"].as_u64().unwrap_or(0);
    let mut out =
        format!("{total} source{}: {enabled} enabled, {disabled} disabled", if total == 1 { "" } else { "s" });
    let problems: Vec<Vec<String>> = data["problems"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| vec![render::cell(&p["id"]), full(&p["problem"])])
        .collect();
    if !problems.is_empty() {
        out.push_str("\n\n");
        out.push_str(&paint(Role::Warning, "problems:"));
        out.push('\n');
        out.push_str(&table(&["ID".into(), "PROBLEM".into()], &problems));
    }
    out
}

/// Every parsed item `fetchers.test` found, title and url, or a line saying it found none.
fn test_items(source: &str, result: &Value) -> String {
    let items = result["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return format!("{source}: no items parsed");
    }
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            // The url must survive copy/paste intact -- render::cell's truncation is fine for a
            // title (a preview), but a truncated url is a different, invalid url, not a shorter
            // version of the real one, and silently hands back a 404 instead of the job posting.
            // Still goes through strip_control: a raw control character has no legitimate place
            // in a url (RFC 3986 requires percent-encoding one), so dropping it changes nothing
            // about where a real link points, but it is the one thing that bypasses render::cell
            // and its own stripping.
            vec![render::cell(&item["title"]), render::strip_control(item["url"].as_str().unwrap_or_default())]
        })
        .collect();
    let n = items.len();
    format!(
        "{source}: {n} item{}\n{}",
        if n == 1 { "" } else { "s" },
        table(&["TITLE".to_owned(), "URL".to_owned()], &rows)
    )
}

/// Every `fetchers.item.found` event matched, most recent first -- `core.events.list` itself
/// returns oldest-first (the event-log's own convention), reversed here only for display,
/// same reasoning as a feed or a log tail: what just happened belongs on top.
fn items(source: Option<&str>, events: &[Value]) -> String {
    if events.is_empty() {
        return match source {
            Some(s) => format!("{s}: no items found yet"),
            None => "no items found yet".into(),
        };
    }
    let n = events.len();
    let summary = match source {
        Some(s) => format!("{s}: {n} item{}", if n == 1 { "" } else { "s" }),
        None => format!("{n} item{}", if n == 1 { "" } else { "s" }),
    };
    // Never truncated, same reasoning as test_items's own url handling.
    let url = |p: &Value| render::strip_control(p["url"].as_str().unwrap_or_default());
    let (header, rows): (Vec<String>, Vec<Vec<String>>) = match source {
        Some(_) => (
            ["TITLE", "URL"].map(String::from).to_vec(),
            events.iter().rev().map(|ev| vec![render::cell(&ev["payload"]["title"]), url(&ev["payload"])]).collect(),
        ),
        None => (
            ["SOURCE", "TITLE", "URL"].map(String::from).to_vec(),
            events
                .iter()
                .rev()
                .map(|ev| {
                    let p = &ev["payload"];
                    vec![p["source_name"].as_str().unwrap_or("?").to_owned(), render::cell(&p["title"]), url(p)]
                })
                .collect(),
        ),
    };
    format!("{summary}\n{}", table(&header, &rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &[&str]) -> std::result::Result<FetchersCmd, String> {
        parse(s.iter().map(|w| w.to_string()).collect())
    }

    #[test]
    fn commands_parse() {
        assert_eq!(p(&[]), Ok(FetchersCmd::List { removed: false }));
        assert_eq!(p(&["list"]), Ok(FetchersCmd::List { removed: false }));
        assert_eq!(p(&["list", "--removed"]), Ok(FetchersCmd::List { removed: true }));
        assert_eq!(p(&["status"]), Ok(FetchersCmd::Status));
        assert_eq!(p(&["show", "blog"]), Ok(FetchersCmd::Show { id: "blog".into() }));
        assert_eq!(
            p(&["new", "blog", "--interval", "daily"]),
            Ok(FetchersCmd::New(SourceInput {
                id: "blog".into(),
                kind: None,
                label: None,
                interval: "daily".into(),
                enabled: false,
                params: Map::new(),
                filter: None,
            }))
        );
        assert_eq!(
            p(&[
                "new",
                "blog",
                "--interval",
                "3600",
                "--kind",
                "rss",
                "--label",
                "My Blog",
                "--enabled",
                "--set",
                "url=https://example.test/f.xml",
                "--filter",
                "{\"title\": {\"contains\": \"junior\"}}",
            ]),
            Ok(FetchersCmd::New(SourceInput {
                id: "blog".into(),
                kind: Some("rss".into()),
                label: Some("My Blog".into()),
                interval: "3600".into(),
                enabled: true,
                params: json!({"url": "https://example.test/f.xml"}).as_object().unwrap().clone(),
                filter: Some(json!({"title": {"contains": "junior"}}).as_object().unwrap().clone()),
            }))
        );
        assert_eq!(p(&["edit", "blog"]), Ok(FetchersCmd::Edit { id: "blog".into(), file: None }));
        assert_eq!(
            p(&["edit", "blog", "--file", "source.toml"]),
            Ok(FetchersCmd::Edit { id: "blog".into(), file: Some("source.toml".into()) })
        );
        assert_eq!(p(&["remove", "blog"]), Ok(FetchersCmd::Remove { id: "blog".into() }));
        assert_eq!(p(&["restore", "blog"]), Ok(FetchersCmd::Restore { id: "blog".into() }));
        assert_eq!(p(&["purge"]), Ok(FetchersCmd::Purge { id: None, yes: false }));
        assert_eq!(p(&["purge", "blog", "--yes"]), Ok(FetchersCmd::Purge { id: Some("blog".into()), yes: true }));
        assert_eq!(p(&["enable", "blog"]), Ok(FetchersCmd::Enable { id: "blog".into() }));
        assert_eq!(p(&["disable", "blog"]), Ok(FetchersCmd::Disable { id: "blog".into() }));
        assert_eq!(
            p(&["rename", "blog", "team-blog"]),
            Ok(FetchersCmd::Rename { id: "blog".into(), to: "team-blog".into() })
        );
        assert_eq!(
            p(&["copy", "blog", "blog-2", "--label", "Blog copy"]),
            Ok(FetchersCmd::Copy { id: "blog".into(), to: "blog-2".into(), label: Some("Blog copy".into()) })
        );
        assert_eq!(
            p(&["test", "hn-whoishiring"]),
            Ok(FetchersCmd::Test { source: "hn-whoishiring".into(), filter: None })
        );
        assert_eq!(
            p(&["run", "hn-whoishiring"]),
            Ok(FetchersCmd::Run { source: "hn-whoishiring".into(), wait: false, filter: None })
        );
        assert_eq!(
            p(&["run", "hn-whoishiring", "--wait"]),
            Ok(FetchersCmd::Run { source: "hn-whoishiring".into(), wait: true, filter: None })
        );
        assert_eq!(
            p(&["run", "hn-whoishiring", "--filter", "{\"title\": {\"not_contains\": \"senior\"}}"]),
            Ok(FetchersCmd::Run {
                source: "hn-whoishiring".into(),
                wait: false,
                filter: Some(json!({"title": {"not_contains": "senior"}}).as_object().unwrap().clone()),
            })
        );
        assert_eq!(
            p(&["test", "hn-whoishiring", "--filter", "{\"title\": {\"contains\": \"junior\"}}"]),
            Ok(FetchersCmd::Test {
                source: "hn-whoishiring".into(),
                filter: Some(json!({"title": {"contains": "junior"}}).as_object().unwrap().clone()),
            })
        );
        assert_eq!(p(&["items"]), Ok(FetchersCmd::Items { source: None, title_contains: None, title_excludes: None }));
        assert_eq!(
            p(&["items", "jobbank-canada"]),
            Ok(FetchersCmd::Items {
                source: Some("jobbank-canada".into()),
                title_contains: None,
                title_excludes: None
            })
        );
        assert_eq!(
            p(&["items", "jobbank-canada", "--contains", "junior,entry level", "--excludes", "senior"]),
            Ok(FetchersCmd::Items {
                source: Some("jobbank-canada".into()),
                title_contains: Some(vec!["junior".into(), "entry level".into()]),
                title_excludes: Some(vec!["senior".into()]),
            })
        );
    }

    #[test]
    fn mistakes_are_explained() {
        assert!(p(&["list", "x"]).unwrap_err().contains("takes no arguments"));
        assert!(p(&["status", "x"]).unwrap_err().contains("takes no arguments"));
        assert!(p(&["test"]).unwrap_err().contains("usage: shimmer fetchers test SOURCE"));
        assert!(p(&["run"]).unwrap_err().contains("usage: shimmer fetchers run SOURCE"));
        assert!(p(&["run", "a", "--bogus"]).unwrap_err().contains("unknown option '--bogus'"));
        assert!(p(&["items", "a", "b"]).unwrap_err().contains("usage: shimmer fetchers items"));
        assert!(p(&["frob"]).unwrap_err().contains("unknown fetchers command 'frob'"));
        assert!(p(&["new"]).unwrap_err().contains("usage: shimmer fetchers new SOURCE --interval SPEC"));
        assert!(p(&["new", "blog"]).unwrap_err().contains("usage: shimmer fetchers new SOURCE --interval SPEC"));
        assert!(p(&["new", "blog", "--interval"]).unwrap_err().contains("--interval needs a value"));
        assert!(p(&["new", "blog", "--interval", "daily", "--set", "nope"]).unwrap_err().contains("KEY=VALUE"));
        assert!(p(&["new", "blog", "--interval", "daily", "--bogus"])
            .unwrap_err()
            .contains("unknown option '--bogus' for 'fetchers new'"));
        assert!(p(&["edit"]).unwrap_err().contains("usage: shimmer fetchers edit SOURCE [--file FILE]"));
        assert!(p(&["edit", "blog", "extra"]).unwrap_err().contains("usage: shimmer fetchers edit SOURCE"));
    }

    #[test]
    fn list_renders_a_table_with_a_count() {
        let now = Utc::now();
        let data = json!({"sources": [
            {"id": "hn-whoishiring", "method": "api", "kind": null, "enabled": true, "interval_s": 86_400,
             "last_run_at": (now - chrono::Duration::seconds(90)).to_rfc3339(), "running": false},
            {"id": "my-team-blog", "method": "api", "kind": "rss", "enabled": false, "interval_s": null,
             "last_run_at": null, "running": true},
        ]});
        let out = list(&data, now);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["ID", "METHOD", "KIND", "STATE", "INTERVAL", "LAST", "RUN"]
        );
        assert!(
            lines[1].contains("hn-whoishiring")
                && lines[1].contains(" on ")
                && lines[1].contains("1d")
                && lines[1].contains("ago")
                && lines[1].ends_with("idle"),
            "{out}"
        );
        assert!(
            lines[2].contains("my-team-blog")
                && lines[2].contains("rss")
                && lines[2].contains(" off ")
                && lines[2].contains("never")
                && lines[2].ends_with("running"),
            "{out}"
        );
        assert_eq!(lines[3], "2 sources");
    }

    #[test]
    fn list_with_nothing_known_says_so() {
        assert_eq!(list(&json!({"sources": []}), Utc::now()), "no sources known");
    }

    #[test]
    fn status_renders_the_feature_wide_summary_and_any_problems() {
        let data = json!({"sources": 5, "enabled": 2, "disabled": 3, "problems": []});
        assert_eq!(status(&data), "5 sources: 2 enabled, 3 disabled");

        let with_problems =
            json!({"sources": 1, "enabled": 0, "disabled": 1, "problems": [{"id": "broken", "problem": "bad toml"}]});
        let out = status(&with_problems);
        assert!(out.contains("1 source: 0 enabled, 1 disabled"), "{out}");
        assert!(out.contains("broken") && out.contains("bad toml"), "{out}");
    }

    #[test]
    fn test_items_lists_title_and_url_or_says_there_were_none() {
        let data = json!({"source": "hn-whoishiring", "items": [{"title": "Acme is hiring", "url": "https://x/1"}]});
        let out = test_items("hn-whoishiring", &data);
        assert_eq!(out, "hn-whoishiring: 1 item\nTITLE           URL\nAcme is hiring  https://x/1");
        assert_eq!(test_items("hn-whoishiring", &json!({"items": []})), "hn-whoishiring: no items parsed");
    }

    #[test]
    fn test_items_never_truncates_the_url_even_when_the_title_does() {
        // A truncated title is still a readable preview; a truncated url is a different,
        // invalid url that 404s if anyone follows it -- the two must not be treated the same.
        let long_title = "x".repeat(80);
        let long_url = format!("https://www.jobbank.gc.ca/jobsearch/jobposting/{}", "1".repeat(80));
        let data = json!({"items": [{"title": long_title, "url": long_url}]});
        let out = test_items("jobbank-canada", &data);
        assert!(out.ends_with(&long_url), "the full url must survive untouched:\n{out}");
        assert!(out.contains('…'), "the long title should still be shortened for display");
    }

    #[test]
    fn test_items_strips_control_characters_from_both_title_and_url() {
        // Both are spec-valid ways a remote source can carry a raw control byte -- a JSON
        // `\u001b` escape (decoded here directly, same as serde_json would) and a url that
        // never legitimately contains one (RFC 3986 requires percent-encoding it).
        let data = json!({"items": [{
            "title": "Click here\u{1b}[8mhidden\u{1b}[0m",
            "url": "https://x/1\u{1b}[2J",
        }]});
        let out = test_items("evil-source", &data);
        assert!(!out.contains('\u{1b}'), "no raw control character must reach the rendered output:\n{out:?}");
    }

    fn found(source: &str, title: &str, url: &str) -> Value {
        json!({"topic": "fetchers.item.found", "payload": {"source_name": source, "title": title, "url": url}})
    }

    #[test]
    fn items_groups_by_source_when_none_is_given_and_shows_newest_first() {
        let events = vec![
            found("hn-whoishiring", "Acme hiring", "https://x/1"),
            found("weworkremotely", "Rust role", "https://x/2"),
        ];
        let out = items(None, &events);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "2 items");
        assert_eq!(lines[1], "SOURCE          TITLE        URL");
        // events arrive oldest-first; display must flip to newest-first.
        assert!(lines[2].contains("weworkremotely") && lines[2].contains("Rust role"), "{out}");
        assert!(lines[3].contains("hn-whoishiring") && lines[3].contains("Acme hiring"), "{out}");
    }

    #[test]
    fn items_for_one_source_omits_the_source_label() {
        let events = vec![found("jobbank-canada", "Software Engineer", "https://x/1")];
        let out = items(Some("jobbank-canada"), &events);
        assert_eq!(out, "jobbank-canada: 1 item\nTITLE              URL\nSoftware Engineer  https://x/1");
    }

    #[test]
    fn items_with_nothing_found_says_so_either_way() {
        assert_eq!(items(None, &[]), "no items found yet");
        assert_eq!(items(Some("jobbank-canada"), &[]), "jobbank-canada: no items found yet");
    }

    #[test]
    fn fetched_pluralises_new_items() {
        assert_eq!(fetched("hn-whoishiring", &json!({"new_items": 0})), "✓ fetched hn-whoishiring: 0 new items");
        assert_eq!(fetched("hn-whoishiring", &json!({"new_items": 1})), "✓ fetched hn-whoishiring: 1 new item");
    }

    #[test]
    fn terms_splits_and_trims_a_comma_list_or_is_none_when_the_flag_was_absent() {
        assert_eq!(terms(None), None);
        let expected: Vec<String> = ["junior", "entry level", "new grad"].iter().map(|s| s.to_string()).collect();
        assert_eq!(terms(Some("junior, entry level ,new grad")), Some(expected));
        assert_eq!(terms(Some("junior")), Some(vec!["junior".to_string()]));
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
    fn filter_params_omits_a_field_entirely_when_its_flag_was_not_given() {
        assert_eq!(filter_params("hn-whoishiring", &None), json!({"id": "hn-whoishiring"}));
        let filter = json!({"title": {"contains": "junior"}}).as_object().unwrap().clone();
        assert_eq!(
            filter_params("hn-whoishiring", &Some(filter)),
            json!({"id": "hn-whoishiring", "filter": {"title": {"contains": "junior"}}})
        );
    }

    #[test]
    fn set_value_reads_json_when_it_parses_and_otherwise_keeps_plain_text() {
        assert_eq!(set_value("50"), json!(50));
        assert_eq!(set_value("true"), json!(true));
        assert_eq!(set_value("active"), json!("active"));
        assert_eq!(set_value("https://example.test/f.xml"), json!("https://example.test/f.xml"));
    }

    #[test]
    fn source_params_sends_an_interval_as_a_number_or_a_preset_and_omits_absent_fields() {
        let minimal = SourceInput {
            id: "blog".into(),
            kind: None,
            label: None,
            interval: "daily".into(),
            enabled: false,
            params: Map::new(),
            filter: None,
        };
        assert_eq!(source_params(&minimal), json!({"id": "blog", "enabled": false, "interval": "daily", "params": {}}));

        let full = SourceInput {
            id: "blog".into(),
            kind: Some("rss".into()),
            label: Some("My Blog".into()),
            interval: "3600".into(),
            enabled: true,
            params: json!({"url": "https://example.test/f.xml"}).as_object().unwrap().clone(),
            filter: Some(json!({"title": {"contains": "junior"}}).as_object().unwrap().clone()),
        };
        assert_eq!(
            source_params(&full),
            json!({
                "id": "blog", "kind": "rss", "label": "My Blog", "enabled": true, "interval": 3600,
                "params": {"url": "https://example.test/f.xml"},
                "filter": {"title": {"contains": "junior"}},
            })
        );
    }
}
