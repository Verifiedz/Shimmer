//! Identifier newtypes. Plain strings on the wire, distinct types in code.

use std::fmt;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self::new(s)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self::new(s)
            }
        }
    };
}

string_id!(
    /// A module's identity, e.g. `records`. Also the prefix of every op and topic it owns.
    ModuleId
);
string_id!(
    /// An execution lane, e.g. `fetchers` (§6.1).
    LaneId
);
string_id!(
    /// A scheduler trigger.
    TriggerId
);

impl LaneId {
    /// The shared lane for anything that declares none.
    pub fn default_lane() -> Self {
        Self::new("default")
    }
}

/// A queued unit of work. ULID: sortable, generated on enqueue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(Ulid);

impl TaskId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for TaskId {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ulid::from_string(s).map(Self).map_err(|_| crate::Error::invalid_params(format!("'{s}' is not a task id")))
    }
}

fn valid_id_shaped(s: &str, first_ok: impl Fn(char) -> bool) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if first_ok(c))
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// True for `[a-z][a-z0-9_-]*`: safe as a module id, a data namespace and a path segment.
pub fn is_valid_name(s: &str) -> bool {
    valid_id_shaped(s, |c| c.is_ascii_lowercase())
}

/// True for `[a-z0-9][a-z0-9_-]*` (ADR 0008): a record id, a workspace launch step name, or
/// anything else sharing that charset but — unlike [`is_valid_name`] — allowing a leading
/// digit, so `1-two-sum` or `01-setup` work.
pub fn is_valid_id(s: &str) -> bool {
    valid_id_shaped(s, |c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

#[cfg(test)]
mod id_shape_tests {
    use super::*;

    #[test]
    fn is_valid_name_requires_a_leading_letter() {
        assert!(is_valid_name("deep-work"));
        assert!(is_valid_name("a1"));
        assert!(!is_valid_name("1-two-sum"), "is_valid_name does not allow a leading digit");
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("Setup"));
        assert!(!is_valid_name("a/b"));
    }

    #[test]
    fn is_valid_id_allows_a_leading_digit() {
        assert!(is_valid_id("1-two-sum"));
        assert!(is_valid_id("01-setup"));
        assert!(is_valid_id("deep-work"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("Setup.v2"));
        assert!(!is_valid_id("../x"));
        assert!(!is_valid_id("a/b"));
    }
}
