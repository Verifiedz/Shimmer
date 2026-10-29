//! #309's third review, end to end: while a save waits (its file can't be written), the next
//! `records update` reads through it, so both changes are kept once the files can be written.

mod common;

use common::{ok, Home};

#[cfg(unix)]
#[test]
fn two_updates_while_the_first_waits_keep_both_changes() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::with_leetcode();
    ok(&home, &["records", "add", "leetcode/two-sum", "--title", "Two Sum", "--difficulty", "easy"]);
    let items = home.dir.path().join("home/data/records/items/leetcode");
    std::fs::set_permissions(&items, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::File::create(items.join("probe")).is_ok() {
        return; // root ignores permissions
    }
    ok(&home, &["records", "update", "leetcode/two-sum", "--difficulty", "hard"]);
    ok(&home, &["records", "update", "leetcode/two-sum", "--url", "https://x.test/two-sum"]);
    std::fs::set_permissions(&items, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The next save lands the waiting ones, in order.
    ok(&home, &["records", "add", "leetcode/other", "--title", "Other", "--difficulty", "easy"]);
    let file = std::fs::read_to_string(items.join("two-sum.toml")).unwrap();
    assert!(file.contains("hard"), "the first change kept: {file}");
    assert!(file.contains("https://x.test/two-sum"), "the second change kept: {file}");
    ok(&home, &["shutdown"]);
}
