//! Commit messages written by a model for what `git commit` would commit, in a
//! short-lived session that can only read the project.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, bail};
use futures::future::BoxFuture;
use serde_json::Value;
use zp_agent::{AgentTool, AgentToolResult, ToolContext};
use zp_ai::{CacheRetention, Model, Models, StopReason, StreamOptions, ThinkingLevel};
use zp_session::Session;

use crate::coding_session::{CodingSession, CodingSessionOptions};
use crate::compaction::CompactionSettings;
use crate::permissions::{
    Approver, Decision, PermissionGate, PermissionMode, PermissionPolicy, PermissionRequest,
};

/// What the model is asked; the Git buffer follows it in the same message.
pub const COMMIT_PROMPT: &str = "Read the contents of the current git buffer and write a \
                                commit message (conventional commit, single line, like \
                                `xx(xx): xx`). Reply with the commit message only: no \
                                quotes, no code fence, no other text.";

const SYSTEM_PROMPT: &str = "You write commit messages for the changes of a Git repository. \
                             The current git buffer shows the status, the recent commit \
                             subjects and the diff of what would be committed. You may read \
                             files of the project to understand a change, but you cannot \
                             change anything. Match the repository's commit style when its \
                             recent history has one.";

/// Longest output of each Git command sent; the model can read the files for more.
const OUTPUT_CHARS: usize = 40_000;
/// Recent commits shown for their style.
const RECENT_COMMITS: usize = 10;
/// Longest message kept.
const MAX_MESSAGE_CHARS: usize = 200;
/// Room for a tool call or a short answer.
const MAX_TOKENS: u64 = 1024;
/// Tool calls the model may make before it has to answer.
pub const MAX_READS: usize = 8;
/// How long the whole request may take.
pub const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Why no model writes commit messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoCommitModel {
    /// Nothing is configured and no model is usable.
    None,
    /// The configured model (`provider/model`) is unknown or has no credentials.
    Unavailable(String),
}

impl std::fmt::Display for NoCommitModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(
                f,
                "no model to write commit messages: set up a provider first"
            ),
            Self::Unavailable(spec) => write!(
                f,
                "the commit message model `{spec}` is unavailable: choose another in \
                 Settings \u{2192} General"
            ),
        }
    }
}

impl std::error::Error for NoCommitModel {}

/// The model that writes commit messages. `configured` (as `provider/model`) is used
/// when known and with credentials; it is never silently replaced, since the user chose
/// where the diff goes. Otherwise `fallback` (the default model).
pub fn commit_model(
    models: &Models,
    configured: Option<&str>,
    fallback: Option<&Model>,
) -> Result<Model, NoCommitModel> {
    let usable = |spec: &str| {
        models
            .find(spec)
            .filter(|model| models.resolve_auth(&model.provider).is_some())
    };
    match configured {
        Some(spec) => usable(spec).ok_or_else(|| NoCommitModel::Unavailable(spec.to_owned())),
        None => fallback.cloned().ok_or(NoCommitModel::None),
    }
}

/// What `git commit` would commit from `cwd` in the repository of `repo`, for the model:
/// the status, the recent commit subjects and the diff of the staged changes. When
/// nothing is staged, a commit takes the changed tracked files under `cwd` only, so the
/// status and the diff are limited to it. With `amend`, the last commit's message and
/// whatever is staged on top of it.
pub fn git_buffer(repo: &Path, cwd: &Path, amend: bool) -> String {
    let mut out = String::new();
    // `repo` is usually canonical (`repo_root` normalizes it) while `cwd` may still
    // contain symlinks, so both are compared canonically. A `cwd` outside the
    // repository would not narrow anything.
    let canonical_repo = repo.canonicalize().ok();
    let canonical_cwd = cwd.canonicalize().ok();
    let cwd = canonical_cwd
        .as_deref()
        .filter(|canonical| {
            canonical_repo
                .as_deref()
                .is_some_and(|repo| canonical.starts_with(repo))
        })
        .or_else(|| cwd.strip_prefix(repo).is_ok().then_some(cwd))
        .unwrap_or(repo);
    let staged = crate::git::git(repo, &["diff", "--cached", "--quiet"])
        .is_ok_and(|output| output.status.code() == Some(1));
    // Unstaged changes are not part of an amended commit: only the index is.
    let everything = staged || amend;
    if amend {
        section(
            &mut out,
            repo,
            &["log", "-1", "--format=%B"],
            "git log -1 --format=%B",
        );
    } else {
        let status = ["status", "--short", "--", "."];
        section(&mut out, cwd, &status, "git status --short");
        let count = RECENT_COMMITS.to_string();
        section(
            &mut out,
            repo,
            &["log", "--format=%s", "-n", &count],
            "git log --oneline",
        );
    }
    let diff = ["diff", "--no-color", "--no-ext-diff", "--no-textconv"];
    if everything {
        let mut args = diff.to_vec();
        args.push("--staged");
        section(&mut out, repo, &args, "git diff --staged");
    } else {
        // Run from `cwd` with a pathspec, so only its files are listed: `git diff` alone
        // shows the whole worktree whatever the current directory is.
        let mut args = diff.to_vec();
        args.extend(["--", "."]);
        section(&mut out, cwd, &args, "git diff");
    }
    out
}

