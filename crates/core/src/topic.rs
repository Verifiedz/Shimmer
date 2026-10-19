//! Topic pattern matching: IPC subscriptions, and automations' `on` (ADR 0035 §1). In `core` so a
//! module can use it (a module depends on `core` only, CLAUDE.md §3); `proto` re-exports it.

/// Patterns support a trailing `*` on a segment boundary: `records.*`, `records.item.*`, or
/// `*` alone for everything. No other globbing.
pub fn is_valid_pattern(pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let body = pattern.strip_suffix(".*").unwrap_or(pattern);
    !body.is_empty() && body.split('.').all(|s| !s.is_empty() && !s.contains('*'))
}

pub fn topic_matches(pattern: &str, topic: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix(".*") {
        Some(prefix) => topic.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('.') && rest.len() > 1),
        None => pattern == topic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        assert!(topic_matches("*", "anything.at.all"));
        assert!(topic_matches("records.*", "records.item.created"));
        assert!(topic_matches("records.item.*", "records.item.created"));
        assert!(!topic_matches("records.item.*", "records.other.created"));
        assert!(!topic_matches("records.*", "recordsx.item.created"), "segment boundary");
        assert!(!topic_matches("records.*", "records"));
        assert!(topic_matches("queue.task.failed", "queue.task.failed"));
        assert!(!topic_matches("queue.task.failed", "queue.task.failed.x"));
    }

    #[test]
    fn validity() {
        for ok in ["*", "records.*", "a.b.c", "a.b.*"] {
            assert!(is_valid_pattern(ok), "{ok}");
        }
        for bad in ["", "rec*", "*.item", "a.*.c", ".*", "a..b", "**"] {
            assert!(!is_valid_pattern(bad), "{bad}");
        }
    }
}
