//! The program in a terminal's foreground, for its label (like VS Code's `${process}`).

use std::future::Future;

/// Watches a PTY's foreground process group.
pub struct Watch {
    #[cfg(unix)]
    pty: Option<std::fs::File>,
    /// The last foreground process group seen.
    #[cfg(unix)]
    last: Option<i32>,
}

impl Watch {
    #[cfg(unix)]
    pub fn new(pty: &alacritty_terminal::tty::Pty) -> Self {
        Self {
            pty: pty.file().try_clone().ok(),
            last: None,
        }
    }

    #[cfg(not(unix))]
    pub fn new<T>(_pty: &T) -> Self {
        Self {}
    }

    /// When the foreground process changed since the last call: a lookup of its name
    /// (to run in the background).
    #[cfg(unix)]
    pub fn changed(&mut self) -> Option<impl Future<Output = Option<String>> + Send + 'static> {
        let group = rustix::termios::tcgetpgrp(self.pty.as_ref()?).ok()?;
        let pid = group.as_raw_nonzero().get();
        if self.last == Some(pid) {
            return None;
        }
        self.last = Some(pid);
        Some(async move { name_of(pid) })
    }

    #[cfg(not(unix))]
    pub fn changed(&mut self) -> Option<std::future::Ready<Option<String>>> {
        None
    }
}

/// The command name of process `pid`.
#[cfg(target_os = "linux")]
fn name_of(pid: i32) -> Option<String> {
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    clean(&name)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn name_of(pid: i32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    clean(&String::from_utf8_lossy(&output.stdout))
}

/// `-zsh` (a login shell) or `/bin/zsh` → `zsh`.
#[cfg(unix)]
fn clean(name: &str) -> Option<String> {
    let name = name.trim();
    let name = name.rsplit('/').next().unwrap_or(name);
    let name = name.trim_start_matches('-');
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean("-zsh\n").as_deref(), Some("zsh"));
        assert_eq!(clean("/usr/bin/vim").as_deref(), Some("vim"));
        assert_eq!(clean("  "), None);
        assert!(name_of(std::process::id() as i32).is_some());
    }
}
