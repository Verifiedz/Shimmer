//! Colour in the Shimmer palette (#189), read from the logo. Each colour has a meaning (a role),
//! never just decoration, and every coloured line keeps its words and marks (✓, ✗, `dirty`), so
//! nothing depends on colour alone.
//!
//! Colour is decided once per run: only on a terminal, never with `--json`, never when
//! `NO_COLOR` is set (no-color.org) or `--no-color` is given. Until [`init`] turns it on,
//! everything here returns text unchanged, so tests and pipes see plain output.

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// What a colour means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `✓ site is active`
    Success,
    /// `✗ failed`, `dirty`, the `shimmer:` before an error
    Error,
    /// Headings and table headers.
    Heading,
    /// Names and ids you can type: a table's first column.
    Name,
    /// Things left open, warnings.
    Warning,
    /// Hints and secondary information.
    Hint,
}

impl Role {
    pub const ALL: [Role; 6] = [Role::Success, Role::Error, Role::Heading, Role::Name, Role::Warning, Role::Hint];

    /// Its name in a pack's `[colors]`.
    pub fn key(self) -> &'static str {
        match self {
            Role::Success => "success",
            Role::Error => "error",
            Role::Heading => "heading",
            Role::Name => "name",
            Role::Warning => "warning",
            Role::Hint => "hint",
        }
    }

    /// The logo's colours, two deepened so text stays readable on light backgrounds too.
    fn default_rgb(self) -> (u8, u8, u8) {
        match self {
            Role::Success => (0x3f, 0xbf, 0x9b), // the band's mint, deepened
            Role::Error => (0xdb, 0x00, 0x7c),   // the cork
            Role::Heading => (0x72, 0x5e, 0xff), // the liquid, deep
            Role::Name => (0xa2, 0x94, 0xff),    // the liquid, mid
            Role::Warning => (0xd9, 0xa4, 0x41), // the band's cream, deepened to amber
            Role::Hint => (0x8a, 0x86, 0x9e),    // muted grey
        }
    }

    /// The nearest of the 16 basic terminal colours, where true colour isn't available.
    fn basic(self) -> &'static str {
        match self {
            Role::Success => "32",
            Role::Error => "35",
            Role::Heading => "1;34",
            Role::Name => "36",
            Role::Warning => "33",
            Role::Hint => "90",
        }
    }
}

/// Colours for one run: the roles (a pack may override some) and whether the terminal takes
/// 24-bit colour.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    rgb: [(u8, u8, u8); 6],
    truecolor: bool,
}

impl Palette {
    pub fn new(truecolor: bool, overrides: &BTreeMap<String, (u8, u8, u8)>) -> Self {
        let rgb = Role::ALL.map(|r| overrides.get(r.key()).copied().unwrap_or_else(|| r.default_rgb()));
        Self { rgb, truecolor }
    }

    fn start(&self, role: Role) -> String {
        format!("\x1b[{}m", self.code(role))
    }

    fn rgb(&self, role: Role) -> (u8, u8, u8) {
        self.rgb[Role::ALL.iter().position(|x| *x == role).expect("every role is in ALL")]
    }

    /// `role`'s SGR code, without the escape around it: `1;38;2;114;94;255`, or `1;34` in 16
    /// colours. zsh's `list-colors` takes these (#188).
    pub fn code(&self, role: Role) -> String {
        if !self.truecolor {
            return role.basic().to_owned();
        }
        let (r, g, b) = self.rgb(role);
        let bold = if role == Role::Heading { "1;" } else { "" };
        format!("{bold}38;2;{r};{g};{b}")
    }

    /// `role` as zsh writes a highlight: `fg=#a294ff`, `bold,fg=#725eff`, or a basic colour's
    /// name in 16 colours. zsh's `region_highlight` takes these (#188).
    pub fn zsh(&self, role: Role) -> String {
        let bold = if role == Role::Heading { "bold," } else { "" };
        if !self.truecolor {
            let name = match role {
                Role::Success => "green",
                Role::Error => "magenta",
                Role::Heading => "blue",
                Role::Name => "cyan",
                Role::Warning => "yellow",
                Role::Hint => "8",
            };
            return format!("{bold}fg={name}");
        }
        let (r, g, b) = self.rgb(role);
        format!("{bold}fg=#{r:02x}{g:02x}{b:02x}")
    }
}

const RESET: &str = "\x1b[0m";

/// Colour for standard output and for standard error, each `None` when it isn't a terminal.
static OUT: OnceLock<Option<Palette>> = OnceLock::new();
static ERR: OnceLock<Option<Palette>> = OnceLock::new();

