//! Shell completion (#188). `shimmer completions bash|zsh|fish` prints a script to load once;
//! on Tab, the script runs `shimmer __complete WORDS…` (the words so far, the one being typed
//! last) and shows what this prints.
//!
//! The fixed words (commands, subcommands, options and the values an option lists) are read
//! from the help pages, the same `USAGE` text `--help` prints, so completion can't drift from
//! the commands. Your own names (workspaces, collections, records, templates…) come from ops the
//! daemon already answers. Completion never starts a daemon, and gives up on one that is slow:
//! without it, you still get the fixed words.
//!
//! What `__complete` prints: groups, each a `>TAG<tab>TITLE` line, then one `WORD<tab>DESCRIPTION`
//! line per candidate. The tag is zsh's name for the group (`commands`, `workspaces`), which the
//! zsh script colours by; the tag `files` says "complete file names here". A candidate ending
//! in `/` (`lc/`, a collection before its record) takes no space after it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::client::Client;
use crate::packs::pack::Pack;
use crate::style::{Palette, Role};

/// How long completion waits for the daemon, connecting and per call. Tab must feel instant.
const DAEMON_WAIT: Duration = Duration::from_millis(500);

/// The modules with their own help page, by the command word that opens it.
fn modules() -> [(&'static str, &'static str); 7] {
    [
        ("records", crate::records::USAGE),
        ("workspaces", crate::workspaces::USAGE),
        ("queue", crate::queue::USAGE),
        ("scheduler", crate::scheduler::USAGE),
        ("packs", crate::packs::cmd::USAGE),
        ("fetchers", crate::fetchers::USAGE),
        ("notify", crate::notify::USAGE),
    ]
}

// ---------------------------------------------------------------- reading the help pages

/// One command as its help page writes it.
#[derive(Clone, Debug, Default, PartialEq)]
struct Cmd {
    word: String,
    summary: String,
    /// Per position, what may go there: placeholders (`COLLECTION/ID`) or words (`none`).
    positionals: Vec<Vec<String>>,
    options: Vec<Opt>,
    /// `[--FIELD VALUE]…`: a records command that takes its collection's fields as options.
    field_flags: bool,
    /// `[list options]`: takes `list`'s options too (`records export`).
    list_options: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Opt {
    name: String,
    /// What follows it: placeholders (`TEMPLATE`), or nothing for a switch like `--wait`.
    args: Vec<String>,
    /// The values it lists itself: `--format csv|json`.
    values: Vec<String>,
}

/// A help summary as a one-line description: without a trailing example (`…, e.g.` continues on
/// the next line) or the punctuation that led into more text (#230).
fn tidy(summary: &str) -> String {
    let cut = summary.find(", e.g.").or_else(|| summary.strip_suffix(" e.g.").map(str::len)).unwrap_or(summary.len());
    summary[..cut].trim_end_matches([';', ':', ',', ' ']).to_owned()
}

/// Every command a help page lists, in its order. Entries are the lines two spaces in under a
/// title like `commands:` or `moving data:`; a command listed twice (`queue move`) is merged.
fn commands(usage: &str) -> Vec<Cmd> {
    let mut out: Vec<Cmd> = Vec::new();
    let mut in_section = false;
    let mut current: Option<usize> = None;
    for line in usage.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            in_section = is_section_title(line);
            current = None;
            continue;
        }
        if !in_section {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let text = line.trim_start();
        if indent == 2 && text.starts_with(|c: char| c.is_ascii_lowercase()) {
            let (synopsis, summary) = split_entry(text);
            let cmd = parse_synopsis(&synopsis, summary);
            match out.iter().position(|c| c.word == cmd.word) {
                Some(i) => {
                    merge(&mut out[i], cmd);
                    current = Some(i);
                }
                None => {
                    out.push(cmd);
                    current = Some(out.len() - 1);
                }
            }
        } else if indent >= 4 && text.starts_with('[') {
            // A continuation: more options for the command above, maybe with its description.
            if let Some(i) = current {
                let (more, said) = split_entry(text);
                let extra = parse_synopsis(&format!("{} {more}", out[i].word), String::new());
                merge(&mut out[i], extra);
                if out[i].summary.is_empty() {
                    out[i].summary = tidy(&said);
                }
            }
        } else if indent >= 4 {
            // A line of text under a command whose first line had no room for one (`export`).
            if let Some(i) = current.filter(|i| out[*i].summary.is_empty()) {
                out[i].summary = tidy(text);
            }
        }
    }
    out
}

/// `commands:`, `moving data:`, `changing a collection:`; not `WHEN is exactly one of:`.
fn is_section_title(line: &str) -> bool {
    line.strip_suffix(':').is_some_and(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_lowercase() || c == ' '))
}

/// An entry's synopsis and its summary. They are split by a column gap, or, when the synopsis
/// is too long for its column, by one space before the first plain word of the summary
/// (`reopen COLLECTION/ID [--clear-stamp] mark a done record todo again`).
fn split_entry(text: &str) -> (String, String) {
    let (left, right) = match text.find("  ") {
        Some(i) => (&text[..i], text[i..].trim()),
        None => (text, ""),
    };
    let tokens = tokenize(left);
    for i in 2..tokens.len() {
        if is_plain_word(&tokens[i]) {
            let synopsis = tokens[..i].join(" ");
            let summary = format!("{} {right}", tokens[i..].join(" "));
            return (synopsis, summary.trim().to_owned());
        }
    }
    (left.to_owned(), right.to_owned())
}

fn is_plain_word(token: &str) -> bool {
    token.starts_with(|c: char| c.is_ascii_lowercase()) && token.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// Words, keeping `[…]` groups and `"…"` quotes whole.
fn tokenize(text: &str) -> Vec<String> {
    let (mut out, mut word, mut depth, mut quoted) = (Vec::new(), String::new(), 0usize, false);
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            '[' if !quoted => depth += 1,
            ']' if !quoted => depth = depth.saturating_sub(1),
            c if c.is_whitespace() && depth == 0 && !quoted => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
                continue;
            }
            _ => {}
        }
        word.push(c);
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

fn parse_synopsis(synopsis: &str, summary: String) -> Cmd {
    let tokens = tokenize(synopsis);
    let mut cmd = Cmd { word: tokens.first().cloned().unwrap_or_default(), summary: tidy(&summary), ..Cmd::default() };
    let mut i = 1;
    while i < tokens.len() {
        let token = tokens[i].trim_end_matches('…');
        i += 1;
        if let Some(inner) = token.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            if inner == "list options" {
                cmd.list_options = true;
            } else if inner.starts_with('-') {
                let words = tokenize(inner);
                add_option(&mut cmd, &words[0], &words[1..]);
            } else {
                cmd.positionals.push(vec![inner.to_owned()]);
            }
        } else if token.starts_with("--") {
            // A required option takes the placeholder after it, if one follows.
            let takes = tokens.get(i).filter(|t| is_placeholder(t) || t.contains('|'));
            let args: Vec<String> = takes.into_iter().cloned().collect();
            i += args.len();
            add_option(&mut cmd, token, &args);
        } else if token != "…" {
            cmd.positionals.push(vec![token.to_owned()]);
        }
    }
    cmd
}

