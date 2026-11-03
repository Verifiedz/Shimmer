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

/// #309's review: with --socket, doctor may be asking another folder's daemon, so it doesn't
/// report this folder's saves as if they were that one's: it says they weren't checked.
#[test]
fn doctor_with_socket_says_saves_werent_checked() {
    let home = Home::new();
    assert!(home.shimmer(&["ping"]).status.success());
    let socket = home.socket();
    let out = home.shimmer(&["--socket", socket.to_str().unwrap(), "doctor"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("saves not checked: with --socket"), "{text}");
    assert!(!text.contains("every save finished"), "{text}");
    home.shimmer(&["shutdown"]);
}
