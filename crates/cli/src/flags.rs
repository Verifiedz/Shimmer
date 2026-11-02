//! Splitting a subcommand's words into positional arguments, `--name VALUE` options and switches,
//! for commands with more than one or two flags (`queue`, `scheduler`). `--name=VALUE` works too.

pub struct Flags {
    pub positional: Vec<String>,
    values: Vec<(&'static str, String)>,
    switches: Vec<&'static str>,
}

impl Flags {
    /// `words` after the subcommand. `options` take a value; `switches` don't. Anything else
    /// starting with `-` is an error naming it.
    pub fn split(words: Vec<String>, options: &[&'static str], switches: &[&'static str]) -> Result<Self, String> {
        let mut flags = Flags { positional: Vec::new(), values: Vec::new(), switches: Vec::new() };
        let mut words = words.into_iter();
        while let Some(word) = words.next() {
            if !word.starts_with("--") || word.len() == 2 {
                if word.starts_with('-') && word.len() > 1 {
                    return Err(format!("unknown option '{word}'"));
                }
                flags.positional.push(word);
                continue;
            }
            let (name, inline) = match word.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (word.clone(), None),
            };
            if let Some(opt) = options.iter().find(|o| **o == name) {
                let value = inline.or_else(|| words.next()).filter(|v| !v.is_empty());
                let value = value.ok_or_else(|| format!("{name} needs a value"))?;
                if flags.values.iter().any(|(n, _)| n == opt) {
                    return Err(format!("{name} given twice"));
                }
                flags.values.push((opt, value));
            } else if let Some(sw) = switches.iter().find(|s| **s == name).filter(|_| inline.is_none()) {
                flags.switches.push(sw);
            } else {
                let known: Vec<&str> = options.iter().chain(switches).copied().collect();
                return Err(crate::suggest::unknown_option(&name, &known));
            }
        }
        Ok(flags)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }

    pub fn has(&self, name: &str) -> bool {
        self.switches.contains(&name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &[&str]) -> Vec<String> {
        s.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn options_switches_and_positionals() {
        let f = Flags::split(
            words(&["a", "--every", "2h", "--catch-up=skip", "--last", "b"]),
            &["--every", "--catch-up"],
            &["--last"],
        )
        .unwrap();
        assert_eq!(f.positional, ["a", "b"]);
        assert_eq!((f.value("--every"), f.value("--catch-up"), f.value("--lane")), (Some("2h"), Some("skip"), None));
        assert!(f.has("--last"));
    }

    #[test]
    fn mistakes_are_named() {
        let split = |w: &[&str]| Flags::split(words(w), &["--every"], &["--last"]).err().unwrap();
        assert_eq!(split(&["--bogus"]), "unknown option '--bogus'");
        assert_eq!(split(&["-x"]), "unknown option '-x'");
        assert_eq!(split(&["--every"]), "--every needs a value");
        assert_eq!(split(&["--every="]), "--every needs a value");
        assert_eq!(split(&["--every", "1h", "--every", "2h"]), "--every given twice");
        assert_eq!(split(&["--last=yes"]), "unknown option '--last'");
    }
}