fn add_option(cmd: &mut Cmd, name: &str, args: &[String]) {
    if name == "--FIELD" {
        cmd.field_flags = true;
        return;
    }
    let mut opt = Opt { name: name.trim_end_matches(',').to_owned(), args: Vec::new(), values: Vec::new() };
    for a in args {
        let a = a.trim_matches('"');
        if a.contains('|') && a.chars().all(|c| c.is_ascii_lowercase() || c == '|' || c == '-' || c == '_') {
            opt.values = a.split('|').map(String::from).collect();
        } else {
            opt.args.extend(a.split_whitespace().map(String::from));
        }
    }
    if !cmd.options.iter().any(|o| o.name == opt.name) {
        cmd.options.push(opt);
    }
}

/// `COLLECTION`, `-FIELD`, `QUESTION=ANSWER`, `"EXPR"`: a stand-in, not a word to type.
fn is_placeholder(token: &str) -> bool {
    let t = token.trim_matches('"').trim_start_matches('-');
    t.starts_with(|c: char| c.is_ascii_uppercase())
}

fn merge(into: &mut Cmd, from: Cmd) {
    for (i, alternatives) in from.positionals.into_iter().enumerate() {
        match into.positionals.get_mut(i) {
            Some(existing) => {
                for a in alternatives {
                    if !existing.contains(&a) {
                        existing.push(a);
                    }
                }
            }
            None => into.positionals.push(alternatives),
        }
    }
    for o in from.options {
        if !into.options.iter().any(|x| x.name == o.name) {
            into.options.push(o);
        }
    }
    into.field_flags |= from.field_flags;
    into.list_options |= from.list_options;
}

/// The options a help page lists under `options:` (the top-level page: `--json`, `--pack`).
fn listed_options(usage: &str) -> Vec<Opt> {
    let mut out = Vec::new();
    let mut in_options = false;
    for line in usage.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() {
            in_options = line == "options:";
            continue;
        }
        let text = line.trim_start();
        if in_options && line.starts_with("  -") {
            let (synopsis, _) = split_entry(text);
            for (j, word) in tokenize(&synopsis).iter().enumerate() {
                if let Some(name) = word.trim_end_matches(',').strip_prefix('-') {
                    let rest: Vec<String> =
                        tokenize(&synopsis).into_iter().skip(j + 1).filter(|t| is_placeholder(t)).collect();
                    out.push(Opt { name: format!("-{name}"), args: rest, values: Vec::new() });
                }
            }
        }
    }
    out
}

/// The alternatives a help page gives for a placeholder: `WHEN is exactly one of:` followed
/// by `--every DURATION`, `--cron "EXPR"` and `--once TIME`.
fn alternatives(usage: &str, placeholder: &str) -> Vec<Opt> {
    let title = format!("{placeholder} is exactly one of:");
    let mut lines = usage.lines().skip_while(|l| *l != title).skip(1);
    let mut out = Vec::new();
    // Entries until the block ends (a blank or unindented line); an indented line that isn't an
    // option continues the one above (`--cron-local`'s second line), so it's skipped, not the end.
    for line in lines.by_ref().take_while(|l| l.starts_with("  ")) {
        if !line.starts_with("  -") {
            continue;
        }
        let (synopsis, _) = split_entry(line.trim_start());
        let words = tokenize(&synopsis);
        out.push(Opt { name: words[0].clone(), args: words[1..].to_vec(), values: Vec::new() });
    }
    out
}

