//! Project trust on the terminal, shared with the app.
//!
//! A project's instruction files, `.zp` settings, MCP servers, prompts and skills are only
//! used once the user has said they trust it: in the app, with `--trust`, or by answering
//! the question asked here on a terminal. Without an answer (no terminal, or the user
//! declined), they are ignored and a notice says so.

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use zp_core::app_state::AppState;

use crate::run::Style;

/// How the trust decision is reached when the user has not given one yet.
pub(crate) enum Ask<'a> {
    /// Put the question to the user (a terminal); `Some` is their answer.
    Interactive(&'a mut dyn FnMut(&[PathBuf]) -> Option<bool>),
    /// Nobody to ask: the project's files are ignored.
    Never,
}

/// What [`decide`] concluded and whether the user should be told.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) trusted: bool,
    /// The project's files that are being ignored (empty when trusted or nothing to trust).
    pub(crate) ignored: Vec<PathBuf>,
    /// The user declined earlier (the notice points at `--trust`).
    pub(crate) declined: bool,
}

/// Whether to use the project at `cwd`, remembering new answers in `state_file`.
pub(crate) fn decide(
    state_file: Option<&Path>,
    cwd: &Path,
    trust_flag: bool,
    ask: Ask<'_>,
) -> Decision {
    let mut state = state_file.map(AppState::load).unwrap_or_default();
    let remember = |state: &mut AppState, trusted: bool| {
        state.project_mut(cwd).trusted = Some(trusted);
        if let Some(file) = state_file
            && let Err(error) = state.save(file)
        {
            eprintln!("warning: could not remember the project's trust: {error:#}");
        }
    };
    let trusted = |trusted| Decision {
        trusted,
        ignored: Vec::new(),
        declined: false,
    };
    if trust_flag {
        if !state.is_trusted(cwd) {
            remember(&mut state, true);
        }
        return trusted(true);
    }
    if state.is_trusted(cwd) {
        return trusted(true);
    }
    let files = zp_core::project::trust_files(cwd);
    if files.is_empty() {
        return trusted(false);
    }
    let declined = state.project(cwd).trusted == Some(false);
    if !declined
        && let Ask::Interactive(ask) = ask
        && let Some(answer) = ask(&files)
    {
        remember(&mut state, answer);
        if answer {
            return trusted(true);
        }
        return Decision {
            trusted: false,
            ignored: files,
            declined: true,
        };
    }
    Decision {
        trusted: false,
        ignored: files,
        declined,
    }
}

/// [`decide`] on the terminal: asks when stdin and stderr are terminals, else (or after a
/// "no") prints which files are ignored. `None` `data_dir`: nothing is remembered.
pub(crate) fn resolve(data_dir: Option<&Path>, cwd: &Path, trust_flag: bool) -> bool {
    let style = Style::detect();
    let state_file = data_dir.map(zp_core::paths::state_file);
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let mut ask_terminal = |files: &[PathBuf]| ask_on_terminal(cwd, files, style);
    let ask = if interactive {
        Ask::Interactive(&mut ask_terminal)
    } else {
        Ask::Never
    };
    let decision = decide(state_file.as_deref(), cwd, trust_flag, ask);
    if !decision.ignored.is_empty() {
        let why = if decision.declined {
            "you did not trust this project"
        } else {
            "this project is not trusted"
        };
        eprintln!(
            "{}",
            style.paint(
                "33",
                &format!(
                    "Ignoring {} ({why}). Run with --trust if you trust its authors.",
                    shown(cwd, &decision.ignored).join(", ")
                )
            )
        );
    }
    decision.trusted
}

/// The same question as the app's, answered with y/n. `None` when stdin closes.
fn ask_on_terminal(cwd: &Path, files: &[PathBuf], style: Style) -> Option<bool> {
    eprintln!(
        "{}",
        style.paint(
            "1;33",
            "This folder has instructions or settings for AI agents:"
        )
    );
    for file in shown(cwd, files) {
        eprintln!("  {file}");
    }
    eprintln!(
        "A trusted project's files can tell the agent what to do, change its permissions \
         and start MCP servers. If you don't trust the folder's authors, ZP ignores them."
    );
    eprint!("Trust this project? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(matches!(line.trim().to_lowercase().as_str(), "y" | "yes")),
    }
}

