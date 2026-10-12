//! Opening a file in the person's editor, for `workspaces edit` and `records edit`: the one
//! place the CLI starts a program for the person (§2: the daemon still owns the data, and checks
//! what was saved the next time it reads the file).

use std::path::Path;

use shimmer_core::{Error, Result};

/// Open `path` in the editor and wait for it to close. An error when there's no editor or it
/// fails, so a caller never checks a file nobody saved.
pub fn open(path: &Path) -> Result<()> {
    let editor =
        editor_command(std::env::var("VISUAL").ok().as_deref(), std::env::var("EDITOR").ok().as_deref(), which)
            .ok_or_else(|| Error::invalid_params("no editor found: set $EDITOR (e.g. export EDITOR=nano)"))?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()
        .map_err(|e| Error::unavailable(format!("couldn't start {editor}: {e}")))?;
    if !status.success() {
        return Err(Error::unavailable(format!("{editor} exited with {status}; nothing checked")));
    }
    Ok(())
}

/// `$VISUAL`, then `$EDITOR`, then the first of `nano`, `vi` that exists. A GUI editor needs its
/// wait flag (`EDITOR="code --wait"`), or the check runs before you've saved.
fn editor_command(visual: Option<&str>, editor: Option<&str>, has: impl Fn(&str) -> bool) -> Option<String> {
    [visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|e| !e.is_empty())
        .map(str::to_owned)
        .or_else(|| ["nano", "vi"].into_iter().find(|e| has(e)).map(str::to_owned))
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_is_visual_then_editor_then_nano_or_vi() {
        let has = |p: &str| p == "vi";
        assert_eq!(editor_command(Some("code --wait"), Some("nano"), has).as_deref(), Some("code --wait"));
        assert_eq!(editor_command(Some(" "), Some("nano"), has).as_deref(), Some("nano"));
        assert_eq!(editor_command(None, None, has).as_deref(), Some("vi"));
        assert_eq!(editor_command(None, None, |_| false), None);
    }
}