/// The first words of `WHEN` as `shimmer scheduler add --help` lists them (`WHEN is one of:`), each
/// with what follows it: `daily` → `HH:MM`, `every` → `DURATION`, `cron` → `"EXPR"`. An example
/// list of days (`mon,wed,fri`) isn't a word to type, so it's left out.
fn when_words(usage: &str) -> Vec<(String, String)> {
    let lines = usage.lines().skip_while(|l| *l != "WHEN is one of:").skip(1);
    let mut out = Vec::new();
    for line in lines.take_while(|l| l.starts_with("  ")).filter(|l| !l.starts_with("   ")) {
        let synopsis = line.trim_start().split("  ").next().unwrap_or_default();
        for entry in synopsis.split(", ") {
            if let Some((word, rest)) = entry.split_once(' ') {
                if !word.contains(',') {
                    out.push((word.to_owned(), rest.to_owned()));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- what goes where

/// One suggestion.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub word: String,
    pub description: String,
}

/// Suggestions of one kind, shown together under `title`.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub tag: &'static str,
    pub title: &'static str,
    pub candidates: Vec<Candidate>,
}

/// Where the daemon's answers come from: the real daemon, or canned ones in tests.
#[allow(async_fn_in_trait)]
pub trait Source {
    async fn call(&mut self, op: &str, params: Value) -> Option<Value>;
}

/// What completion knows besides the line itself.
pub struct Context<'a> {
    pub pack: Option<&'a Pack>,
    /// Pack folders under `$SHIMMER_HOME/packs`, besides the built-in ones.
    pub pack_folders: Vec<String>,
}

/// The suggestions for the last of `words` (the one being typed; empty after a space). `words[0]`
/// is the program: `shimmer`, or a pack's alias run on its own.
pub async fn complete(words: &[String], cx: &Context<'_>, source: &mut impl Source) -> Vec<Group> {
    let Some((current, before)) = words.split_last() else { return Vec::new() };
    let mut line: Vec<String> = before.iter().skip(1).cloned().collect();
    // `ikuzo deep-<Tab>`: the alias stands for the whole command.
    if let Some(program) = before.first() {
        let name = Path::new(program).file_stem().and_then(|n| n.to_str()).unwrap_or_default();
        if cx.pack.is_some_and(|p| p.aliases.contains_key(name)) {
            line.insert(0, name.to_owned());
        }
    }
    let mut groups = Groups::default();
    groups.complete(&line, current, cx, source).await;
    groups.finish(current)
}

#[derive(Default)]
struct Groups(Vec<Group>);

impl Groups {
    fn add(&mut self, tag: &'static str, title: &'static str, word: impl Into<String>, description: impl Into<String>) {
        let candidate = Candidate { word: word.into(), description: description.into() };
        match self.0.iter_mut().find(|g| g.tag == tag) {
            Some(g) if !g.candidates.iter().any(|c| c.word == candidate.word) => g.candidates.push(candidate),
            Some(_) => {}
            None => self.0.push(Group { tag, title, candidates: vec![candidate] }),
        }
    }

    fn files(&mut self) {
        if !self.0.iter().any(|g| g.tag == "files") {
            self.0.push(Group { tag: "files", title: "files", candidates: Vec::new() });
        }
    }

    /// Only what starts with what's typed; no group left empty.
    fn finish(self, current: &str) -> Vec<Group> {
        self.0
            .into_iter()
            .filter_map(|mut g| {
                g.candidates.retain(|c| c.word.starts_with(current));
                (g.tag == "files" || !g.candidates.is_empty()).then_some(g)
            })
            .collect()
    }

    async fn complete(&mut self, line: &[String], current: &str, cx: &Context<'_>, source: &mut impl Source) {
        let top = commands(crate::args::USAGE);
        let globals = listed_options(crate::args::USAGE);
        // The words up to the command, and what's left after it.
        let mut i = 0;
        while i < line.len() && line[i].starts_with('-') {
            let takes = globals.iter().find(|o| o.name == line[i]).is_some_and(|o| !o.args.is_empty());
            i += if takes && !line[i].contains('=') { 2 } else { 1 };
        }
        if i > line.len() {
            // `--pack <Tab>` or `--socket <Tab>`.
            return self.value_of(line.last().map(String::as_str), cx);
        }
        let Some(first) = line.get(i) else {
            if current.starts_with('-') {
                for o in &globals {
                    self.add("options", "options", o.name.clone(), o.args.join(" "));
                }
                return;
            }
            for c in &top {
                self.add("commands", "commands", c.word.clone(), c.summary.clone());
            }
            if let Some(pack) = cx.pack {
                for (alias, target) in &pack.aliases {
                    self.add("aliases", "aliases", alias.clone(), format!("shimmer {}", target.words.join(" ")));
                }
            }
            return;
        };
        // An alias stands for its command's words.
        let mut rest: Vec<String> = line[i..].to_vec();
        if let Some(target) = cx.pack.and_then(|p| p.aliases.get(first)) {
            rest.splice(0..1, target.words.iter().map(|w| w.to_string()));
        }
        let first = rest[0].clone();
        if first == "scheduler" && rest.get(1).map(String::as_str) == Some("add") {
            return self.schedule(&rest[2..], current, cx, source).await;
        }
        let (cmd, usage, after) = match modules().into_iter().find(|(m, _)| *m == first) {
            Some((module, usage)) => {
                let subs = commands(usage);
                let Some(sub) = rest.get(1) else {
                    for c in &subs {
                        self.add("commands", "commands", c.word.clone(), c.summary.clone());
                    }
                    return;
                };
                let Some(cmd) = subs.iter().find(|c| &c.word == sub).cloned() else { return };
                let cmd = if cmd.list_options {
                    let mut list = subs.iter().find(|c| c.word == "list").cloned().unwrap_or_default();
                    // Paging is list's own: the commands that take its filters (records export,
                    // notify count, mark-read, mark-unread, remove) refuse --limit and --offset.
                    list.options.retain(|o| o.name != "--limit" && o.name != "--offset");
                    Cmd { field_flags: true, ..merged(cmd, list) }
                } else {
                    cmd
                };
                (Some((module, cmd)), usage, rest[2..].to_vec())
            }
            None => match top.iter().find(|c| c.word == first) {
                Some(cmd) => (Some(("", cmd.clone())), crate::args::USAGE, rest[1..].to_vec()),
                None => (None, "", Vec::new()),
            },
        };
        let Some((module, cmd)) = cmd else { return };
        self.arguments(module, &cmd, usage, &after, current, cx, source).await;
    }

    /// `shimmer scheduler add WHEN [--catch-up WHAT] [--lane LANE] COMMAND…` (#230, ADR 0031 §6): the ways to say
    /// when, then the schedule's options, then a command that can be scheduled, completed as it is
    /// on its own. Where the command starts is read as running it reads it (`parse_when`).
    async fn schedule(&mut self, after: &[String], current: &str, cx: &Context<'_>, source: &mut impl Source) {
        let usage = crate::schedule::USAGE;
        let Ok((_, used)) = crate::schedule::parse_when(after) else {
            // Still saying when: its first word, then nothing to offer for a time or a duration.
            if after.is_empty() {
                for (word, hint) in when_words(usage) {
                    self.add("when", "when", word, hint);
                }
            }
            return;
        };
        let synopsis = usage.lines().next().unwrap_or_default().trim_start_matches("usage: shimmer ");
        let own = parse_synopsis(synopsis, String::new());
        let mut i = used;
        while i < after.len() && after[i].starts_with("--") {
            if !after[i].contains('=') && i + 1 == after.len() {
                // `--catch-up <Tab>`, `--lane <Tab>`.
                let wants = own.options.iter().find(|o| o.name == after[i]).and_then(|o| o.args.first());
                if let Some(placeholder) = wants {
                    self.placeholder("scheduler", "", placeholder, None, current, cx, source).await;
                }
                return;
            }
            i += if after[i].contains('=') { 1 } else { 2 };
        }
        let schedulable = |words: &[&str]| {
            crate::schedule::SCHEDULABLE
                .iter()
                .any(|(group, subs)| words.len() == 2 && words[0] == *group && subs.contains(&words[1]))
        };
        let Some(word) = after.get(i) else {
            if current.starts_with('-') {
                for o in &own.options {
                    self.add("options", "options", o.name.clone(), o.args.join(" "));
                }
                return;
            }
            let top = commands(crate::args::USAGE);
            for (group, _) in crate::schedule::SCHEDULABLE {
                let summary = top.iter().find(|c| c.word == group).map(|c| c.summary.clone()).unwrap_or_default();
                self.add("commands", "commands", group, summary);
            }
            if let Some(pack) = cx.pack {
                for (alias, target) in pack.aliases.iter().filter(|(_, t)| schedulable(t.words)) {
                    self.add("aliases", "aliases", alias.clone(), format!("shimmer {}", target.words.join(" ")));
                }
            }
            return;
        };
        // The command, its alias written out as running it does (`rewrite_inner`).
        let mut command: Vec<String> = after[i..].to_vec();
        if let Some(target) = cx.pack.and_then(|p| p.aliases.get(word)) {
            command.splice(0..1, target.words.iter().map(|w| w.to_string()));
        }
        let Some((module, usage)) = modules().into_iter().find(|(m, _)| *m == command[0]) else { return };
        let subs = commands(usage);
        let Some(sub) = command.get(1) else {
            for c in subs.iter().filter(|c| schedulable(&[module, &c.word])) {
                self.add("commands", "commands", c.word.clone(), c.summary.clone());
            }
            return;
        };
        let Some(mut cmd) = subs.into_iter().find(|c| &c.word == sub && schedulable(&[module, &c.word])) else {
            return;
        };
        // A scheduled run has no one waiting and takes its turn: these are refused there.
        cmd.options.retain(|o| o.name != "--wait" && o.name != "--first");
        self.arguments(module, &cmd, usage, &command[2..], current, cx, source).await;
    }

    /// After a command: its options, the values they take, and what goes in each position.
    #[allow(clippy::too_many_arguments)]
    async fn arguments(
        &mut self,
        module: &str,
        cmd: &Cmd,
        usage: &str,
        after: &[String],
        current: &str,
        cx: &Context<'_>,
        source: &mut impl Source,
    ) {
        let globals = listed_options(crate::args::USAGE);
        let when = cmd.positionals.iter().flatten().find(|p| alternatives(usage, p).len() > 1);
        let when_options = when.map(|p| alternatives(usage, p)).unwrap_or_default();
        let mut options: Vec<Opt> = cmd.options.clone();
        options.extend(when_options.iter().cloned());
        options.extend(globals.iter().filter(|o| o.name == "--json" || o.name == "--socket").cloned());

        // Walk what's typed: positionals so far, and what the last option still wants.
        let mut positionals: Vec<String> = Vec::new();
        let mut wanting: Option<(Opt, usize)> = None;
        for word in after {
            if let Some((opt, n)) = wanting.take() {
                if n + 1 < opt.args.len() {
                    wanting = Some((opt, n + 1));
                }
                let _ = word;
                continue;
            }
            if word.starts_with("--") && !word.contains('=') {
                match options.iter().find(|o| &o.name == word) {
                    Some(o) if !o.args.is_empty() || !o.values.is_empty() => wanting = Some((o.clone(), 0)),
                    Some(_) => {}
                    // `--difficulty easy`: a field takes a value.
                    None if cmd.field_flags => {
                        wanting = Some((Opt { name: word.clone(), args: vec!["VALUE".into()], values: Vec::new() }, 0))
                    }
                    None => {}
                }
            } else if !word.starts_with('-') {
                positionals.push(word.clone());
            }
        }
        // The collection a records command is about, for its fields.
        let collection = (module == "records")
            .then(|| positionals.first().map(|p| p.split('/').next().unwrap_or_default().to_owned()))
            .flatten();

        if let Some((opt, n)) = wanting {
            if !opt.values.is_empty() {
                for v in &opt.values {
                    self.add("values", "values", v.clone(), "");
                }
                return;
            }
            let placeholder = opt.args.get(n).cloned().unwrap_or_default();
            if placeholder == "VALUE" && opt.args.len() == 1 {
                // A field's own value: its choices, if it's an enum.
                let field = opt.name.trim_start_matches('-').replace('-', "_");
                if let Some(fields) = self.fields(collection.as_deref(), source).await {
                    if let Some(f) = fields.iter().find(|f| f["name"] == field.as_str()) {
                        for v in f["values"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                            self.add("values", "values", v, "");
                        }
                    }
                }
                return;
            }
            return self.placeholder(module, &cmd.word, &placeholder, collection.as_deref(), current, cx, source).await;
        }
        let next = cmd.positionals.get(positionals.len());
        if current.starts_with('-') || next.is_none() {
            for o in &options {
                let hint = if o.values.is_empty() { o.args.join(" ") } else { o.values.join("|") };
                self.add("options", "options", o.name.clone(), hint);
            }
            if cmd.field_flags {
                for f in self.fields(collection.as_deref(), source).await.unwrap_or_default() {
                    let name = f["name"].as_str().unwrap_or_default().replace('_', "-");
                    self.add("fields", "fields", format!("--{name}"), field_type(&f));
                }
            }
            if current.starts_with('-') {
                return;
            }
        }
        for alternative in next.into_iter().flatten() {
            self.placeholder(module, &cmd.word, alternative, collection.as_deref(), current, cx, source).await;
        }
    }

    /// The candidates for one placeholder, e.g. `NAME` in `workspaces activate NAME`.
    #[allow(clippy::too_many_arguments)]
    async fn placeholder(
        &mut self,
        module: &str,
        command: &str,
        placeholder: &str,
        collection: Option<&str>,
        current: &str,
        cx: &Context<'_>,
        source: &mut impl Source,
    ) {
        if is_plain_word(placeholder) {
            // A word the command takes as it is: `packs use none`.
            return self.add("values", "values", placeholder, "");
        }
        match (module, placeholder.trim_start_matches('-')) {
            ("records", "COLLECTION") => self.collections(false, source).await,
            ("records", "COLLECTION/ID" | "COLLECTION[/ID]") => match current.split_once('/') {
                Some((c, _)) => self.records(c, source).await,
                None => self.collections(true, source).await,
            },
            // A collection by its id, except a new one's.
            ("records", "ID") if command != "new" && command != "add" => self.collections(false, source).await,
            ("records", "FIELD" | "FROM") => {
                for f in self.fields(collection, source).await.unwrap_or_default() {
                    self.add("fields", "fields", f["name"].as_str().unwrap_or_default(), field_type(&f));
                }
            }
            ("records" | "workspaces", "TEMPLATE") => self.templates(module, source).await,
            ("records" | "workspaces", "CATEGORY") => self.categories(module, source).await,
            ("workspaces", "NAME") if command != "new" => self.workspaces(source).await,
            ("packs", "NAME") | ("", "NAME") => self.packs(cx),
            ("packs", "NAME|FOLDER") => {
                self.packs(cx);
                self.files();
            }
            ("records", "FILE") | ("", "PATH") => self.files(),
            ("queue", "TASK" | "OTHER") => self.tasks(source).await,
            ("queue" | "scheduler", "LANE") => self.lanes(source).await,
            ("scheduler", "TRIGGER") => self.triggers(command, source).await,
            ("scheduler", "REMOVED") => self.removed_triggers(source).await,
            ("notify", "ID") => self.messages(source).await,
            ("scheduler" | "", "OP") => self.ops(source).await,
            ("scheduler", "WHAT") => {
                for v in crate::scheduler::CATCH_UP {
                    self.add("values", "values", *v, "");
                }
            }
            _ => {}
        }
    }

    fn value_of(&mut self, option: Option<&str>, cx: &Context<'_>) {
        match option {
            Some("--pack") => {
                self.packs(cx);
                self.add("values", "values", "none", "plain names, no pack");
            }
            Some("--socket") => self.files(),
            _ => {}
        }
    }

    async fn collections(&mut self, with_slash: bool, source: &mut impl Source) {
        let Some(data) = source.call("records.collections", json!({})).await else { return };
        for c in data["collections"].as_array().into_iter().flatten() {
            let id = c["id"].as_str().unwrap_or_default();
            let word = if with_slash { format!("{id}/") } else { id.to_owned() };
            self.add("collections", "collections", word, text(&c["label"]));
        }
    }

    async fn records(&mut self, collection: &str, source: &mut impl Source) {
        let params = json!({"collection": collection, "limit": 500});
        let Some(data) = source.call("records.list", params).await else { return };
        for r in data["items"].as_array().into_iter().flatten() {
            let id = r["id"].as_str().unwrap_or_default();
            let title = data["titles"][id].as_str().map(String::from).unwrap_or_else(|| text(&r["title"]));
            let description = match r["status"].as_str() {
                Some(status) if title.is_empty() => status.to_owned(),
                Some(status) => format!("{title} ({status})"),
                None => title,
            };
            self.add("records", "records", format!("{collection}/{id}"), description);
        }
    }

    async fn fields(&mut self, collection: Option<&str>, source: &mut impl Source) -> Option<Vec<Value>> {
        let data = source.call("records.collections", json!({})).await?;
        let c = data["collections"].as_array()?.iter().find(|c| c["id"].as_str() == collection)?;
        c["fields"].as_array().cloned()
    }

    async fn templates(&mut self, module: &str, source: &mut impl Source) {
        let Some(data) = source.call(&format!("{module}.templates"), json!({})).await else { return };
        for t in data["templates"].as_array().into_iter().flatten() {
            self.add("templates", "templates", text(&t["id"]), shorten(&text(&t["description"])));
        }
    }

    async fn categories(&mut self, module: &str, source: &mut impl Source) {
        let Some(data) = source.call(&format!("{module}.templates"), json!({})).await else { return };
        for c in data["categories"].as_array().into_iter().flatten() {
            self.add("categories", "categories", text(&c["id"]), text(&c["label"]));
        }
    }

    async fn workspaces(&mut self, source: &mut impl Source) {
        let Some(data) = source.call("workspaces.list", json!({})).await else { return };
        for w in data["workspaces"].as_array().into_iter().flatten() {
            let description = format!("{} ({})", text(&w["label"]), text(&w["state"]));
            self.add("workspaces", "workspaces", text(&w["id"]), description.trim_start());
        }
    }

    async fn tasks(&mut self, source: &mut impl Source) {
        let Some(data) = source.call("queue.list", json!({})).await else { return };
        for lane in data["lanes"].as_array().into_iter().flatten() {
            for t in ["running", "queued"].iter().flat_map(|k| lane[*k].as_array().into_iter().flatten()) {
                let description = format!("{} ({})", text(&t["op"]), text(&t["status"]));
                self.add("tasks", "tasks", text(&t["task_id"]), description);
            }
        }
    }

    async fn lanes(&mut self, source: &mut impl Source) {
        let Some(data) = source.call("core.manifest", json!({})).await else { return };
        for l in data["lanes"].as_array().into_iter().flatten() {
            self.add("lanes", "lanes", text(&l["id"]), format!("{} at a time", text(&l["max_concurrent"])));
        }
    }

    /// Inbox messages by the short ending `notify list` shows, newest first.
    async fn messages(&mut self, source: &mut impl Source) {
        let Some(data) = source.call("notify.list", json!({"limit": 200})).await else { return };
        let all = data["items"].as_array().cloned().unwrap_or_default();
        let ids: Vec<String> = all.iter().map(|m| text(&m["id"])).collect();
        for (m, short) in all.iter().zip(crate::render::short_ids(&ids)) {
            self.add("messages", "messages", short, text(&m["title"]));
        }
    }

    async fn triggers(&mut self, command: &str, source: &mut impl Source) {
        let Some(data) = source.call("scheduler.list", json!({})).await else { return };
        let all: Vec<Value> = data["triggers"]
            .as_array()
            .or_else(|| data.as_array())
            .into_iter()
            .flatten()
            // The ones you added, as `scheduler list` shows (#244), for anything you can only do to
            // your own. Pausing is the one thing you can do to a module's schedule, so pause and
            // resume offer every one (#249's review).
            .filter(|t| matches!(command, "disable" | "enable") || t["source"]["type"] == "user")
            .cloned()
            .collect();
        // The same short endings `scheduler list` shows, which every scheduler command accepts.
        let ids: Vec<String> = all.iter().map(|t| text(&t["id"])).collect();
        for (t, short) in all.iter().zip(crate::render::short_ids(&ids)) {
            self.add("triggers", "triggers", short, text(&t["op"]));
        }
    }

    /// Removed schedules, for `scheduler restore` and `purge` (ADR 0038).
    async fn removed_triggers(&mut self, source: &mut impl Source) {
        let Some(data) = source.call("scheduler.list", json!({"removed": true})).await else { return };
        // Only a person's own schedule is ever removed (a module's can only be disabled).
        let all: Vec<Value> = data["triggers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["source"]["type"] == "user")
            .cloned()
            .collect();
        let ids: Vec<String> = all.iter().map(|t| text(&t["id"])).collect();
        for (t, short) in all.iter().zip(crate::render::short_ids(&ids)) {
            self.add("triggers", "triggers", short, text(&t["op"]));
        }
    }

    async fn ops(&mut self, source: &mut impl Source) {
        let Some(data) = source.call("core.manifest", json!({})).await else { return };
        for m in data["modules"].as_array().into_iter().flatten() {
            for c in m["commands"].as_array().into_iter().flatten() {
                self.add("ops", "ops", text(&c["op"]), text(&c["summary"]));
            }
        }
    }

    fn packs(&mut self, cx: &Context<'_>) {
        for (id, _) in crate::packs::builtin::BUILTIN {
            let label = crate::packs::builtin::builtin(id).map(|p| p.label).unwrap_or_default();
            self.add("packs", "packs", *id, label);
        }
        for id in &cx.pack_folders {
            self.add("packs", "packs", id.clone(), "yours");
        }
    }
}

fn merged(mut a: Cmd, b: Cmd) -> Cmd {
    for o in b.options {
        if !a.options.iter().any(|x| x.name == o.name) {
            a.options.push(o);
        }
    }
    a
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn shorten(s: &str) -> String {
    const MAX: usize = 60;
    if s.chars().count() > MAX {
        s.chars().take(MAX - 1).chain(['…']).collect()
    } else {
        s.to_owned()
    }
}

/// `enum: easy|medium|hard`, `date`, `text`.
fn field_type(f: &Value) -> String {
    match f["values"].as_array() {
        Some(values) => values.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("|"),
        None => match f["type"].as_str() {
            Some("string") | None => "text".into(),
            Some(t) => t.to_owned(),
        },
    }
}

/// The groups as `__complete` prints them (see the module docs).
pub fn print(groups: &[Group]) -> String {
    let mut out = String::new();
    for g in groups {
        out.push_str(&format!(">{}\t{}\n", g.tag, g.title));
        for c in &g.candidates {
            let description = c.description.replace(['\t', '\n'], " ");
            out.push_str(&format!("{}\t{description}\n", c.word));
        }
    }
    out
}

// ---------------------------------------------------------------- the daemon, briefly

/// The real daemon, if one is already listening. Never started for completion; one that's slow
/// is given up on, and later calls don't wait again.
pub struct Daemon {
    socket: PathBuf,
    client: Option<Client>,
    tried: bool,
    cache: BTreeMap<String, Option<Value>>,
}

impl Daemon {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket, client: None, tried: false, cache: BTreeMap::new() }
    }
}

impl Source for Daemon {
    async fn call(&mut self, op: &str, params: Value) -> Option<Value> {
        let key = format!("{op} {params}");
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }
        if !self.tried {
            self.tried = true;
            self.client = tokio::time::timeout(DAEMON_WAIT, Client::connect(&self.socket)).await.ok()?.ok();
        }
        let client = self.client.as_mut()?;
        let answer = match tokio::time::timeout(DAEMON_WAIT, client.call(op, params)).await {
            Ok(Ok(data)) => Some(data),
            Ok(Err(_)) => None,
            Err(_) => {
                self.client = None;
                None
            }
        };
        self.cache.insert(key, answer.clone());
        answer
    }
}

/// The pack folders in `home/packs`, by name.
pub fn pack_folders(home: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(home.join("packs")) else { return Vec::new() };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------- the shell scripts

const BASH: &str = include_str!("complete/shimmer.bash");
const ZSH: &str = include_str!("complete/shimmer.zsh");
const FISH: &str = include_str!("complete/shimmer.fish");

/// The script for `shell`, calling `exe` back on Tab. Aliases of the active pack complete too;
/// in zsh, the suggestions are coloured with `palette` (#189).
pub fn script(shell: &str, exe: &Path, pack: Option<&Pack>, palette: &Palette) -> Result<String, String> {
    let exe = quote(&exe.display().to_string());
    let aliases: Vec<&str> = pack.map(|p| p.aliases.keys().map(String::as_str).collect()).unwrap_or_default();
    let names = std::iter::once("shimmer").chain(aliases.iter().copied()).collect::<Vec<_>>().join(" ");
    let text = match shell {
        "bash" => BASH.to_owned(),
        "fish" => FISH.to_owned(),
        "zsh" => ZSH
            .replace("@PATTERN@", &format!("({})", names.replace(' ', "|")))
            .replace("@NAME_CODE@", &palette.code(Role::Name))
            .replace("@HINT_CODE@", &palette.code(Role::Hint))
            .replace("@HEADING_CODE@", &palette.code(Role::Heading)),
        other => return Err(format!("'{other}' isn't a shell completion is made for: use bash, zsh or fish")),
    };
    Ok(text.replace("@EXE@", &exe).replace("@NAMES@", &names))
}

/// `s` in single quotes, for any of the three shells.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canned daemon answers, by op.
    struct Canned(BTreeMap<&'static str, Value>);

    impl Source for Canned {
        async fn call(&mut self, op: &str, _params: Value) -> Option<Value> {
            self.0.get(op).cloned()
        }
    }

    fn daemon() -> Canned {
        Canned(BTreeMap::from([
            (
                "records.collections",
                json!({"collections": [{"id": "lc", "label": "LeetCode", "fields": [
                    {"name": "title", "type": "string"},
                    {"name": "difficulty", "type": "enum", "values": ["easy", "medium", "hard"]},
                    {"name": "time_min", "type": "int"}]}]}),
            ),
            ("records.list", json!({"items": [{"id": "two-sum", "status": "done", "title": "Two Sum"}], "titles": {}})),
            (
                "records.templates",
                json!({"categories": [{"id": "practice", "label": "Practice"}],
                       "templates": [{"id": "leetcode", "description": "Problems"}]}),
            ),
            ("workspaces.list", json!({"workspaces": [{"id": "site", "label": "Site", "state": "ready"}]})),
            (
                "scheduler.list",
                json!({"triggers": [
                    {"id": "usr-01JABCDEFGHJKMNPQRSTVWXYZ0", "op": "records.list", "source": {"type": "user"}},
                    {"id": "notify.flush", "op": "notify.flush", "source": {"type": "module", "id": "notify"}}]}),
            ),
            (
                "core.manifest",
                json!({"lanes": [{"id": "default", "max_concurrent": 4}],
                       "modules": [{"commands": [{"op": "core.ping", "summary": "Ping"}]}]}),
            ),
        ]))
    }

    /// The words suggested for `line`, typed after `shimmer` (a trailing space starts a new word).
    async fn words_with(line: &str, source: &mut impl Source) -> Vec<String> {
        let mut words: Vec<String> = std::iter::once("shimmer").chain(line.split(' ')).map(String::from).collect();
        if line.is_empty() {
            words.truncate(1);
            words.push(String::new());
        }
        let cx = Context { pack: None, pack_folders: Vec::new() };
        complete(&words, &cx, source).await.into_iter().flat_map(|g| g.candidates).map(|c| c.word).collect()
    }

    async fn words(line: &str) -> Vec<String> {
        words_with(line, &mut daemon()).await
    }

    #[tokio::test]
    async fn every_command_with_more_to_type_completes_after_its_name() {
        // A command written with `…` in `shimmer --help` (`records …`, `scheduler …`)
        // takes more words, so Tab after it should offer some. A new group (fetchers, notify,
        // calendar) only does once modules() reads its help page; this says so instead of it
        // being forgotten, whatever shape its help line has (#230: `shimmer schedule` slipped past a
        // check that only knew `NAME …`).
        let groups: Vec<&str> = crate::args::USAGE
            .lines()
            .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
            .filter(|l| l.split("  ").find(|p| !p.is_empty()).is_some_and(|synopsis| synopsis.contains('…')))
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        assert!(groups.contains(&"records") && groups.contains(&"scheduler"), "groups not found: {groups:?}");
        for group in groups {
            assert!(
                !words(&format!("{group} ")).await.is_empty(),
                "`shimmer {group} <Tab>` offers nothing. If it's a group of subcommands, add \
                 (\"{group}\", <its help's USAGE constant>) to complete.rs's modules(), as the others are; \
                 if it reads its words its own way, as `scheduler add` does, give it its own case in complete()."
            );
        }
    }

    #[test]
    fn every_command_in_the_help_pages_is_read() {
        let top: Vec<String> = commands(crate::args::USAGE).into_iter().map(|c| c.word).collect();
        for word in ["ping", "records", "workspaces", "queue", "scheduler", "packs", "call", "shutdown"] {
            assert!(top.contains(&word.to_owned()), "{word} missing from {top:?}");
        }
        // Each module's commands, as the pack targets know them: the same list, read twice.
        for (module, usage) in modules().into_iter().filter(|(m, _)| *m != "packs") {
            let read: Vec<String> = commands(usage).into_iter().map(|c| c.word).collect();
            let targets: std::collections::BTreeSet<&str> = crate::packs::pack::TARGETS
                .iter()
                .filter(|t| t.words.len() == 2 && t.words[0] == module)
                .map(|t| t.words[1])
                .collect();
            let read_set: std::collections::BTreeSet<&str> = read.iter().map(String::as_str).collect();
            assert_eq!(read_set, targets, "{module}: the help and the pack targets list different commands");
        }
        let packs: Vec<String> = commands(crate::packs::cmd::USAGE).into_iter().map(|c| c.word).collect();
        assert_eq!(packs, ["list", "show", "use", "link", "unlink", "check"]);
    }

    #[test]
    fn a_help_entry_gives_its_positions_and_options() {
        let records = commands(crate::records::USAGE);
        let new = records.iter().find(|c| c.word == "new").unwrap();
        assert_eq!(new.positionals, [vec!["ID".to_owned()]]);
        assert!(new.options.iter().any(|o| o.name == "--from" && o.args == ["TEMPLATE"]));
        assert!(new.options.iter().any(|o| o.name == "--label"));
        assert_eq!(new.summary, "create a collection from a template", "cut before its example (#230)");
        // #230: a command whose first line has no room for a description takes the next line's.
        let summary = |word: &str| records.iter().find(|c| c.word == word).unwrap().summary.clone();
        assert_eq!(summary("add"), "add a record");
        assert_eq!(summary("update"), "change a record's fields");
        assert_eq!(summary("complete"), "mark done and stamp the collection's date field");
        assert_eq!(summary("export"), "write records to stdout; imports back unchanged");
        let list = records.iter().find(|c| c.word == "list").unwrap();
        assert!(list.field_flags);
        let status = list.options.iter().find(|o| o.name == "--status").unwrap();
        assert_eq!(status.values, ["todo", "done"]);
        assert!(list.options.iter().any(|o| o.name == "--sort"), "from a continuation line");
        let export = records.iter().find(|c| c.word == "export").unwrap();
        assert!(export.list_options);
        let peek = commands(crate::workspaces::USAGE).into_iter().find(|c| c.word == "peek").unwrap();
        assert!(peek.options.iter().any(|o| o.name == "--scripts"));
        let moves = commands(crate::queue::USAGE).into_iter().find(|c| c.word == "move").unwrap();
        assert!(moves.options.iter().any(|o| o.name == "--before") && moves.options.iter().any(|o| o.name == "--last"));
        let uses = commands(crate::packs::cmd::USAGE).into_iter().find(|c| c.word == "use").unwrap();
        assert_eq!(uses.positionals, [vec!["NAME".to_owned(), "none".to_owned()]]);
        // A placeholder's alternatives, as a help page lists them; one whose summary runs onto a
        // second line doesn't end the list.
        let usage = "usage: x\n\nWHEN is exactly one of:\n  --every DURATION   e.g. 2h\n  --cron-local \"EXPR\" local time:\n                     9am stays 9am\n  --once TIME        a time\n";
        let when: Vec<String> = alternatives(usage, "WHEN").into_iter().map(|o| o.name).collect();
        assert_eq!(when, ["--every", "--cron-local", "--once"]);
    }

    #[tokio::test]
    async fn commands_and_subcommands() {
        assert!(words("").await.contains(&"workspaces".to_owned()));
        assert_eq!(words("wor").await, ["workspaces"]);
        assert_eq!(words("workspaces ac").await, ["activate"]);
        assert_eq!(
            words("records re").await,
            [
                "reopen",
                "rename",
                "remove",
                "restore",
                "rename-field",
                "rename-collection",
                "remove-collection",
                "restore-collection"
            ]
        );
        assert_eq!(words("--js").await, ["--json"]);
        assert_eq!(words("--json wor").await, ["workspaces"], "a global option before the command");
    }

    #[test]
    fn the_ways_to_say_when_are_read_from_the_schedule_help() {
        let when: Vec<String> = when_words(crate::schedule::USAGE).into_iter().map(|(w, _)| w).collect();
        assert_eq!(when, ["daily", "weekdays", "weekends", "every", "once", "cron", "cron-local"]);
        assert!(when_words(crate::schedule::USAGE).contains(&("every".into(), "DURATION".into())));
    }

    #[tokio::test]
    async fn a_schedule_completes_when_then_its_options_then_the_command() {
        assert_eq!(
            words("scheduler add ").await,
            ["daily", "weekdays", "weekends", "every", "once", "cron", "cron-local"]
        );
        assert_eq!(words("scheduler add w").await, ["weekdays", "weekends"]);
        assert!(words("scheduler add daily ").await.is_empty(), "a time is typed, not picked");
        assert!(words("scheduler add once 2026-10-20 ").await.is_empty(), "the time still to come");
        assert_eq!(
            words("scheduler add daily 09:00 ").await,
            ["records", "workspaces", "notify"],
            "only what can be scheduled"
        );
        assert_eq!(words("scheduler add daily 09:00 --").await, ["--catch-up", "--lane"]);
        assert_eq!(words("scheduler add every 1d --catch-up ").await, ["skip", "run-once", "backfill"]);
        assert_eq!(words("scheduler add every 1d --lane ").await, ["default"]);
        assert_eq!(
            words("scheduler add every 1d --catch-up skip --lane default wor").await,
            ["workspaces"],
            "the options' values aren't taken for the command"
        );
        assert_eq!(words("scheduler add daily 09:00 workspaces ").await, ["activate", "cleanup", "stop"]);
        assert_eq!(words("scheduler add daily 09:00 records re").await, ["reopen", "remove", "restore"]);
        assert_eq!(words("scheduler add daily 09:00 workspaces activate ").await, ["site"]);
        assert_eq!(words("scheduler add daily 09:00 records complete lc/").await, ["lc/two-sum"]);
        let options = words("scheduler add daily 09:00 workspaces activate site --").await;
        assert!(!options.contains(&"--wait".to_owned()) && !options.contains(&"--first".to_owned()), "{options:?}");
        assert!(words("scheduler add daily 09:00 records list ").await.is_empty(), "list can't be scheduled");
    }

    #[tokio::test]
    async fn a_schedule_offers_and_follows_the_packs_aliases() {
        let pack = crate::packs::builtin::builtin("short").unwrap();
        let cx = Context { pack: Some(&pack), pack_folders: Vec::new() };
        let at = |line: &str| -> Vec<String> {
            std::iter::once("shimmer").chain(line.split(' ')).map(String::from).collect()
        };
        let got = |groups: Vec<Group>| -> Vec<String> {
            groups.into_iter().flat_map(|g| g.candidates).map(|c| c.word).collect()
        };
        let offered = got(complete(&at("scheduler add daily 09:00 "), &cx, &mut daemon()).await);
        assert!(offered.contains(&"wgo".to_owned()), "workspaces activate's alias: {offered:?}");
        assert!(!offered.contains(&"wshow".to_owned()), "workspaces show can't be scheduled");
        assert_eq!(got(complete(&at("scheduler add daily 09:00 wgo "), &cx, &mut daemon()).await), ["site"]);
    }

    #[tokio::test]
    async fn commands_that_take_lists_filters_dont_offer_its_paging() {
        for line in ["records export lc --", "notify count --", "notify mark-read --", "notify remove --"] {
            let offered = words(line).await;
            assert!(
                !offered.contains(&"--limit".to_owned()) && !offered.contains(&"--offset".to_owned()),
                "{line}: {offered:?}"
            );
        }
        assert!(words("notify count --").await.contains(&"--unread".to_owned()), "the filters still come");
        assert!(words("records export lc --").await.contains(&"--status".to_owned()));
        assert!(words("notify list --").await.contains(&"--limit".to_owned()), "list keeps its own");
    }

    #[tokio::test]
    async fn your_own_names() {
        assert_eq!(words("workspaces activate ").await, ["site"]);
        assert_eq!(words("records list ").await, ["lc"]);
        assert_eq!(words("records complete ").await, ["lc/"], "a collection first, then its records");
        assert_eq!(words("records complete lc/").await, ["lc/two-sum"]);
        assert_eq!(words("records new jobs --from ").await, ["leetcode"]);
        assert_eq!(words("records templates ").await, ["practice"]);
        assert_eq!(words("scheduler remove ").await, ["VWXYZ0"], "the short ending lists show, not the whole id");
        assert_eq!(words("scheduler restore ").await, ["VWXYZ0"], "from scheduler list --removed");
        let pause = words("scheduler disable ").await;
        assert!(pause.contains(&"VWXYZ0".to_owned()) && pause.contains(&"notify.flush".to_owned()), "{pause:?}");
        assert_eq!(words("call ").await, ["core.ping"]);
        assert!(words("packs use ").await.contains(&"none".to_owned()));
        assert!(words("packs use ").await.contains(&"short".to_owned()));
        assert!(words("--pack ").await.contains(&"starship".to_owned()));
    }

    #[tokio::test]
    async fn options_fields_and_values() {
        let list = words("records list lc --").await;
        assert!(list.contains(&"--status".to_owned()) && list.contains(&"--difficulty".to_owned()), "{list:?}");
        assert!(list.contains(&"--time-min".to_owned()), "a field's flag has hyphens");
        assert_eq!(words("records list lc --status ").await, ["todo", "done"]);
        assert_eq!(words("records list lc --difficulty ").await, ["easy", "medium", "hard"]);
        assert_eq!(words("records list lc --sort ").await, ["title", "difficulty", "time_min"]);
        assert_eq!(words("records export lc --format ").await, ["csv", "json"]);
        assert!(words("records export lc --").await.contains(&"--status".to_owned()), "list's options too");
        assert_eq!(words("scheduler add every 1d --catch-up ").await, ["skip", "run-once", "backfill"]);
        assert!(words("scheduler add every 1d --").await.contains(&"--catch-up".to_owned()));
        assert_eq!(words("workspaces activate site --w").await, ["--wait"]);
        assert_eq!(words("queue list --lane ").await, ["default"]);
    }

    #[tokio::test]
    async fn without_a_daemon_the_fixed_words_still_come() {
        let mut nothing = Canned(BTreeMap::new());
        assert_eq!(words_with("workspaces ac", &mut nothing).await, ["activate"]);
        assert!(words_with("workspaces activate ", &mut nothing).await.is_empty());
        assert_eq!(words_with("records list x --status ", &mut nothing).await, ["todo", "done"]);
    }

    #[tokio::test]
    async fn files_where_a_file_goes() {
        let cx = Context { pack: None, pack_folders: Vec::new() };
        let line: Vec<String> = ["shimmer", "records", "import", "lc", ""].map(String::from).to_vec();
        let groups = complete(&line, &cx, &mut daemon()).await;
        assert!(groups.iter().any(|g| g.tag == "files"), "{groups:?}");
    }

    #[tokio::test]
    async fn a_packs_aliases_complete_like_their_commands() {
        let pack = crate::packs::builtin::builtin("anime-tropes").unwrap();
        let (alias, _) = pack.aliases.iter().find(|(_, t)| t.op == "workspaces.activate").unwrap();
        let cx = Context { pack: Some(&pack), pack_folders: Vec::new() };
        let mut d = daemon();
        // `shimmer ikuzo <Tab>` and `ikuzo <Tab>`, linked on its own.
        for line in [vec!["shimmer", alias.as_str(), ""], vec![alias.as_str(), ""]] {
            let line: Vec<String> = line.into_iter().map(String::from).collect();
            let got: Vec<String> =
                complete(&line, &cx, &mut d).await.into_iter().flat_map(|g| g.candidates).map(|c| c.word).collect();
            assert_eq!(got, ["site"], "{line:?}");
        }
        let line: Vec<String> = ["shimmer", ""].map(String::from).to_vec();
        let groups = complete(&line, &cx, &mut d).await;
        assert!(groups.iter().any(|g| g.tag == "aliases" && g.candidates.iter().any(|c| &c.word == alias)));
    }

    #[test]
    fn completions_is_a_command_word_no_pack_may_take() {
        // #193's review: like every command word (ADR 0013), so `shimmer completions` can't be shadowed.
        let toml = "[pack]\nid = \"p\"\nlabel = \"P\"\n[alias]\n\"completions\" = \"core.ping\"\n";
        let problems = crate::packs::pack::parse("p", toml).unwrap_err();
        assert!(problems.iter().any(|p| p.contains("completions")), "{problems:?}");
    }

    #[test]
    fn printed_for_the_scripts() {
        let groups = vec![
            Group {
                tag: "commands",
                title: "commands",
                candidates: vec![Candidate { word: "ping".into(), description: "check\tit".into() }],
            },
            Group { tag: "files", title: "files", candidates: Vec::new() },
        ];
        assert_eq!(print(&groups), ">commands\tcommands\nping\tcheck it\n>files\tfiles\n");
    }

    #[test]
    fn each_shell_gets_its_script() {
        let palette = Palette::new(true, &BTreeMap::new());
        let pack = crate::packs::builtin::builtin("short").unwrap();
        for shell in ["bash", "zsh", "fish"] {
            let s = script(shell, Path::new("/opt/it's/shimmer"), Some(&pack), &palette).unwrap();
            assert!(s.contains(r"'/opt/it'\''s/shimmer' __complete"), "{shell}: calls back the binary that made it");
            assert!(s.contains("rdone"), "{shell}: the pack's aliases complete too");
            let unfilled = s
                .split('@')
                .skip(1)
                .step_by(2)
                .find(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
            assert_eq!(unfilled, None, "{shell}: every placeholder filled");
        }
        let zsh = script("zsh", Path::new("shimmer"), None, &palette).unwrap();
        assert!(zsh.contains("38;2;162;148;255"), "zsh: the palette");
        assert!(!zsh.contains("region_highlight"), "zsh: the line isn't coloured as you type");
        assert!(script("tcsh", Path::new("shimmer"), None, &palette).unwrap_err().contains("bash, zsh or fish"));
    }
}
