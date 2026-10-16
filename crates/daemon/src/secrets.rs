//! Where a credential's secret *value* lives (ADR 0036 §3) — never `config.toml`, which is
//! meant to be git-committable (CLAUDE.md §1.4/§7). `core` never imports this: a module only
//! ever sees a credential by name (`crates/core/src/http.rs`), resolved entirely here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use shimmer_core::{Error, Result};

/// Resolves a credential name (a `[credentials.<name>]` key, §2) to its secret value. Never
/// logged by any caller (§6) — only the name a `SecretStore` was asked to resolve is safe to
/// log, never what comes back.
pub trait SecretStore: Send + Sync {
    fn resolve(&self, name: &str) -> Option<String>;
}

/// Tries each store in order; the first to resolve a name wins. A later store is never
/// consulted once an earlier one answers, even with an empty string (an explicit, if odd,
/// value some user configured) — only `None` falls through.
pub struct ChainedSecretStore(pub Vec<Box<dyn SecretStore>>);

impl SecretStore for ChainedSecretStore {
    fn resolve(&self, name: &str) -> Option<String> {
        self.0.iter().find_map(|s| s.resolve(name))
    }
}

/// `SHIMMER_SECRET_<NAME>`, with `name` uppercased and every non-alphanumeric byte replaced by
/// `_` — so `my-api-key` reads `SHIMMER_SECRET_MY_API_KEY`. For CI, ephemeral setups, or
/// anyone who already manages secrets via their shell environment.
pub struct EnvSecretStore;

impl SecretStore for EnvSecretStore {
    fn resolve(&self, name: &str) -> Option<String> {
        std::env::var(env_var_name(name)).ok()
    }
}

fn env_var_name(name: &str) -> String {
    let sanitized: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    format!("SHIMMER_SECRET_{sanitized}")
}

/// A flat `name = "value"` TOML map, outside `$SHIMMER_HOME` on purpose (§3) — loaded once at
/// daemon startup, same "read once, fail loud on a malformed file" posture `Config::load`
/// already has for `config.toml`.
pub struct FileSecretStore {
    values: HashMap<String, String>,
}

impl FileSecretStore {
    /// `None` is a normal, silent "no file store configured" (no `secrets.toml` exists) —
    /// distinct from `Some(Err(..))`, a file that exists but is unreadable, malformed, or (on
    /// unix) not mode `0600`, which must stop the daemon's startup rather than quietly running
    /// with no secrets loaded.
    pub fn load_if_present(path: &Path) -> Option<Result<Self>> {
        if !path.exists() {
            return None;
        }
        Some(Self::load(path))
    }

    fn load(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        check_permissions(path)?;
        let text =
            std::fs::read_to_string(path).map_err(|e| Error::internal(format!("reading {}: {e}", path.display())))?;
        let table: HashMap<String, String> = toml::from_str(&text).map_err(|e| {
            Error::invalid_params(format!("{}: must be a flat table of name = \"value\" pairs: {e}", path.display()))
        })?;
        Ok(Self { values: table })
    }
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| Error::internal(format!("stat {}: {e}", path.display())))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(Error::invalid_params(format!(
            "{}: must be mode 0600 (found {mode:o}) -- a secrets file readable by anyone but you is not safe to load",
            path.display()
        )));
    }
    Ok(())
}

impl SecretStore for FileSecretStore {
    fn resolve(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }
}

/// `$XDG_CONFIG_HOME/shimmer/secrets.toml`, else `~/.config/shimmer/secrets.toml` — the same
/// convention the CLI's own `cli.toml` already uses (`crates/cli/src/packs/settings.rs`, ADR
/// 0013 §3), kept outside `$SHIMMER_HOME` deliberately (§3). `None` if neither
/// `XDG_CONFIG_HOME` nor `HOME` is set.
pub fn default_secrets_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| {
        std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|home| PathBuf::from(home).join(".config"))
    })?;
    Some(base.join("shimmer/secrets.toml"))
}

/// The daemon's full chain (§3): the file store (if `secrets.toml` exists and loads cleanly)
/// first, then the environment, always available. A `secrets.toml` that exists but fails to
/// load (bad permissions, malformed TOML) fails this outright -- the daemon must not start up
/// silently ignoring a secrets file the user clearly meant to use.
pub fn load_chain() -> Result<Arc<dyn SecretStore>> {
    let mut stores: Vec<Box<dyn SecretStore>> = Vec::new();
    if let Some(path) = default_secrets_path() {
        if let Some(file_store) = FileSecretStore::load_if_present(&path) {
            stores.push(Box::new(file_store?));
        }
    }
    stores.push(Box::new(EnvSecretStore));
    Ok(Arc::new(ChainedSecretStore(stores)))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn env_var_name_uppercases_and_sanitizes() {
        assert_eq!(env_var_name("my-api-key"), "SHIMMER_SECRET_MY_API_KEY");
        assert_eq!(env_var_name("figma.jobs"), "SHIMMER_SECRET_FIGMA_JOBS");
    }

    #[test]
    fn env_store_resolves_nothing_for_a_name_with_no_matching_variable() {
        // §12 rule 8 bans `unsafe`, so this never mutates the real process environment
        // (`std::env::set_var`/`remove_var` require it) -- `env_var_name_uppercases_and_sanitizes`
        // above already covers the name-mapping logic; `std::env::var` itself is stdlib-trusted.
        assert_eq!(EnvSecretStore.resolve("almost-certainly-never-set-anywhere-xyz"), None);
    }

    #[test]
    fn file_store_round_trips_a_value() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "my-api-key = \"abc123\"\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let store = FileSecretStore::load_if_present(&path).unwrap().unwrap();
        assert_eq!(store.resolve("my-api-key"), Some("abc123".to_owned()));
        assert_eq!(store.resolve("no-such-key"), None);
    }

    #[test]
    fn a_missing_file_is_none_not_an_error() {
        let dir = TempDir::new().unwrap();
        assert!(FileSecretStore::load_if_present(&dir.path().join("secrets.toml")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_file_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "k = \"v\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        // `.unwrap_err()` would need `FileSecretStore: Debug`, which must never be derived --
        // that would make the secret values it holds printable by accident (§6).
        match FileSecretStore::load_if_present(&path).unwrap() {
            Ok(_) => panic!("a world-readable secrets file must be rejected, not loaded"),
            Err(e) => assert_eq!(e.code, shimmer_core::ErrorCode::InvalidParams),
        }
    }

    #[test]
    fn chained_store_tries_each_in_order_until_one_resolves() {
        let a = FileSecretStore { values: HashMap::from([("only-a".to_owned(), "a".to_owned())]) };
        let b = FileSecretStore { values: HashMap::from([("only-b".to_owned(), "b".to_owned())]) };
        let chain = ChainedSecretStore(vec![Box::new(a), Box::new(b)]);
        assert_eq!(chain.resolve("only-a"), Some("a".to_owned()));
        assert_eq!(chain.resolve("only-b"), Some("b".to_owned()));
        assert_eq!(chain.resolve("neither"), None);
    }
}
