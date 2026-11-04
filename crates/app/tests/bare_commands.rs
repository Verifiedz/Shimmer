//! A feature's name with nothing after it prints that feature's help, for every feature, and never
//! starts the daemon to do it (#337, from #325: `automations`, `notify`, `fetchers` and `vaults`
//! printed a list instead, and started the daemon).

mod common;

use common::{ok, Home};

/// Every feature in `shimmer --help`: the commands shown as `NAME …`. Read from the help, so a new
/// feature is checked here without anyone remembering to add it.
fn features(help: &str) -> Vec<String> {
    help.lines()
        .filter_map(|l| l.strip_prefix("  "))
        .filter_map(|l| l.split_once(" …"))
        .map(|(name, _)| name.trim().to_owned())
        .filter(|name| !name.is_empty() && !name.contains(' '))
        .collect()
}

#[test]
fn every_feature_alone_prints_its_help_without_starting_the_daemon() {
    let home = Home::new();
    let features = features(&ok(&home, &["--help"]));
    assert!(features.len() >= 10, "read the features from the help: {features:?}");
    for f in ["records", "automations", "notify", "fetchers", "vaults"] {
        assert!(features.iter().any(|x| x == f), "{f} is in the help: {features:?}");
    }
    for f in &features {
        let out = ok(&home, &[f.as_str()]);
        assert!(out.starts_with(&format!("usage: shimmer {f} <command>")), "`shimmer {f}` prints its help:\n{out}");
    }
    assert!(!home.socket().exists(), "no daemon was started");
}
