//! A damaged crash-recovery folder doesn't stop Shimmer starting (#260 finding 2): it's kept
//! aside in `.staging/broken/`, and `shimmer doctor` says so.

mod common;

use common::Home;

#[test]
fn a_damaged_save_left_by_a_crash_is_kept_aside_and_the_daemon_starts() {
    let home = Home::new();
    let tx = home.dir.path().join("home/.staging/01jabcdefghjkmnpqrstvwxyz0");
    std::fs::create_dir_all(tx.join("files")).unwrap();
    std::fs::write(tx.join("manifest.json"), "{ half written").unwrap();
    std::fs::write(tx.join("COMMITTED"), "").unwrap();

    let ping = home.shimmer(&["ping"]);
    assert!(ping.status.success(), "it starts: {}", String::from_utf8_lossy(&ping.stderr));
    let kept = home.dir.path().join("home/.staging/broken/01jabcdefghjkmnpqrstvwxyz0");
    assert!(kept.join("manifest.json").exists() && kept.join("COMMITTED").exists(), "kept whole");

    let doctor = home.shimmer(&["doctor"]);
    let out = String::from_utf8_lossy(&doctor.stdout);
    assert!(out.contains("1 damaged save(s) couldn't be applied and were kept in"), "{out}");
    home.shimmer(&["shutdown"]);
}
