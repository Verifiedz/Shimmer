//! A damaged schedule file is shown, never silently gone (#263 finding 5): in
//! `shimmer scheduler list` and in `shimmer doctor`, with the way out.

mod common;

use common::Home;

#[test]
fn a_damaged_schedule_shows_in_scheduler_list_and_doctor() {
    let home = Home::new();
    let dir = home.dir.path().join("home/data/scheduler/triggers");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("usr-damaged.json"), "{\"spec\": {").unwrap();

    let list = home.shimmer(&["scheduler", "list"]);
    let out = String::from_utf8_lossy(&list.stdout);
    assert!(list.status.success(), "{out}{}", String::from_utf8_lossy(&list.stderr));
    assert!(out.contains("usr-damaged can't be read") && out.contains("shimmer scheduler remove usr-damaged"), "{out}");

    let doctor = home.shimmer(&["doctor"]);
    let out = String::from_utf8_lossy(&doctor.stdout);
    assert!(out.contains("usr-damaged can't be read, so it doesn't run"), "{out}");
    assert!(out.contains("shimmer scheduler remove usr-damaged"), "{out}");

    let removed = home.shimmer(&["scheduler", "remove", "usr-damaged"]);
    assert!(removed.status.success(), "{}", String::from_utf8_lossy(&removed.stderr));
    assert!(!dir.join("usr-damaged.json").exists());
    let doctor = home.shimmer(&["doctor"]);
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("every file readable"));
    home.shimmer(&["shutdown"]);
}