/// Appends `$ name` and the command's output (cut to [`OUTPUT_CHARS`] characters) to
/// `out`; a command that cannot run or fails is left out.
fn section(out: &mut String, dir: &Path, args: &[&str], name: &str) {
    let Ok(output) = crate::git::git(dir, args) else {
        return;
    };
    if !output.status.success() {
        return;
    }
    out.push_str(&format!("\n$ {name}\n"));
    out.push_str(&excerpt(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        OUTPUT_CHARS,
    ));
    out.push('\n');
}

/// `text` cut to `limit` characters, marked when something was cut.
fn excerpt(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("\n… (truncated)");
    out
}

/// Asks `model` for a commit message for what `git commit` would commit from `cwd`, in
/// a one-off session with the read-only tools, which may only touch `cwd`. The model
/// gets at most [`MAX_READS`] tool calls and [`TIMEOUT`].
pub async fn generate_commit_message(
    models: Models,
    cwd: &Path,
    model: &Model,
    amend: bool,
) -> anyhow::Result<String> {
    let repo = crate::git::repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let buffer = git_buffer(&repo, cwd, amend);
    if buffer.trim().is_empty() {
        bail!("nothing to commit: the git buffer is empty");
    }
    let prompt = format!("{COMMIT_PROMPT}\n\nThe current git buffer:\n{buffer}");
    let mut options = CodingSessionOptions::new(models.clone(), cwd, Session::in_memory(cwd));
    options.model = Some(model.clone());
    options.thinking_level = Some(ThinkingLevel::Off);
    let reads = Arc::new(AtomicUsize::new(0));
    options.tools = Some(
        zp_tools::read_only_tools(cwd)
            .into_iter()
            .map(|tool| {
                Arc::new(LimitedReads {
                    tool,
                    reads: reads.clone(),
                }) as Arc<dyn AgentTool>
            })
            .collect(),
    );
    options.custom_system_prompt = Some(SYSTEM_PROMPT.to_owned());
    options.request_options = StreamOptions {
        max_tokens: Some(MAX_TOKENS),
        // A one-off request: a cache write would not be read again.
        cache_retention: CacheRetention::None,
        ..StreamOptions::default()
    };
    options.permissions = Some(Arc::new(PermissionGate::new(
        read_only_policy(cwd),
        Arc::new(DenyAll),
        models,
    )));
    // A one-off session: no project instructions, no compaction.
    options.trusted = false;
    options.compaction = CompactionSettings {
        enabled: false,
        ..CompactionSettings::default()
    };
    let coding = CodingSession::new(options)?;
    let added = tokio::time::timeout(TIMEOUT, coding.prompt(prompt))
        .await
        .map_err(|_| anyhow::anyhow!("writing the commit message took longer than {TIMEOUT:?}"))?
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let reply = added
        .iter()
        .rev()
        .find_map(|message| match message.as_llm() {
            Some(zp_ai::Message::Assistant(reply)) => Some(reply),
            _ => None,
        })
        .context("the model returned no answer")?;
    if matches!(reply.stop_reason, StopReason::Error | StopReason::Aborted) {
        bail!(
            "the commit message request failed: {}",
            reply.error_message.clone().unwrap_or_default()
        );
    }
    clean_commit_message(&reply.text()).context("the model returned an empty commit message")
}

/// Only the project may be read; anything else would need the user, who is not there.
fn read_only_policy(cwd: &Path) -> PermissionPolicy {
    PermissionPolicy {
        mode: PermissionMode::Ask,
        root: cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()),
        ..PermissionPolicy::default()
    }
}