/// Should a stream be coloured? `tty`: is it a terminal; `no_color_env`: `NO_COLOR`'s value
/// (any non-empty value turns colour off); `flag`: `--no-color`; `json`: `--json`.
pub fn should_color(tty: bool, no_color_env: Option<&str>, flag: bool, json: bool) -> bool {
    tty && !flag && !json && no_color_env.is_none_or(str::is_empty)
}

/// Decide colour for this run, once. `overrides` are the active pack's `[colors]`.
pub fn init(no_color_flag: bool, json: bool, overrides: &BTreeMap<String, (u8, u8, u8)>) {
    use std::io::IsTerminal;
    let env = std::env::var("NO_COLOR").ok();
    let truecolor = std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit");
    let palette =
        |tty: bool| should_color(tty, env.as_deref(), no_color_flag, json).then(|| Palette::new(truecolor, overrides));
    let _ = OUT.set(palette(std::io::stdout().is_terminal()));
    let _ = ERR.set(palette(std::io::stderr().is_terminal()));
}

/// The palette for a shell script that colours later, at the terminal (#188): never off, since
/// the script itself checks `NO_COLOR` when it runs. True colour as `COLORTERM` says now.
pub fn for_script(overrides: &BTreeMap<String, (u8, u8, u8)>) -> Palette {
    let truecolor = std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit");
    Palette::new(truecolor, overrides)
}

fn out() -> Option<&'static Palette> {
    OUT.get().and_then(Option::as_ref)
}

/// `text` in `role`'s colour, for standard output.
pub fn paint(role: Role, text: &str) -> String {
    paint_with(out(), role, text)
}

/// `text` in `role`'s colour, for standard error.
pub fn paint_err(role: Role, text: &str) -> String {
    paint_with(ERR.get().and_then(Option::as_ref), role, text)
}

fn paint_with(palette: Option<&Palette>, role: Role, text: &str) -> String {
    match palette {
        Some(p) if !text.is_empty() => format!("{}{text}{RESET}", p.start(role)),
        _ => text.to_owned(),
    }
}

/// `word`, a state or status (`done`, `dirty`, `paused`…), in the colour of what it means: done
/// or running as it should in success, broken in error, held back in warning. Anything else
/// (`todo`, `ready`, `queued`) stays plain.
pub fn state(word: &str) -> String {
    match state_role(word) {
        Some(role) => paint(role, word),
        None => word.to_owned(),
    }
}

fn state_role(word: &str) -> Option<Role> {
    match word {
        "done" | "active" | "succeeded" => Some(Role::Success),
        "dirty" | "failed" | "invalid" => Some(Role::Error),
        "paused" | "cancelled" => Some(Role::Warning),
        _ => None,
    }
}

/// `label` followed by spaces to `width`, in the hint colour: the labels down the left of a
/// page about one thing (`records get`, `queue show`), so the values stand out.
pub fn label(label: &str, width: usize) -> String {
    let pad = " ".repeat(width.saturating_sub(label.chars().count()));
    format!("{}{pad}", paint(Role::Hint, label))
}

/// The finishing pass over what a command prints: `✓` in success colour, `✗` in error colour,
/// "left open" lines as warnings, `(…)` lines as hints; a help page is read by [`help_with`].
/// Tables colour themselves (`render::table`).
pub fn decorate(text: &str) -> String {
    decorate_with(out(), text)
}

/// [`decorate`] for standard error: a usage error's help page.
pub fn decorate_err(text: &str) -> String {
    decorate_with(ERR.get().and_then(Option::as_ref), text)
}

fn decorate_with(palette: Option<&Palette>, text: &str) -> String {
    let Some(p) = palette else { return text.to_owned() };
    if text.starts_with("usage: shimmer") {
        return help_with(p, text);
    }
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let indent = &line[..line.len() - line.trim_start().len()];
            let rest = line.trim_start();
            if let Some(after) = rest.strip_prefix('✓') {
                format!("{indent}{}{}", paint_with(Some(p), Role::Success, "✓"), commands(p, after))
            } else if let Some(after) = rest.strip_prefix('✗') {
                format!("{indent}{}{}", paint_with(Some(p), Role::Error, "✗"), commands(p, after))
            } else if rest.starts_with("left open") {
                format!("{indent}{}", paint_with(Some(p), Role::Warning, rest))
            } else if rest.starts_with('(') && rest.ends_with(')') {
                format!("{indent}{}", paint_with(Some(p), Role::Hint, rest))
            } else if let Some(after) = rest.strip_prefix("usage: shimmer") {
                format!("{indent}usage: {}{after}", paint_with(Some(p), Role::Heading, "shimmer"))
            } else {
                commands(p, line)
            }
        })
        .collect();
    lines.join("\n")
}

