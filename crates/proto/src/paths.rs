//! Where things live on disk (CLAUDE.md §7, protocol.md "Transport"). Path arithmetic from the
//! environment; the only filesystem access is resolving symlinks in an existing folder and reading
//! who owns the home folder (ADR 0039).

use std::env;
use std::path::{Component, Path, PathBuf};

/// `$SHIMMER_HOME`, else the platform data dir.
pub fn shimmer_home() -> PathBuf {
    if let Some(p) = env::var_os("SHIMMER_HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support/shimmer");
    }
    match env::var_os("XDG_DATA_HOME").filter(|p| !p.is_empty()) {
        Some(x) => PathBuf::from(x).join("shimmer"),
        None => home.join(".local/share/shimmer"),
    }
}

/// `$SHIMMER_SOCKET` (what workspace scripts see) wins. Otherwise [`default_socket`] for
/// [`shimmer_home`].
pub fn socket_path() -> PathBuf {
    if let Some(p) = env::var_os("SHIMMER_SOCKET").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    default_socket(&shimmer_home())
}

/// The socket a daemon for `home` listens on when `SHIMMER_SOCKET` isn't set (ADR 0039 §1, §5):
/// `$XDG_RUNTIME_DIR/shimmer/<id>.sock` on Linux, else `<home>/daemon.sock`, and
/// `/tmp/shimmer-<uid>/<id>.sock` when that would be too long for a socket.
pub fn default_socket(home: &Path) -> PathBuf {
    let runtime = cfg!(target_os = "linux").then(|| env::var_os("XDG_RUNTIME_DIR")).flatten();
    default_socket_in(home, runtime.as_deref().map(Path::new).filter(|p| !p.as_os_str().is_empty()), owner_uid())
}

/// [`default_socket`] with its inputs given, so it can be tested on any platform.
pub fn default_socket_in(home: &Path, runtime_dir: Option<&Path>, uid: u32) -> PathBuf {
    let id = home_id(home);
    let preferred = match runtime_dir {
        Some(rt) => rt.join("shimmer").join(format!("{id}.sock")),
        None => home.join("daemon.sock"),
    };
    if fits(&preferred) {
        preferred
    } else {
        short_socket_dir(uid).join(format!("{id}.sock"))
    }
}

/// The private folder in `/tmp` for sockets whose usual path is too long (ADR 0039 §5). Its owner
/// and mode are checked by whoever creates or connects to it ([`check_private_dir`]).
pub fn short_socket_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/shimmer-{uid}"))
}

/// The longest socket path this platform takes, in bytes: `sun_path` holds 104 bytes on macOS and
/// 108 on Linux, including the terminating NUL.
pub const MAX_SOCKET_PATH: usize = if cfg!(target_os = "macos") { 103 } else { 107 };

/// Whether `path` can be a socket path here.
pub fn fits(path: &Path) -> bool {
    path.as_os_str().len() <= MAX_SOCKET_PATH
}

/// What an explicit `SHIMMER_SOCKET` that can't work should say (ADR 0039 §5). `None` when it fits.
pub fn too_long(path: &Path) -> Option<String> {
    (!fits(path)).then(|| {
        format!(
            "the socket path {} is {} bytes; a socket path can be at most {MAX_SOCKET_PATH} here, so use a shorter SHIMMER_SOCKET",
            path.display(),
            path.as_os_str().len()
        )
    })
}

/// A Shimmer folder's path, resolved the same way everywhere (ADR 0039 §1): made absolute, `.` and
/// `..` removed, and the longest part that exists canonicalised (symlinks followed), with the rest
/// appended. So a folder gets the same answer before and after it exists, and through a symlink.
pub fn resolve(home: &Path) -> PathBuf {
    let absolute =
        if home.is_absolute() { home.to_path_buf() } else { env::current_dir().unwrap_or_default().join(home) };
    let mut clean = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    let mut existing = clean.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            return rest.iter().rev().fold(real, |p, name| p.join(name));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return clean,
        }
    }
}