/// Answers every question with no: the session runs on its own, without a user.
struct DenyAll;

impl Approver for DenyAll {
    fn approve(&self, _: PermissionRequest) -> BoxFuture<'static, Decision> {
        Box::pin(async {
            Decision::Deny(Some("This session can only read the project.".to_owned()))
        })
    }
}

/// A read-only tool that stops answering after [`MAX_READS`] calls in its session, so a
/// model that keeps reading cannot keep the Git panel waiting.
struct LimitedReads {
    tool: Arc<dyn AgentTool>,
    reads: Arc<AtomicUsize>,
}

impl AgentTool for LimitedReads {
    fn name(&self) -> &str {
        self.tool.name()
    }

    fn description(&self) -> &str {
        self.tool.description()
    }

    fn parameters(&self) -> Value {
        self.tool.parameters()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        self.tool.prompt_snippet()
    }

    fn prompt_guidelines(&self) -> &[&str] {
        self.tool.prompt_guidelines()
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let calls = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
            if calls > MAX_READS {
                return Ok(AgentToolResult::text(
                    "No more reads: write the commit message now.",
                ));
            }
            self.tool.execute(args, ctx).await
        })
    }
}

/// The message in the model's reply: the first line that is a conventional commit
/// (`type(scope): subject`), else the first line; without quotes, a code fence or a
/// `git commit -m "…"` around it.
pub fn clean_commit_message(raw: &str) -> Option<String> {
    let mut first = None;
    for line in raw.lines().map(str::trim) {
        let Some(message) = message_of(line) else {
            continue;
        };
        if is_conventional(&message) {
            return Some(message);
        }
        first.get_or_insert(message);
    }
    first
}

/// `line` as a commit message, when it has one (code fences and empty lines do not).
fn message_of(line: &str) -> Option<String> {
    if line.is_empty() || line.starts_with("```") {
        return None;
    }
    let line = match line.strip_prefix("git commit") {
        // The command, as some models answer; only its quoted message counts.
        Some(rest) => quoted(rest)?,
        None => line.to_owned(),
    };
    let line =
        line.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '\u{201c}' | '\u{201d}'));
    let line = line.trim_start_matches("Commit message:").trim();
    (!line.is_empty()).then(|| line.chars().take(MAX_MESSAGE_CHARS).collect())
}

/// Whether `message` has the conventional form `type(scope)!: subject`.
fn is_conventional(message: &str) -> bool {
    let Some((kind, subject)) = message.split_once(':') else {
        return false;
    };
    let kind = kind.trim().strip_suffix('!').unwrap_or(kind.trim());
    let kind = kind
        .strip_suffix(')')
        .and_then(|kind| kind.split_once('('))
        .map_or(kind, |(kind, _)| kind);
    !kind.is_empty() && kind.chars().all(|c| c.is_ascii_lowercase()) && !subject.trim().is_empty()
}