/// Paths relative to the project where possible.
fn shown(cwd: &Path, files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|file| match file.strip_prefix(cwd) {
            Ok(relative) => relative.display().to_string(),
            Err(_) => zp_core::project::tilde_path(file),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        project: PathBuf,
        state: PathBuf,
    }

    /// A project with an MCP config (something to trust) and an empty data directory.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".zp")).unwrap();
        std::fs::write(project.join(".zp/mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
        let state = dir.path().join("state.json");
        Fixture {
            _dir: dir,
            project,
            state,
        }
    }

    fn saved(fixture: &Fixture) -> Option<bool> {
        AppState::load(&fixture.state)
            .project(&fixture.project)
            .trusted
    }

    #[test]
    fn unknown_projects_are_not_trusted_without_an_answer() {
        let fixture = fixture();
        let decision = decide(Some(&fixture.state), &fixture.project, false, Ask::Never);
        assert!(!decision.trusted);
        assert_eq!(decision.ignored, [fixture.project.join(".zp/mcp.json")]);
        assert!(!decision.declined);
        assert_eq!(saved(&fixture), None, "not asked, so nothing is remembered");
    }

    #[test]
    fn the_terminal_answer_is_remembered_like_the_apps() {
        let fixture = fixture();
        let mut asked = 0;
        let mut yes = |files: &[PathBuf]| {
            asked += 1;
            assert_eq!(files.len(), 1);
            Some(true)
        };
        let decision = decide(
            Some(&fixture.state),
            &fixture.project,
            false,
            Ask::Interactive(&mut yes),
        );
        assert!(decision.trusted && decision.ignored.is_empty());
        assert_eq!(saved(&fixture), Some(true));
        // Trusted from now on, in the CLI and the app, without asking again.
        let mut never = |_: &[PathBuf]| -> Option<bool> { panic!("asked again") };
        let again = decide(
            Some(&fixture.state),
            &fixture.project,
            false,
            Ask::Interactive(&mut never),
        );
        assert!(again.trusted);
        assert_eq!(asked, 1);
    }

    #[test]
    fn a_declined_project_is_not_asked_again_until_trust_is_given() {
        let fixture = fixture();
        let mut no = |_: &[PathBuf]| Some(false);
        let decision = decide(
            Some(&fixture.state),
            &fixture.project,
            false,
            Ask::Interactive(&mut no),
        );
        assert!(!decision.trusted && decision.declined);
        assert_eq!(saved(&fixture), Some(false));
        let mut never = |_: &[PathBuf]| -> Option<bool> { panic!("asked again") };
        let again = decide(
            Some(&fixture.state),
            &fixture.project,
            false,
            Ask::Interactive(&mut never),
        );
        assert!(!again.trusted && again.declined);

        let trusted = decide(Some(&fixture.state), &fixture.project, true, Ask::Never);
        assert!(trusted.trusted);
        assert_eq!(saved(&fixture), Some(true), "--trust is remembered");
    }

    #[test]
    fn a_closed_stdin_leaves_the_question_open() {
        let fixture = fixture();
        let mut closed = |_: &[PathBuf]| None;
        let decision = decide(
            Some(&fixture.state),
            &fixture.project,
            false,
            Ask::Interactive(&mut closed),
        );
        assert!(!decision.trusted && !decision.declined);
        assert_eq!(saved(&fixture), None);
    }

    #[test]
    fn projects_without_anything_to_trust_need_no_answer() {
        let dir = tempfile::tempdir().unwrap();
        let mut never = |_: &[PathBuf]| -> Option<bool> { panic!("asked") };
        let decision = decide(None, dir.path(), false, Ask::Interactive(&mut never));
        assert!(!decision.trusted && decision.ignored.is_empty());
    }
}