/// Each command in `line` you could type next (`see 'shimmer records templates'`, `add one:
/// shimmer records add lc …`, `(undo: shimmer records restore lc/x)`) in the name colour. A
/// command runs to its closing quote, or else to a `)` it didn't open, two spaces (a column
/// gap) or the end of the line.
fn commands(p: &Palette, line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = find_command(rest) {
        let quoted = rest[..at].ends_with('\'');
        let tail = &rest[at..];
        let len = if quoted {
            tail.find('\'').unwrap_or(tail.len())
        } else {
            let mut depth = 0usize;
            let mut end = tail.len();
            for (i, c) in tail.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' if depth == 0 => {
                        end = i;
                        break;
                    }
                    ')' => depth -= 1,
                    ' ' if tail[i..].starts_with("  ") => {
                        end = i;
                        break;
                    }
                    _ => {}
                }
            }
            end
        };
        out.push_str(&rest[..at]);
        out.push_str(&paint_with(Some(p), Role::Name, &tail[..len]));
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// Where the next `shimmer COMMAND` starts in `text`: the word on its own, not `shimmer:` or a
/// part of another word.
fn find_command(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = text[from..].find("shimmer ").map(|i| i + from) {
        let before = text[..i].chars().next_back();
        if before.is_none_or(|c| matches!(c, ' ' | '\'' | '(' | '`')) {
            return Some(i);
        }
        from = i + 1;
    }
    None
}

/// A help page: "shimmer" in the usage line in the heading purple, section titles (`commands:`,
/// `options:`, `moving data:`) in the heading colour, and the command or option starting each
/// entry in the name colour. Only help is read this way: it's the one text with this layout.
fn help_with(p: &Palette, text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            if let Some(after) = line.strip_prefix("usage: shimmer") {
                return format!("usage: {}{after}", paint_with(Some(p), Role::Heading, "shimmer"));
            }
            let is_title = !line.starts_with(' ') && line.ends_with(':') && line.len() < 40 && !line.contains(". ");
            if is_title {
                return paint_with(Some(p), Role::Heading, line);
            }
            // An entry: exactly two spaces, then the command or option it describes.
            match line.strip_prefix("  ").filter(|rest| !rest.starts_with(' ')) {
                Some(rest) => {
                    let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                    let (word, after) = rest.split_at(word_end);
                    format!("  {}{after}", paint_with(Some(p), Role::Name, word))
                }
                None => line.to_owned(),
            }
        })
        .collect();
    lines.join("\n")
}