/// The first quoted part of `text`, if any.
fn quoted(text: &str) -> Option<String> {
    let inner = |quote: char| {
        let start = text.find(quote)?;
        let end = start + 1 + text[start + 1..].find(quote)?;
        Some(text[start + 1..end].to_owned())
    };
    inner('"').or_else(|| inner('\''))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use zp_ai::{FauxProvider, FauxResponse, Message, Tool};

    /// A repository with `a.txt` and `b.txt` committed.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        repo_at(dir.path());
        dir
    }

    fn repo_at(path: &Path) -> PathBuf {
        git(path, &["init", "-q"]);
        git(path, &["config", "user.name", "Tester"]);
        git(path, &["config", "user.email", "t@example.com"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        git(path, &["config", "core.autocrlf", "false"]);
        std::fs::write(path.join("a.txt"), "before\n").unwrap();
        std::fs::write(path.join("b.txt"), "other\n").unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "-q", "-m", "feat: base"]);
        path.to_path_buf()
    }

    /// A repository at `root` with `project/a.txt` and `sibling/b.txt` committed, both
    /// modified afterwards.
    fn repo_with_subprojects(root: &Path) -> PathBuf {
        for dir in ["project", "sibling"] {
            std::fs::create_dir(root.join(dir)).unwrap();
        }
        git(root, &["init", "-q"]);
        for (key, value) in [
            ("user.name", "Tester"),
            ("user.email", "t@example.com"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            git(root, &["config", key, value]);
        }
        std::fs::write(root.join("project/a.txt"), "before\n").unwrap();
        std::fs::write(root.join("sibling/b.txt"), "before\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "feat: base"]);
        std::fs::write(root.join("project/a.txt"), "after\n").unwrap();
        std::fs::write(root.join("sibling/b.txt"), "sibling change\n").unwrap();
        root.to_path_buf()
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
    }

    /// The text parts of a message.
    fn text_of(message: &Message) -> String {
        let parts = |content: &[zp_ai::UserContent]| {
            content
                .iter()
                .filter_map(zp_ai::UserContent::as_text)
                .collect::<Vec<_>>()
                .join("\n")
        };
        match message {
            Message::User(user) => parts(&user.content),
            Message::Assistant(assistant) => assistant.text(),
            Message::ToolResult(result) => parts(&result.content),
        }
    }

    #[test]
    fn cleans_messages() {
        assert_eq!(
            clean_commit_message("feat(git): add the star button\n").as_deref(),
            Some("feat(git): add the star button")
        );
        assert_eq!(
            clean_commit_message("```\nfix(parser): handle empty input\n```").as_deref(),
            Some("fix(parser): handle empty input")
        );
        assert_eq!(
            clean_commit_message("\"docs: update the readme\"").as_deref(),
            Some("docs: update the readme")
        );
        assert_eq!(
            clean_commit_message("git commit -m \"chore: bump deps\"").as_deref(),
            Some("chore: bump deps")
        );
        assert_eq!(
            clean_commit_message("Here you go:\n\nfeat!: breaking change").as_deref(),
            Some("feat!: breaking change"),
            "a conventional line wins over prose before it"
        );
        assert_eq!(
            clean_commit_message("Add star button").as_deref(),
            Some("Add star button"),
            "other styles are kept as they are"
        );
        assert_eq!(clean_commit_message(" \n```\n"), None);
        assert_eq!(clean_commit_message("git commit"), None);
    }

    #[test]
    fn buffer_shows_what_would_be_committed() {
        let dir = repo();
        let path = dir.path();
        std::fs::write(path.join("a.txt"), "after\n").unwrap();
        let unstaged = git_buffer(path, path, false);
        assert!(
            unstaged.contains("$ git status --short\n M a.txt"),
            "{unstaged}"
        );
        assert!(
            unstaged.contains("$ git log --oneline\nfeat: base"),
            "{unstaged}"
        );
        assert!(unstaged.contains("$ git diff"));
        assert!(unstaged.contains("+after"));

        git(path, &["add", "a.txt"]);
        std::fs::write(path.join("a.txt"), "later\n").unwrap();
        let staged = git_buffer(path, path, false);
        assert!(staged.contains("$ git diff --staged"), "{staged}");
        assert!(staged.contains("+after"));
        assert!(!staged.contains("+later"), "the unstaged edit is left out");

        let amend = git_buffer(path, path, true);
        assert!(
            amend.contains("$ git log -1 --format=%B\nfeat: base"),
            "{amend}"
        );
        assert!(!amend.contains("$ git status"), "{amend}");
        assert!(amend.contains("+after"));
        assert!(!amend.contains("+later"));
    }

    #[test]
    fn the_configured_model_is_used_or_reported() {
        let faux = FauxProvider::default();
        let mut models = Models::new();
        models.set_provider(Arc::new(faux.clone()));
        let fallback = faux.model();
        assert_eq!(
            commit_model(&models, None, Some(&fallback)),
            Ok(fallback.clone())
        );
        assert_eq!(
            commit_model(&models, Some("faux/faux-1"), None),
            Ok(faux.model())
        );
        assert_eq!(
            commit_model(&models, Some("nope/nothing"), Some(&fallback)),
            Err(NoCommitModel::Unavailable("nope/nothing".into())),
            "a configured model is never silently replaced"
        );
        assert_eq!(commit_model(&models, None, None), Err(NoCommitModel::None));
    }

    #[tokio::test]
    async fn writes_a_message_from_the_buffer_with_read_only_tools() {
        let dir = repo();
        let path = dir.path();
        std::fs::write(path.join("a.txt"), "hello\n").unwrap();
        let faux = FauxProvider::default();
        faux.push(FauxResponse::text("feat: add a\n"));
        let mut models = Models::new();
        models.set_provider(Arc::new(faux.clone()));
        let message = generate_commit_message(models, path, &faux.model(), false)
            .await
            .unwrap();
        assert_eq!(message, "feat: add a");
        let requests = faux.requests();
        assert_eq!(requests.len(), 1);
        let tools: Vec<String> = requests[0]
            .tools
            .iter()
            .map(|tool: &Tool| tool.name.clone())
            .collect();
        assert_eq!(tools, ["read", "grep", "find", "ls"]);
        assert!(
            requests[0]
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("commit messages"))
        );
        let sent = text_of(&requests[0].messages[0]);
        assert!(sent.starts_with(COMMIT_PROMPT), "{sent}");
        assert!(
            sent.contains("$ git diff") && sent.contains("+hello"),
            "{sent}"
        );
    }

    #[tokio::test]
    async fn reading_outside_the_project_is_denied() {
        // A sibling of the project, under the same temporary root.
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let _repo = repo_at(&path);
        std::fs::write(path.join("a.txt"), "hello\n").unwrap();
        let outside = root.path().join("outside.txt");
        std::fs::write(&outside, "secret\n").unwrap();
        let faux = FauxProvider::default();
        faux.push(FauxResponse::tool_call(
            "read",
            serde_json::json!({"path": outside}),
        ));
        faux.push(FauxResponse::text("feat: add a"));
        let mut models = Models::new();
        models.set_provider(Arc::new(faux.clone()));
        let message = generate_commit_message(models, &path, &faux.model(), false)
            .await
            .unwrap();
        assert_eq!(message, "feat: add a");
        let requests = faux.requests();
        assert_eq!(requests.len(), 2);
        let shown = text_of(&requests[1].messages[2]);
        assert!(shown.contains("denied"), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
    }

    #[tokio::test]
    async fn a_model_that_keeps_reading_is_cut_off() {
        let dir = repo();
        let path = dir.path();
        std::fs::write(path.join("a.txt"), "hello\n").unwrap();
        let faux = FauxProvider::default();
        for _ in 0..MAX_READS {
            faux.push(FauxResponse::tool_call(
                "read",
                serde_json::json!({"path": "a.txt"}),
            ));
        }
        faux.push(FauxResponse::tool_call(
            "read",
            serde_json::json!({"path": "a.txt"}),
        ));
        faux.push(FauxResponse::text("feat: add a"));
        let mut models = Models::new();
        models.set_provider(Arc::new(faux.clone()));
        let message = generate_commit_message(models, path, &faux.model(), false)
            .await
            .unwrap();
        assert_eq!(message, "feat: add a");
        let requests = faux.requests();
        let last = text_of(requests.last().unwrap().messages.last().unwrap());
        assert!(last.contains("No more reads"), "{last}");
    }

    #[tokio::test]
    async fn only_the_project_directory_is_described_when_nothing_is_staged() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_subprojects(root.path());
        let project = root.path().join("project");
        let buffer = git_buffer(&repo, &project, false);
        assert!(
            buffer.contains(" M a.txt"),
            "paths are relative to the project"
        );
        assert!(buffer.contains("+after"), "{buffer}");
        assert!(!buffer.contains("sibling"), "{buffer}");
        // From the repository root itself, everything is described.
        let buffer = git_buffer(&repo, &repo, false);
        assert!(
            buffer.contains("+after") && buffer.contains("sibling change"),
            "{buffer}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_symlinked_project_directory_still_narrows_the_diff() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        repo_with_subprojects(&repo);
        // A link outside the repository requires canonical comparison to narrow the diff.
        let link = root.path().join("link");
        std::os::unix::fs::symlink(repo.join("project"), &link).unwrap();
        let linked_repo = crate::git::repo_root(&link).unwrap();
        assert_eq!(linked_repo, repo.canonicalize().unwrap());
        let buffer = git_buffer(&linked_repo, &link, false);
        assert!(buffer.contains(" M a.txt"), "{buffer}");
        assert!(buffer.contains("+after"), "{buffer}");
        assert!(!buffer.contains("sibling"), "{buffer}");
    }

    #[tokio::test]
    async fn an_empty_reply_is_an_error() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let faux = FauxProvider::default();
        faux.push(FauxResponse::text(" \n"));
        let mut models = Models::new();
        models.set_provider(Arc::new(faux.clone()));
        let error = generate_commit_message(models, dir.path(), &faux.model(), false)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("empty commit message"),
            "{error:#}"
        );
    }
}