/// 16 hex digits naming a Shimmer folder in socket paths (ADR 0039 §1): 64-bit FNV-1a over the
/// bytes of its [`resolve`]d path. **Never change this**: login units and scripts hold the path.
pub fn home_id(home: &Path) -> String {
    let resolved = resolve(home);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in resolved.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Who owns the user's home folder: the uid to name and check `/tmp/shimmer-<uid>` by, found
/// without `unsafe` (CLAUDE.md §12 rule 8). 0 when there is no `$HOME` to look at.
pub fn owner_uid() -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    env::var_os("HOME").and_then(|h| std::fs::metadata(h).ok()).map_or(0, |m| m.uid())
}

/// A folder that will hold a socket in a shared place (`/tmp`) must be a real folder, owned by
/// `uid`, and closed to everyone else (ADR 0039 §5). Someone else may have made it first.
pub fn check_private_dir(dir: &Path, uid: u32) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} isn't a folder; remove it so Shimmer can make its own", dir.display()));
    }
    if meta.uid() != uid {
        return Err(format!("{} belongs to someone else; Shimmer won't use it for your socket", dir.display()));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} is open to other users; run: chmod 700 {}", dir.display(), dir.display()));
    }
    Ok(())
}

/// Whether `socket` is in the shared short-socket folder, so its folder needs
/// [`check_private_dir`].
pub fn in_short_socket_dir(socket: &Path, uid: u32) -> bool {
    socket.parent() == Some(short_socket_dir(uid).as_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_folders_get_two_sockets_and_one_folder_always_the_same() {
        let tmp = std::env::temp_dir().join(format!("shimmer-paths-{}", std::process::id()));
        let (a, b) = (tmp.join("a"), tmp.join("b"));
        let rt = Path::new("/run/user/1000");
        let (sa, sb) = (default_socket_in(&a, Some(rt), 1000), default_socket_in(&b, Some(rt), 1000));
        assert_ne!(sa, sb, "two folders, two sockets");
        assert!(sa.starts_with("/run/user/1000/shimmer") && sa.extension().unwrap() == "sock", "{sa:?}");

        // Before the folder exists, after it exists, and through a symlink: the same socket.
        let before = default_socket_in(&a, Some(rt), 1000);
        std::fs::create_dir_all(&a).unwrap();
        assert_eq!(default_socket_in(&a, Some(rt), 1000), before);
        let link = tmp.join("link");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        assert_eq!(default_socket_in(&link, Some(rt), 1000), before);
        assert_eq!(default_socket_in(&tmp.join("a/../a/."), Some(rt), 1000), before);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn without_a_runtime_folder_the_socket_is_in_the_shimmer_folder() {
        let home = Path::new("/home/me/.local/share/shimmer");
        assert_eq!(default_socket_in(home, None, 1000), home.join("daemon.sock"));
    }

    #[test]
    fn a_socket_path_too_long_moves_to_a_short_private_folder() {
        let deep = PathBuf::from(format!("/home/me/{}/shimmer", "very-long-folder-name/".repeat(6)));
        let socket = default_socket_in(&deep, None, 1000);
        assert_eq!(socket.parent().unwrap(), Path::new("/tmp/shimmer-1000"));
        assert!(fits(&socket) && in_short_socket_dir(&socket, 1000), "{socket:?}");
        assert_eq!(socket.file_name().unwrap().to_str().unwrap(), format!("{}.sock", home_id(&deep)));
        assert!(too_long(&deep.join("daemon.sock")).unwrap().contains("at most"));
        assert!(too_long(&socket).is_none());
    }

    #[test]
    fn the_id_never_changes() {
        // Login units and scripts hold socket paths: this value must stay the same forever.
        assert_eq!(home_id(Path::new("/nonexistent-shimmer-test/home")), "a9ed5aaf25292a1c");
    }

    #[test]
    fn a_shared_folder_must_be_ours_and_closed() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("shimmer-priv-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        use std::os::unix::fs::MetadataExt as _;
        let me = std::fs::metadata(&dir).unwrap().uid();
        assert!(check_private_dir(&dir, me).is_ok());
        assert!(check_private_dir(&dir, me + 1).unwrap_err().contains("someone else"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_private_dir(&dir, me).unwrap_err().contains("chmod 700"));
        std::fs::remove_dir(&dir).unwrap();
        let file = dir.with_extension("file");
        std::fs::write(&file, "").unwrap();
        assert!(check_private_dir(&file, me).unwrap_err().contains("isn't a folder"));
        std::fs::remove_file(&file).unwrap();
    }
}
