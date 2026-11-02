//! Crash-safe file primitives: write to a temp file in the same directory, fsync, rename,
//! fsync the directory. A reader sees the old file or the new one, never a torn one (§7.1).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Makes each in-flight temp file's name its own (#260 finding 3).
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

pub fn write_atomic(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = dest.parent().ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    // Unique per call, not just per process: two writers of one file at once (the HTTP cache's
    // two requests for one url) each rename their own temp file, and the last rename wins
    // cleanly, where one shared name let one writer's rename take the other's file (#260).
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.tmp-{}-{seq}", std::process::id()));
    let result = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, dest)?;
        fsync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Leftovers of an interrupted [`write_atomic`]; never real data.
pub fn is_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.contains(".tmp-")
}

/// Collect files under `dir` as paths relative to `root`, using `/` separators.
pub fn walk_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            walk_files(root, &path, out)?;
        } else if ty.is_file() && !is_temp_name(&entry.file_name().to_string_lossy()) {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"));
            }
        }
    }
    Ok(())
}
