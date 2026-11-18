//! Prepare and verify before quitting; a helper replaces files after process exit.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, ensure};
use zp_core::update::{PackageKind, Update};

pub(super) struct Prepared {
    command: Command,
    staging: tempfile::TempDir,
}

impl Prepared {
    pub fn start(mut self) -> anyhow::Result<()> {
        let log = File::create(self.staging.path().join("helper.log"))?;
        self.command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        self.command
            .spawn()
            .context("could not start the update helper")?;
        let _ = self.staging.keep();
        Ok(())
    }
}

pub(super) fn prepare(
    update: &Update,
    download: &Path,
    bundle: Option<&Path>,
) -> anyhow::Result<Prepared> {
    // Recheck the on-disk file before executing installers or extracting packages.
    zp_core::update::verify_download(download, &update.asset)?;

    let current_exe = std::env::current_exe()?;
    let target = match update.asset.kind {
        PackageKind::MacApp => {
            let bundle = bundle.context("install ZP in Applications before updating")?;
            ensure!(
                bundle.extension().is_some_and(|e| e == "app"),
                "this executable is not an installed macOS application"
            );
            bundle.to_path_buf()
        }
        PackageKind::AppImage => std::env::var_os("APPIMAGE")
            .map(Into::into)
            .context("the AppImage location is unavailable")?,
        _ => current_exe,
    };
    let parent = target.parent().context("no installation directory")?;
    let staging = tempfile::Builder::new()
        .prefix(".zp-update-")
        .tempdir_in(parent)
        .context("the installation directory is not writable; install the update manually")?;
    let stage = staging.path();
    let staged = match update.asset.kind {
        PackageKind::MacApp => {
            let output = Command::new("ditto")
                .args(["-x", "-k"])
                .arg(download)
                .arg(stage)
                .output()?;
            ensure!(output.status.success(), "could not unpack the update");
            let staged = stage.join("ZP.app");
            ensure!(
                staged.join("Contents/MacOS/zp").is_file(),
                "the update contains no ZP.app"
            );
            let old_signature = Command::new("codesign")
                .args(["-dv", "--verbose=4"])
                .arg(&target)
                .output()?;
            let details = String::from_utf8_lossy(&old_signature.stderr);
            let team = details.lines().find_map(|line| line.strip_prefix("TeamIdentifier="))
                .filter(|team| !team.is_empty() && *team != "not set")
                .context("this application has no Developer ID signature; install a signed release first")?;
            ensure!(
                team.bytes().all(|c| c.is_ascii_alphanumeric()),
                "invalid signing team"
            );
            // A leading `=` makes codesign read requirement text instead of a file path.
            let requirement = format!(
                "=anchor apple generic and identifier \"dev.zp.ZP\" and certificate leaf[subject.OU] = \"{team}\""
            );
            let verified = Command::new("codesign")
                .args(["--verify", "--deep", "--strict", "-R"])
                .arg(requirement)
                .arg(&staged)
                .output()?;
            ensure!(
                verified.status.success(),
                "the new application signature or signing identity is invalid"
            );
            let assessed = Command::new("spctl")
                .args(["--assess", "--type", "execute"])
                .arg(&staged)
                .output()?;
            ensure!(
                assessed.status.success(),
                "macOS could not validate the notarized application"
            );
            staged
        }
        PackageKind::WindowsPortable => {
            let script = stage.join("extract.ps1");
            File::create(&script)?
                .write_all(include_bytes!("../../../../packaging/updater/extract.ps1"))?;
            let result = powershell().arg(script).arg(download).arg(stage).output()?;
            ensure!(
                result.status.success(),
                "could not unpack the portable update"
            );
            let staged = stage.join("zp.exe");
            ensure!(staged.is_file(), "the portable package contains no zp.exe");
            staged
        }
        PackageKind::AppImage => {
            let staged = stage.join("zp");
            fs::copy(download, &staged)?;
            staged
        }
        PackageKind::Archive => {
            let staged = stage.join("zp");
            let file = File::create(&staged)?;
            let result = Command::new("tar")
                .arg("-xOf")
                .arg(download)
                .arg(format!("zp-{}/zp", update.version))
                .stdout(Stdio::from(file))
                .status()?;
            ensure!(result.success(), "could not unpack the update executable");
            staged
        }
        _ => anyhow::bail!("unsupported installation type"),
    };
    #[cfg(unix)]
    if update.asset.kind != PackageKind::MacApp {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }
    let mut command;
    if cfg!(windows) {
        let script = stage.join("replace.ps1");
        File::create(&script)?
            .write_all(include_bytes!("../../../../packaging/updater/replace.ps1"))?;
        command = powershell();
        command
            .arg(script)
            .arg(std::process::id().to_string())
            .arg(&target)
            .arg(&staged)
            .arg(stage);
    } else {
        let script = stage.join("replace.sh");
        File::create(&script)?
            .write_all(include_bytes!("../../../../packaging/updater/replace.sh"))?;
        let launch = if update.asset.kind == PackageKind::MacApp {
            target.join("Contents/MacOS/zp")
        } else {
            target.clone()
        };
        command = Command::new("sh");
        command
            .arg(script)
            .arg(std::process::id().to_string())
            .arg(&target)
            .arg(&staged)
            .arg(stage)
            .arg(launch);
    }
    Ok(Prepared { command, staging })
}

/// Windows PowerShell running a script file, without a console window: ZP is a GUI
/// program, so a console child would otherwise get a visible window of its own.
fn powershell() -> Command {
    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-WindowStyle",
        "Hidden",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn replacement(starts: bool, name: &str) {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join(format!("ZP with spaces '{name}'"));
        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let staging = tempfile::Builder::new()
            .prefix(".zp-update-")
            .tempdir_in(directory.path())
            .unwrap();
        let stage = staging.path();
        let staged = stage.join("zp");
        let content = if starts {
            "#!/bin/sh\nprintf ok > \"$ZP_UPDATE_ACK\"\n"
        } else {
            "#!/bin/sh\nexit 1\n"
        };
        fs::write(&staged, content).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        let script = stage.join("replace.sh");
        fs::write(
            &script,
            include_bytes!("../../../../packaging/updater/replace.sh"),
        )
        .unwrap();
        let mut old = Command::new("sleep").arg("0.1").spawn().unwrap();
        let mut helper = Command::new("sh")
            .arg(script)
            .arg(old.id().to_string())
            .arg(&target)
            .arg(&staged)
            .arg(stage)
            .arg(&target)
            .spawn()
            .unwrap();
        old.wait().unwrap();
        assert!(helper.wait().unwrap().success());
        if starts {
            assert_eq!(fs::read_to_string(&target).unwrap(), content);
            assert!(!stage.exists());
        } else {
            assert_eq!(fs::read_to_string(&target).unwrap(), "#!/bin/sh\nexit 0\n");
            assert!(stage.join("failed").exists());
        }
    }

    #[test]
    fn helper_keeps_new_application_only_after_startup_acknowledgement() {
        replacement(true, "success");
    }

    #[test]
    fn helper_restores_old_application_when_new_startup_fails() {
        replacement(false, "rollback");
    }
}