/// `#rrggbb` → its colour, for a pack's `[colors]`.
pub fn parse_hex(text: &str) -> Option<(u8, u8, u8)> {
    let hex = text.strip_prefix('#').filter(|h| h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit()))?;
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette(truecolor: bool) -> Palette {
        Palette::new(truecolor, &BTreeMap::new())
    }

    #[test]
    fn colour_only_on_a_terminal_and_never_when_asked_not_to() {
        assert!(should_color(true, None, false, false));
        assert!(!should_color(false, None, false, false), "a pipe or a file");
        assert!(!should_color(true, None, false, true), "--json");
        assert!(!should_color(true, None, true, false), "--no-color");
        assert!(!should_color(true, Some("1"), false, false), "NO_COLOR");
        assert!(should_color(true, Some(""), false, false), "an empty NO_COLOR doesn't count");
    }

    #[test]
    fn roles_use_the_logo_colours_in_true_colour_and_the_nearest_basic_ones_otherwise() {
        assert_eq!(paint_with(Some(&palette(true)), Role::Error, "x"), "\x1b[38;2;219;0;124mx\x1b[0m", "the cork");
        assert_eq!(paint_with(Some(&palette(true)), Role::Heading, "x"), "\x1b[1;38;2;114;94;255mx\x1b[0m");
        assert_eq!(paint_with(Some(&palette(false)), Role::Success, "x"), "\x1b[32mx\x1b[0m");
        assert_eq!(paint_with(None, Role::Success, "x"), "x", "off: unchanged");
        assert_eq!(paint(Role::Success, "x"), "x", "never initialised here: plain");
    }

    #[test]
    fn a_command_you_could_type_next_is_in_the_name_colour() {
        let p = palette(false);
        let name = |s: &str| format!("\x1b[36m{s}\x1b[0m");
        assert_eq!(
            decorate_with(
                Some(&p),
                "no collections yet: see 'shimmer records templates', then 'shimmer records new ID'"
            ),
            format!(
                "no collections yet: see '{}', then '{}'",
                name("shimmer records templates"),
                name("shimmer records new ID")
            )
        );
        assert_eq!(
            decorate_with(Some(&p), "removed lc/x (undo: shimmer records restore lc/x)"),
            format!("removed lc/x (undo: {})", name("shimmer records restore lc/x"))
        );
        assert_eq!(
            decorate_with(Some(&p), "  change it: shimmer workspaces edit sc   (the [env] table)"),
            format!("  change it: {}   (the [env] table)", name("shimmer workspaces edit sc"))
        );
        let ok = decorate_with(Some(&p), "✓ made lc\n  add one: shimmer records add lc --title (x) …");
        assert!(ok.ends_with(&format!("add one: {}", name("shimmer records add lc --title (x) …"))), "{ok}");
        assert_eq!(decorate_with(Some(&p), "your Shimmer folder; shimmer: x"), "your Shimmer folder; shimmer: x");
    }

    #[test]
    fn a_state_is_coloured_by_what_it_means() {
        assert_eq!(state_role("done"), Some(Role::Success));
        assert_eq!(state_role("active"), Some(Role::Success));
        assert_eq!(state_role("dirty"), Some(Role::Error));
        assert_eq!(state_role("failed"), Some(Role::Error));
        assert_eq!(state_role("paused"), Some(Role::Warning));
        assert_eq!(state_role("todo"), None, "nothing to say about it: plain");
        assert_eq!(state_role("ready"), None);
        assert_eq!(state("dirty"), "dirty", "never initialised here: plain");
    }

    #[test]
    fn a_role_as_a_shell_script_writes_it() {
        assert_eq!(palette(true).code(Role::Heading), "1;38;2;114;94;255");
        assert_eq!(palette(false).code(Role::Name), "36");
        assert_eq!(palette(true).zsh(Role::Name), "fg=#a294ff");
        assert_eq!(palette(true).zsh(Role::Heading), "bold,fg=#725eff");
        assert_eq!(palette(false).zsh(Role::Hint), "fg=8");
    }

    #[test]
    fn a_pack_can_override_a_role() {
        let mut ov = BTreeMap::new();
        ov.insert("success".to_owned(), (1, 2, 3));
        assert_eq!(paint_with(Some(&Palette::new(true, &ov)), Role::Success, "x"), "\x1b[38;2;1;2;3mx\x1b[0m");
        assert_eq!(paint_with(Some(&Palette::new(true, &ov)), Role::Error, "x"), "\x1b[38;2;219;0;124mx\x1b[0m");
    }

    #[test]
    fn decorate_marks_success_errors_warnings_and_hints_and_keeps_the_words() {
        let p = palette(false);
        let out = decorate_with(
            Some(&p),
            "✓ site is active\n  left open, close it yourself: the Terminal window\n(--limit is the row limit)\nplain",
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "\x1b[32m✓\x1b[0m site is active");
        assert_eq!(lines[1], "  \x1b[33mleft open, close it yourself: the Terminal window\x1b[0m");
        assert_eq!(lines[2], "\x1b[90m(--limit is the row limit)\x1b[0m");
        assert_eq!(lines[3], "plain");
        assert_eq!(decorate_with(Some(&p), "  ✗ failed at step 2"), "  \x1b[35m✗\x1b[0m failed at step 2");
        assert_eq!(decorate_with(None, "✓ x"), "✓ x", "off: unchanged");
    }

    #[test]
    fn shimmer_in_a_help_title_is_the_heading_purple() {
        let out = decorate_with(Some(&palette(true)), "usage: shimmer <command>");
        assert_eq!(out, "usage: \x1b[1;38;2;114;94;255mshimmer\x1b[0m <command>");
        assert_eq!(decorate_with(Some(&palette(false)), "usage: shimmer x"), "usage: \x1b[1;34mshimmer\x1b[0m x");
    }

    #[test]
    fn a_help_page_colours_its_titles_and_entries() {
        let p = palette(false);
        let help = "usage: shimmer records <command>\n\ncommands:\n  list COLLECTION     list records\n                      more words\nThe end.";
        let out: Vec<String> = decorate_with(Some(&p), help).lines().map(String::from).collect();
        assert_eq!(out[2], "\x1b[1;34mcommands:\x1b[0m");
        assert_eq!(out[3], "  \x1b[36mlist\x1b[0m COLLECTION     list records");
        assert_eq!(out[4], "                      more words", "continuation lines untouched");
        assert_eq!(out[5], "The end.");
    }

    #[test]
    fn hex_colours() {
        assert_eq!(parse_hex("#DB007c"), Some((219, 0, 124)));
        for bad in ["db007c", "#db007", "#db007cc", "#gg0000", ""] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
    }
}
