//! Locating a shell and stopping process trees.

use std::path::{Path, PathBuf};

/// How to run a command string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shell {
    /// Executable.
    pub program: PathBuf,
    /// Arguments placed before the command string.
    pub args: Vec<String>,
}

impl Shell {
    /// Bash where available (Git Bash on Windows), else `sh` / `cmd.exe`.
    pub fn detect() -> Self {
        if let Some(bash) = find_bash() {
            return Self {
                program: bash,
                args: vec!["-c".into()],
            };
        }
        if cfg!(windows) {
            Self {
                program: PathBuf::from("cmd.exe"),
                args: vec!["/C".into()],
            }
        } else {
            Self {
                program: PathBuf::from("/bin/sh"),
                args: vec!["-c".into()],
            }
        }
    }

    /// A display name (`bash`, `sh`, `cmd`).
    pub fn name(&self) -> String {
        self.program
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shell".into())
    }
}

fn find_bash() -> Option<PathBuf> {
    if cfg!(windows) {
        let mut candidates = Vec::new();
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(var) {
                candidates.push(Path::new(&dir).join("Git").join("bin").join("bash.exe"));
            }
        }
        candidates
            .into_iter()
            .find(|path| path.is_file())
            .or_else(|| find_on_path("bash.exe"))
    } else {
        ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .or_else(|| find_on_path("bash"))
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// Kills `pid` and its descendants. On Unix the command runs in its own process group,
/// so the whole group is killed; on Windows `taskkill /T` walks the tree.
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        if let Ok(pid) = i32::try_from(pid) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}
