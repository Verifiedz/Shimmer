//! The inbox's cap (ADR 0032 §3), so it can't fill the disk even if nobody reads it. Pure.

/// What to remove from an inbox whose messages are `(id, read)`, oldest first, to keep at most
/// `max`: the oldest read ones first, and only when no read one is left, the oldest unread.
/// Returns the ids, and how many of them were unread.
pub fn prune(messages: &[(String, bool)], max: usize) -> (Vec<String>, usize) {
    let mut over = messages.len().saturating_sub(max);
    if over == 0 {
        return (Vec::new(), 0);
    }
    let mut remove = Vec::new();
    for (id, _) in messages.iter().filter(|(_, read)| *read).take(over) {
        remove.push(id.clone());
    }
    over -= remove.len();
    let unread: Vec<String> = messages.iter().filter(|(_, read)| !*read).take(over).map(|(id, _)| id.clone()).collect();
    let unread_removed = unread.len();
    remove.extend(unread);
    (remove, unread_removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inbox(reads: &[bool]) -> Vec<(String, bool)> {
        reads.iter().enumerate().map(|(i, r)| (format!("m{i}"), *r)).collect()
    }

    #[test]
    fn nothing_goes_up_to_the_cap() {
        assert_eq!(prune(&inbox(&[true, true, false]), 3), (vec![], 0));
    }

    #[test]
    fn the_oldest_read_go_first() {
        let (gone, unread) = prune(&inbox(&[false, true, false, true, true]), 3);
        assert_eq!((gone, unread), (vec!["m1".to_owned(), "m3".to_owned()], 0), "unread m0 stays though older");
    }

    #[test]
    fn then_the_oldest_unread() {
        let (gone, unread) = prune(&inbox(&[false, false, true, false]), 1);
        assert_eq!(gone, ["m2", "m0", "m1"]);
        assert_eq!(unread, 2);
    }
}
