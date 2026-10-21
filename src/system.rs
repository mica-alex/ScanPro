//! Hand-offs to the desktop: open a file, reveal it in the file manager, run
//! the user's after-save command.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn open(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = Command::new("open");
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        // The empty "" is `start`'s window title; without it a quoted path
        // would be taken as the title.
        c.args(["/C", "start", ""]);
        c
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut cmd = Command::new("xdg-open");
    cmd.arg(path).spawn().with_context(|| format!("opening {}", path.display()))?;
    Ok(())
}

pub fn reveal(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    Command::new("open").arg("-R").arg(path).spawn()?;
    #[cfg(windows)]
    {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path);
        Command::new("explorer").arg(arg).spawn()?;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        // No portable "select this file"; open the folder instead.
        Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn()?;
    }
    Ok(())
}

/// Quote `s` for the platform shell that [`run_hook`] uses.
pub fn shell_quote(s: &str) -> String {
    if cfg!(windows) { format!("\"{}\"", s.replace('"', "\"\"")) } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// Run an after-save command through the shell, with `{path}` replaced by
/// the (quoted) saved file and `{folder}` by its folder. Waits for it so a
/// failure can be reported.
pub fn run_hook(template: &str, path: &Path) -> Result<()> {
    let folder = path.parent().unwrap_or(path);
    let command = template
        .replace("{path}", &shell_quote(&path.to_string_lossy()))
        .replace("{folder}", &shell_quote(&folder.to_string_lossy()));
    #[cfg(windows)]
    let status = Command::new("cmd").args(["/C", &command]).status();
    #[cfg(not(windows))]
    let status = Command::new("sh").args(["-c", &command]).status();
    let status = status.with_context(|| format!("running {command}"))?;
    if !status.success() {
        bail!("after-save command exited with {status}");
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn hook_receives_quoted_path() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("it's here.pdf");
        std::fs::write(&target, b"x").unwrap();
        let marker = dir.path().join("out.txt");
        run_hook(&format!("cp {{path}} {}", shell_quote(&marker.to_string_lossy())), &target).unwrap();
        assert!(marker.exists());
    }
}
