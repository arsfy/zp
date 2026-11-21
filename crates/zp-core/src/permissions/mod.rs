//! Permissions for tool calls: which calls run on their own and which need the user.
//!
//! A [`PermissionGate`] sits in the agent's `before_tool_call` hook. For each call it
//! [evaluates](PermissionPolicy::evaluate) the [`PermissionMode`], the project scope and
//! the grants made for the session; calls that need approval go to an [`Approver`] (the
//! GUI's approval card, a terminal prompt), and in auto-review mode first to a review
//! model.

pub mod review;
pub mod shell;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zp_agent::{AgentMessage, ToolCallDecision};
use zp_ai::{Message, Model, Models, UserContent};

/// How much the agent may do without asking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Read-only commands run automatically; other commands and file changes need approval.
    #[default]
    Ask,
    /// File changes, including workspace-local removals, and routine commands run automatically.
    AutoEdit,
    /// Like `AutoEdit`, with a model reviewing other commands that do not require the user.
    AutoReview,
    /// Everything runs.
    FullAuto,
}

impl PermissionMode {
    pub const ALL: [Self; 4] = [Self::Ask, Self::AutoEdit, Self::AutoReview, Self::FullAuto];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask every time",
            Self::AutoEdit => "Auto-approve edits",
            Self::AutoReview => "Auto-review",
            Self::FullAuto => "Full auto",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Ask => {
                "Approve commands and file changes; read-only commands and echo output run automatically"
            }
            Self::AutoEdit => {
                "Auto-approve file changes, echo writes and non-destructive Git operations"
            }
            Self::AutoReview => {
                "Auto-approve file changes; a review model checks other commands; destructive ones need you"
            }
            Self::FullAuto => "Everything runs without asking",
        }
    }

    /// The command line spelling: `ask`, `auto-edit`, `auto-review`, `full-auto`.
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AutoEdit => "auto-edit",
            Self::AutoReview => "auto-review",
            Self::FullAuto => "full-auto",
        }
    }

    pub fn from_cli_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.cli_name() == name)
    }
}

/// What a tool can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    /// Reads files (`read`, `grep`, `find`, `ls`).
    Read,
    /// Changes files (`write`, `edit`).
    Edit,
    /// Runs commands (`bash`).
    Execute,
    /// Talks to the user or tracks progress (`ask`, `todo`); never needs approval.
    Interaction,
    /// Anything else (extensions).
    Other,
}

impl ToolKind {
    pub fn of(tool: &str) -> Self {
        match tool {
            "read" | "grep" | "find" | "ls" => Self::Read,
            "write" | "edit" => Self::Edit,
            "bash" => Self::Execute,
            "ask" | "todo" => Self::Interaction,
            _ => Self::Other,
        }
    }
}

/// Why a call needs the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskReason {
    /// The permission mode asks for this kind of call.
    Mode(PermissionMode),
    /// It accesses files outside the project.
    OutsideProject(Vec<PathBuf>),
    /// A destructive or high-risk command.
    Dangerous(String),
    /// The review model did not approve it.
    Review(String),
    /// The delegation policy asks before sending a task to a child model.
    Delegation,
}

impl AskReason {
    pub fn describe(&self) -> String {
        match self {
            Self::Mode(mode) => format!("{} is on", mode.label()),
            Self::OutsideProject(paths) => format!(
                "Outside the project: {}",
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Dangerous(why) => format!("Potentially destructive: {why}"),
            Self::Review(why) => format!("Review model: {why}"),
            Self::Delegation => {
                "The task and the files the subagent reads are sent to this model".to_owned()
            }
        }
    }
}

/// What "approve for session" allows from then on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SessionGrant {
    /// Every file change in the project.
    Edits,
    /// Commands starting with this prefix (`cargo test`).
    Command(String),
    /// Files in this directory (outside the project).
    Directory(PathBuf),
    /// Every call of this tool.
    Tool(String),
}

impl SessionGrant {
    pub fn describe(&self) -> String {
        match self {
            Self::Edits => "file changes".to_owned(),
            Self::Command(prefix) => format!("`{prefix}`"),
            Self::Directory(dir) => format!("files in {}", dir.display()),
            Self::Tool(tool) => format!("the {tool} tool"),
        }
    }
}

/// A tool call waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub kind: ToolKind,
    /// One line describing the call (`bash cargo test`, `edit src/main.rs`).
    pub summary: String,
    /// The validated arguments.
    pub args: Value,
    /// The full command of a `bash` call.
    pub command: Option<String>,
    /// Where a delegated call sends its task: the model and its endpoint.
    pub destination: Option<String>,
    /// What the call would do in plain words, from the review model that handed it to
    /// the user.
    pub description: Option<String>,
    pub reasons: Vec<AskReason>,
    /// What "approve for session" would grant; empty when it is not offered (destructive
    /// commands).
    pub grants: Vec<SessionGrant>,
}

/// The user's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Allow, and the request's grants for the rest of the session.
    AllowForSession,
    /// Deny, with a reason for the model.
    Deny(Option<String>),
}

/// Asks the user about tool calls.
pub trait Approver: Send + Sync {
    /// Resolves when the user answers (dropping the request counts as a denial).
    fn approve(&self, request: PermissionRequest) -> BoxFuture<'static, Decision>;

    /// The review model started (`true`) or stopped (`false`) looking at call
    /// `tool_call_id`, which waits meanwhile; the user can be shown it is not running yet.
    fn reviewing(&self, _tool_call_id: &str, _active: bool) {}
}

/// Tells the approver a review stopped, however it ended (also when cancelled).
struct Reviewing<'a> {
    approver: &'a dyn Approver,
    tool_call_id: &'a str,
}

impl<'a> Reviewing<'a> {
    fn start(approver: &'a dyn Approver, tool_call_id: &'a str) -> Self {
        approver.reviewing(tool_call_id, true);
        Self {
            approver,
            tool_call_id,
        }
    }
}

impl Drop for Reviewing<'_> {
    fn drop(&mut self) {
        self.approver.reviewing(self.tool_call_id, false);
    }
}

/// The rules for a project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionPolicy {
    pub mode: PermissionMode,
    /// The project directory (canonical).
    pub root: PathBuf,
    /// Directories outside the project the tools may use without asking.
    pub additional_directories: Vec<PathBuf>,
    /// Command prefixes that run without asking (unless destructive).
    pub allow_commands: Vec<String>,
    /// The review model (`provider/model`); `None` picks a cheap model of the agent's
    /// provider.
    pub review_model: Option<String>,
    /// How much the review model reasons; clamped to what the model supports.
    pub review_thinking: zp_ai::ThinkingLevel,
    /// Temporary directories (canonical) the tools may use like the project: file tools
    /// work there without asking, and removals there count as edits.
    pub temp_directories: Vec<PathBuf>,
}

/// Grants made with "approve for session".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grants {
    pub edits: bool,
    pub commands: Vec<String>,
    pub directories: Vec<PathBuf>,
    pub tools: HashSet<String>,
}

impl Grants {
    pub fn add(&mut self, grant: &SessionGrant) {
        match grant {
            SessionGrant::Edits => self.edits = true,
            SessionGrant::Command(prefix) => {
                if !self.commands.contains(prefix) {
                    self.commands.push(prefix.clone());
                }
            }
            SessionGrant::Directory(dir) => {
                if !self.directories.contains(dir) {
                    self.directories.push(dir.clone());
                }
            }
            SessionGrant::Tool(tool) => {
                self.tools.insert(tool.clone());
            }
        }
    }
}

/// The verdict on a call before anyone is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Evaluation {
    Allow,
    /// Ask the review model (auto-review mode); `grants` apply if it escalates.
    Review {
        grants: Vec<SessionGrant>,
    },
    Ask {
        reasons: Vec<AskReason>,
        grants: Vec<SessionGrant>,
    },
}

/// The canonical form of `path`: symlinks of its longest existing ancestor resolved.
fn canonical(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut out = canonical;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The files a call reads or writes (for scope checks).
pub fn tool_paths(tool: &str, args: &Value, cwd: &Path) -> Vec<PathBuf> {
    let path = |default: Option<&str>| {
        args["path"]
            .as_str()
            .or(default)
            .map(|p| canonical(&zp_tools::resolve_path(cwd, p)))
    };
    match tool {
        "read" | "write" | "edit" => path(None).into_iter().collect(),
        "ls" | "grep" | "find" => path(Some(".")).into_iter().collect(),
        _ => Vec::new(),
    }
}

impl PermissionPolicy {
    /// Whether `path` (canonical) is inside the project or an allowed directory.
    pub fn in_scope(&self, path: &Path, grants: &Grants) -> bool {
        path.starts_with(&self.root)
            || self
                .additional_directories
                .iter()
                .chain(&self.temp_directories)
                .chain(&grants.directories)
                .any(|dir| path.starts_with(dir))
    }

    /// Decides whether a call may run, needs review or needs the user.
    pub fn evaluate(&self, tool: &str, args: &Value, grants: &Grants) -> Evaluation {
        let kind = ToolKind::of(tool);
        let mut reasons = Vec::new();
        let mut offer = Vec::new();
        let mut review = false;

        if self.mode != PermissionMode::FullAuto {
            let outside: Vec<PathBuf> = tool_paths(tool, args, &self.root)
                .into_iter()
                .filter(|path| !self.in_scope(path, grants))
                .collect();
            if !outside.is_empty() {
                for path in &outside {
                    let dir = if kind == ToolKind::Read && path.is_dir() {
                        path.clone()
                    } else {
                        path.parent()
                            .map_or_else(|| path.clone(), Path::to_path_buf)
                    };
                    offer.push(SessionGrant::Directory(dir));
                }
                reasons.push(AskReason::OutsideProject(outside));
            }
        }

        match kind {
            ToolKind::Read | ToolKind::Interaction => {}
            ToolKind::Edit => {
                if self.mode == PermissionMode::Ask && !grants.edits {
                    reasons.push(AskReason::Mode(self.mode));
                    offer.push(SessionGrant::Edits);
                }
            }
            ToolKind::Execute => {
                let command = args["command"].as_str().unwrap_or_default();
                if self.mode != PermissionMode::FullAuto {
                    let edits = matches!(
                        self.mode,
                        PermissionMode::AutoEdit | PermissionMode::AutoReview
                    );
                    let mut allowed: Vec<String> = self
                        .allow_commands
                        .iter()
                        .chain(&grants.commands)
                        .cloned()
                        .collect();
                    if edits {
                        allowed.extend(["git", "echo", "python", "python3"].map(str::to_owned));
                    }
                    let removable: Vec<&Path> = if edits {
                        std::iter::once(self.root.as_path())
                            .chain(self.temp_directories.iter().map(PathBuf::as_path))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    match shell::assess(command, &allowed, &removable) {
                        shell::CommandAssessment::Allowed => {}
                        shell::CommandAssessment::Dangerous(why) => {
                            reasons.push(AskReason::Dangerous(why));
                            // Never approved wholesale.
                            return Evaluation::Ask {
                                reasons,
                                grants: Vec::new(),
                            };
                        }
                        shell::CommandAssessment::NeedsApproval(prefixes) => {
                            offer.extend(prefixes.into_iter().map(SessionGrant::Command));
                            match self.mode {
                                PermissionMode::AutoReview => review = true,
                                _ => reasons.push(AskReason::Mode(self.mode)),
                            }
                        }
                    }
                }
            }
            ToolKind::Other => {
                if self.mode != PermissionMode::FullAuto && !grants.tools.contains(tool) {
                    offer.push(SessionGrant::Tool(tool.to_owned()));
                    match self.mode {
                        PermissionMode::AutoReview => review = true,
                        _ => reasons.push(AskReason::Mode(self.mode)),
                    }
                }
            }
        }

        if !reasons.is_empty() {
            Evaluation::Ask {
                reasons,
                grants: offer,
            }
        } else if review {
            Evaluation::Review { grants: offer }
        } else {
            Evaluation::Allow
        }
    }
}

/// Applies a [`PermissionPolicy`] to tool calls, asking an [`Approver`] when needed.
pub struct PermissionGate {
    policy: RwLock<PermissionPolicy>,
    grants: Mutex<Grants>,
    approver: Arc<dyn Approver>,
    models: Models,
    /// Calls are checked concurrently; the user is asked about one at a time.
    turns: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for PermissionGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionGate")
            .field("policy", &self.policy())
            .finish_non_exhaustive()
    }
}

impl PermissionGate {
    /// A gate with `policy`; `models` run reviews.
    pub fn new(policy: PermissionPolicy, approver: Arc<dyn Approver>, models: Models) -> Self {
        Self {
            policy: RwLock::new(policy),
            grants: Mutex::default(),
            approver,
            models,
            turns: tokio::sync::Mutex::new(()),
        }
    }

    pub fn policy(&self) -> PermissionPolicy {
        self.policy
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the rules (e.g. the user changed the mode); grants are kept.
    pub fn set_policy(&self, policy: PermissionPolicy) {
        *self.policy.write().unwrap_or_else(PoisonError::into_inner) = policy;
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        self.policy
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .mode = mode;
    }

    pub fn grants(&self) -> Grants {
        self.grants
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Decides about one call: allowed, reviewed, or answered by the user.
    pub async fn check(
        &self,
        call: &zp_ai::ToolCall,
        args: &Value,
        agent_model: Option<&Model>,
        messages: &[AgentMessage],
        cancel: &CancellationToken,
    ) -> ToolCallDecision {
        let policy = self.policy();
        let current_grants = self.grants();
        let evaluation = policy.evaluate(&call.name, args, &current_grants);
        let (reasons, grants, description) = match evaluation {
            Evaluation::Allow => return ToolCallDecision::Allow,
            Evaluation::Ask { reasons, grants } => (reasons, grants, None),
            Evaluation::Review { grants } => {
                let model = policy
                    .review_model
                    .as_deref()
                    .and_then(|spec| self.models.find(spec))
                    .or_else(|| {
                        agent_model.map(|model| crate::title::title_model(&self.models, model))
                    });
                let intent = last_user_text(messages);
                let _reviewing = Reviewing::start(self.approver.as_ref(), &call.id);
                let verdict = match model {
                    Some(model) => {
                        let review = review::review_tool_call(
                            &self.models,
                            &model,
                            &policy,
                            &current_grants,
                            intent.as_deref(),
                            &call.name,
                            args,
                        );
                        tokio::select! {
                            verdict = review => verdict,
                            () = cancel.cancelled() => return aborted(),
                        }
                    }
                    None => Err(anyhow::anyhow!("no review model is available")),
                };
                match verdict {
                    Ok(review::Verdict::Allow(_)) => return ToolCallDecision::Allow,
                    Ok(review::Verdict::Ask { reason, summary }) => {
                        (vec![AskReason::Review(reason)], grants, summary)
                    }
                    Err(error) => (
                        vec![AskReason::Review(format!("review failed ({error:#})"))],
                        grants,
                        None,
                    ),
                }
            }
        };
        let request = PermissionRequest {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            kind: ToolKind::of(&call.name),
            summary: zp_tools::describe_call(&call.name, args, 200),
            args: args.clone(),
            command: args["command"]
                .as_str()
                .filter(|_| call.name == "bash")
                .map(str::to_owned),
            destination: None,
            description,
            reasons,
            grants: grants.clone(),
        };
        let Some(_turn) = self.turn(cancel).await else {
            return aborted();
        };
        // A session grant given while this call waited for its turn may cover it now.
        let policy = self.policy();
        if matches!(
            policy.evaluate(&call.name, args, &self.grants()),
            Evaluation::Allow
        ) {
            return ToolCallDecision::Allow;
        }
        self.prompt(request, cancel).await
    }

    /// Asks the user about `request`, after any question already waiting.
    pub(crate) async fn ask(
        &self,
        request: PermissionRequest,
        cancel: &CancellationToken,
    ) -> ToolCallDecision {
        let Some(_turn) = self.turn(cancel).await else {
            return aborted();
        };
        self.prompt(request, cancel).await
    }

    /// Waits until no other question is put to the user; `None` when cancelled first.
    async fn turn(&self, cancel: &CancellationToken) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        tokio::select! {
            turn = self.turns.lock() => Some(turn),
            () = cancel.cancelled() => None,
        }
    }

    async fn prompt(
        &self,
        request: PermissionRequest,
        cancel: &CancellationToken,
    ) -> ToolCallDecision {
        let grants = request.grants.clone();
        let decision = tokio::select! {
            decision = self.approver.approve(request) => decision,
            () = cancel.cancelled() => return aborted(),
        };
        match decision {
            Decision::Allow => ToolCallDecision::Allow,
            Decision::AllowForSession => {
                let mut current = self.grants.lock().unwrap_or_else(PoisonError::into_inner);
                for grant in &grants {
                    current.add(grant);
                }
                ToolCallDecision::Allow
            }
            Decision::Deny(reason) => ToolCallDecision::Block {
                reason: Some(match reason.filter(|r| !r.trim().is_empty()) {
                    Some(reason) => format!(
                        "The user denied this tool call. Their reason: {}",
                        reason.trim()
                    ),
                    None => "The user denied this tool call.".to_owned(),
                }),
                terminate: false,
            },
        }
    }
}

fn aborted() -> ToolCallDecision {
    ToolCallDecision::Block {
        reason: Some("Operation aborted".into()),
        terminate: true,
    }
}

/// The text of the last user message (what the agent is working on).
fn last_user_text(messages: &[AgentMessage]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        AgentMessage::Llm(Message::User(user)) => Some(
            user.content
                .iter()
                .filter_map(UserContent::as_text)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(mode: PermissionMode, root: &Path) -> PermissionPolicy {
        PermissionPolicy {
            mode,
            root: root.to_path_buf(),
            allow_commands: vec!["git status".into()],
            ..PermissionPolicy::default()
        }
    }

    #[test]
    fn temporary_directories_are_scratch_space_like_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("project");
        let temp = base.join("scratch");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir_all(temp.join("build")).unwrap();
        let none = Grants::default();
        let mut policy = policy(PermissionMode::AutoEdit, &root);
        policy.temp_directories = vec![temp.clone()];
        let write = |path: &Path| json!({"path": path, "content": "x"});
        assert_eq!(
            policy.evaluate("write", &write(&temp.join("notes.txt")), &none),
            Evaluation::Allow
        );
        assert_eq!(
            policy.evaluate("read", &json!({"path": temp.join("build")}), &none),
            Evaluation::Allow
        );
        let outside = base.join("x.txt");
        assert!(matches!(
            policy.evaluate("write", &write(&outside), &none),
            Evaluation::Ask { .. }
        ));
        // Use project-relative Bash paths so the Windows check can resolve them too.
        assert_eq!(
            policy.evaluate(
                "bash",
                &json!({"command": "rm -rf ../scratch/build"}),
                &none,
            ),
            Evaluation::Allow,
            "removing inside a temporary directory counts as an edit"
        );
        assert!(
            matches!(
                policy.evaluate("bash", &json!({"command": "rm -rf ../scratch"}), &none),
                Evaluation::Ask { .. }
            ),
            "never the directory itself"
        );
        policy.temp_directories.clear();
        assert!(matches!(
            policy.evaluate("write", &write(&temp.join("notes.txt")), &none),
            Evaluation::Ask { .. }
        ));
    }

    fn ask(evaluation: Evaluation) -> (Vec<AskReason>, Vec<SessionGrant>) {
        match evaluation {
            Evaluation::Ask { reasons, grants } => (reasons, grants),
            other => panic!("expected ask, got {other:?}"),
        }
    }

    #[test]
    fn modes_scope_and_grants() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path()).join("project");
        std::fs::create_dir_all(&root).unwrap();
        let none = Grants::default();
        let edit = json!({"path": "src/main.rs", "edits": []});
        let test = json!({"command": "cargo test && git status"});
        let read_outside = json!({"path": "../secrets.txt"});

        let ask_mode = policy(PermissionMode::Ask, &root);
        assert_eq!(
            ask_mode.evaluate("read", &json!({"path": "a"}), &none),
            Evaluation::Allow
        );
        assert_eq!(
            ask(ask_mode.evaluate("edit", &edit, &none)),
            (
                vec![AskReason::Mode(PermissionMode::Ask)],
                vec![SessionGrant::Edits]
            )
        );
        assert_eq!(
            ask(ask_mode.evaluate("bash", &test, &none)).1,
            [SessionGrant::Command("cargo test".into())]
        );
        let (reasons, grants) = ask(ask_mode.evaluate("read", &read_outside, &none));
        assert_eq!(
            reasons,
            [AskReason::OutsideProject(vec![
                canonical(dir.path()).join("secrets.txt")
            ])]
        );
        assert_eq!(grants, [SessionGrant::Directory(canonical(dir.path()))]);
        assert!(matches!(
            ask_mode.evaluate("mcp_search", &json!({}), &none),
            Evaluation::Ask { .. }
        ));

        // Grants cover later calls.
        let mut grants = Grants::default();
        grants.add(&SessionGrant::Edits);
        grants.add(&SessionGrant::Command("cargo test".into()));
        grants.add(&SessionGrant::Directory(canonical(dir.path())));
        assert_eq!(ask_mode.evaluate("edit", &edit, &grants), Evaluation::Allow);
        assert_eq!(ask_mode.evaluate("bash", &test, &grants), Evaluation::Allow);
        assert_eq!(
            ask_mode.evaluate("read", &read_outside, &grants),
            Evaluation::Allow
        );
        // … but not destructive commands, which are never granted wholesale.
        let rm = json!({"command": "cargo test && rm -rf target"});
        assert_eq!(
            ask(ask_mode.evaluate("bash", &rm, &grants)),
            (
                vec![AskReason::Dangerous("deletes files (`rm`)".into())],
                vec![]
            )
        );

        let auto_edit = policy(PermissionMode::AutoEdit, &root);
        assert_eq!(auto_edit.evaluate("edit", &edit, &none), Evaluation::Allow);
        assert!(matches!(
            auto_edit.evaluate("bash", &test, &none),
            Evaluation::Ask { .. }
        ));
        assert_eq!(
            auto_edit.evaluate("bash", &json!({"command": "git status -s"}), &none),
            Evaluation::Allow,
            "allow-listed"
        );
        // Writing outside the project still asks.
        assert!(matches!(
            auto_edit.evaluate(
                "write",
                &json!({"path": "/etc/hosts", "content": ""}),
                &none
            ),
            Evaluation::Ask { .. }
        ));

        let review = policy(PermissionMode::AutoReview, &root);
        assert!(matches!(
            review.evaluate("bash", &test, &none),
            Evaluation::Review { .. }
        ));
        assert_eq!(
            review.evaluate("bash", &rm, &none),
            Evaluation::Review {
                grants: vec![SessionGrant::Command("cargo test".into())]
            }
        );
        assert_eq!(
            ask(auto_edit.evaluate("bash", &rm, &none)),
            (
                vec![AskReason::Mode(PermissionMode::AutoEdit)],
                vec![SessionGrant::Command("cargo test".into())]
            )
        );
        assert_eq!(review.evaluate("edit", &edit, &none), Evaluation::Allow);

        let full = policy(PermissionMode::FullAuto, &root);
        for (tool, args) in [("bash", &rm), ("read", &read_outside), ("edit", &edit)] {
            assert_eq!(full.evaluate(tool, args, &none), Evaluation::Allow);
        }
    }

    #[test]
    fn edit_modes_only_auto_approve_rm_with_known_workspace_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path()).join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(dir.path().join("outside.txt"), "keep").unwrap();
        std::fs::create_dir_all(root.join(".cache")).unwrap();
        let mut grants = Grants::default();
        grants.add(&SessionGrant::Command("rm".into()));
        grants.add(&SessionGrant::Directory(canonical(dir.path())));

        for mode in [PermissionMode::AutoEdit, PermissionMode::AutoReview] {
            let evaluate = |command: &str| {
                policy(mode, &root).evaluate("bash", &json!({"command": command}), &grants)
            };
            for command in [
                "rm -rf .cache",
                "rm -- 'name with spaces' '-leading-dash'",
                "rm -f missing/child.txt",
                "pwd; rm -f src/../scratch.txt",
                "cd ./src && rm -f scratch.txt && pwd",
                "rm --recursive --force .cache && git status",
            ] {
                assert_eq!(evaluate(command), Evaluation::Allow, "{mode:?}: {command}");
            }
            for command in [
                "rm -rf .",
                "rm -f ../outside.txt",
                "rm -f src/../../outside.txt",
                "rm -f local.txt ../outside.txt",
                "cd .. && rm -f outside.txt",
                "cd ./src; rm -f scratch.txt",
                "rm -f *.txt",
                "rm -f \"$TARGET\"",
                "rm -f ~/scratch.txt",
                "rm -f local.txt > ../outside.txt",
                "echo local.txt | xargs rm -f",
                "rm -f local.txt && git push",
            ] {
                let (reasons, offers) = ask(evaluate(command));
                assert!(
                    matches!(&reasons[..], [AskReason::Dangerous(_)]),
                    "{mode:?}: {command}"
                );
                assert!(offers.is_empty(), "{command}");
            }
        }
    }

    #[test]
    fn navigation_and_listing_are_allowed_in_every_mode_and_omitted_from_grants() {
        let dir = tempfile::tempdir().unwrap();
        let none = Grants::default();
        for mode in PermissionMode::ALL {
            let policy = policy(mode, dir.path());
            for command in [
                "pwd",
                "cd /tmp",
                "cd .. && pwd -P",
                "cd src && git status",
                "ls",
                "ls -la /tmp",
                "/bin/ls",
                "cd src && ls && pwd",
            ] {
                assert_eq!(
                    policy.evaluate("bash", &json!({"command": command}), &none),
                    Evaluation::Allow,
                    "{mode:?}: {command}"
                );
            }
            let evaluation = policy.evaluate(
                "bash",
                &json!({"command": "cd src && ls && pwd && cargo test && git status"}),
                &none,
            );
            let expected = vec![SessionGrant::Command("cargo test".into())];
            match mode {
                PermissionMode::FullAuto => assert_eq!(evaluation, Evaluation::Allow),
                PermissionMode::AutoReview => {
                    assert_eq!(evaluation, Evaluation::Review { grants: expected })
                }
                _ => assert_eq!(ask(evaluation).1, expected),
            }
            if mode != PermissionMode::FullAuto {
                let (reasons, grants) = ask(policy.evaluate(
                    "bash",
                    &json!({"command": "cd src && rm -rf target && pwd"}),
                    &none,
                ));
                assert!(matches!(&reasons[..], [AskReason::Dangerous(_)]));
                assert!(grants.is_empty());
            }
        }
    }

    #[test]
    fn read_only_commands_skip_approval_and_review_in_every_mode() {
        let dir = tempfile::tempdir().unwrap();
        let grants = Grants::default();
        for mode in PermissionMode::ALL {
            let policy = PermissionPolicy {
                mode,
                root: dir.path().to_owned(),
                ..Default::default()
            };
            for command in [
                "head -n 20 file.txt",
                "tail -n 10 file.txt",
                "cat file.txt | grep pattern | sort | uniq -c",
                "rg --files -g '*.rs' && wc -l src/main.rs",
                "find . -type f -name '*.rs'",
                "head < file.txt 2>/dev/null",
            ] {
                assert_eq!(
                    policy.evaluate("bash", &json!({"command": command}), &grants),
                    Evaluation::Allow,
                    "{mode:?}: {command}"
                );
            }
            if mode != PermissionMode::FullAuto {
                for command in [
                    "head file.txt > output.txt",
                    "sort -o output.txt file.txt",
                    "uniq file.txt output.txt",
                    "find . -exec touch output.txt ';'",
                    "rg --pre=sh pattern",
                    "head file.txt && cargo test",
                    "head file.txt && git push",
                ] {
                    assert_ne!(
                        policy.evaluate("bash", &json!({"command": command}), &grants),
                        Evaluation::Allow,
                        "{mode:?}: {command}"
                    );
                }
            }
        }
    }

    #[test]
    fn echo_and_git_follow_the_edit_mode_rules() {
        let dir = tempfile::tempdir().unwrap();
        let grants = Grants::default();
        for mode in PermissionMode::ALL {
            let policy = PermissionPolicy {
                mode,
                root: dir.path().to_owned(),
                ..Default::default()
            };
            for command in ["echo hello", "echo 'literal > text'", "cd src && echo done"] {
                assert_eq!(
                    policy.evaluate("bash", &json!({"command": command}), &grants),
                    Evaluation::Allow,
                    "{mode:?}: {command}"
                );
            }
            for command in [
                "echo hello > file",
                "echo hello>>file",
                "git status",
                "git add . && git commit -m update",
                "git -C repo diff",
                "git checkout feature",
                "git switch feature",
            ] {
                let evaluation = policy.evaluate("bash", &json!({"command": command}), &grants);
                if mode == PermissionMode::Ask {
                    assert!(matches!(evaluation, Evaluation::Ask { .. }), "{command}");
                } else {
                    assert_eq!(evaluation, Evaluation::Allow, "{mode:?}: {command}");
                }
            }
            for command in [
                "git push",
                "git -C repo push",
                "git reset --hard",
                "git -C repo clean -fd",
                "git restore .",
                "git checkout -- file",
                "git branch -D feature",
                "git stash drop",
            ] {
                let evaluation = policy.evaluate("bash", &json!({"command": command}), &grants);
                if mode == PermissionMode::FullAuto {
                    assert_eq!(evaluation, Evaluation::Allow);
                } else {
                    let (reasons, offers) = ask(evaluation);
                    assert!(
                        matches!(&reasons[..], [AskReason::Dangerous(_)]),
                        "{mode:?}: {command}"
                    );
                    assert!(offers.is_empty());
                }
            }
            if mode != PermissionMode::FullAuto {
                let evaluation = policy.evaluate(
                    "bash",
                    &json!({"command": "echo hello > file && cargo test"}),
                    &grants,
                );
                assert!(matches!(
                    evaluation,
                    Evaluation::Ask { .. } | Evaluation::Review { .. }
                ));
            }
        }
    }

    #[test]
    fn python_commands_follow_the_edit_mode_rules() {
        let dir = tempfile::tempdir().unwrap();
        let grants = Grants::default();
        for mode in PermissionMode::ALL {
            let policy = policy(mode, dir.path());
            for command in [
                "python script.py",
                "python3 script.py --output result.json",
                "python -c 'print(1)'",
                "python3 -m unittest",
                "/usr/bin/python3 script.py",
                "env PYTHONPATH=src python3 script.py",
                "cd src && python script.py && python3 -c 'print(2)'",
            ] {
                let evaluation = policy.evaluate("bash", &json!({"command": command}), &grants);
                if mode == PermissionMode::Ask {
                    assert!(matches!(evaluation, Evaluation::Ask { .. }), "{command}");
                } else {
                    assert_eq!(evaluation, Evaluation::Allow, "{mode:?}: {command}");
                }
            }
            if mode != PermissionMode::FullAuto {
                for command in [
                    "python3 script.py && git push",
                    "python script.py; rm -rf .",
                ] {
                    let (reasons, offers) =
                        ask(policy.evaluate("bash", &json!({"command": command}), &grants));
                    assert!(
                        matches!(&reasons[..], [AskReason::Dangerous(_)]),
                        "{mode:?}: {command}"
                    );
                    assert!(offers.is_empty());
                }
                let evaluation = policy.evaluate(
                    "bash",
                    &json!({"command": "python3 script.py && cargo test"}),
                    &grants,
                );
                assert!(matches!(
                    evaluation,
                    Evaluation::Ask { .. } | Evaluation::Review { .. }
                ));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_project_are_outside() {
        let dir = tempfile::tempdir().unwrap();
        let base = canonical(dir.path());
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(base.join("missing"), root.join("dangling")).unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), root.join("backslash\\")).unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), root.join("wide\u{2003}space")).unwrap();
        for mode in [PermissionMode::AutoEdit, PermissionMode::AutoReview] {
            let policy = policy(mode, &root);
            let evaluation = policy.evaluate(
                "write",
                &json!({"path": "link/new.txt", "content": "x"}),
                &Grants::default(),
            );
            assert!(
                matches!(&evaluation, Evaluation::Ask { reasons, .. } if matches!(&reasons[0], AskReason::OutsideProject(p) if p[0] == base.join("elsewhere/new.txt"))),
                "{mode:?}: {evaluation:?}"
            );
            for command in [
                "rm -f link/new.txt",
                "rm -f dangling/new.txt",
                "cd ./link && rm -f new.txt",
                r#"rm -f "backslash\/new.txt""#,
                "rm -f wide\u{2003}space/new.txt",
            ] {
                let (reasons, grants) =
                    ask(policy.evaluate("bash", &json!({"command": command}), &Grants::default()));
                assert!(
                    matches!(&reasons[..], [AskReason::Dangerous(_)]),
                    "{command}"
                );
                assert!(grants.is_empty(), "{command}");
            }
            let local = format!("{:?}", root.join("local.txt"));
            assert_eq!(
                policy.evaluate(
                    "bash",
                    &json!({"command": format!("rm -f -- {local}")}),
                    &Grants::default()
                ),
                Evaluation::Allow
            );
        }
    }

    #[test]
    fn modes_round_trip() {
        for mode in PermissionMode::ALL {
            assert_eq!(PermissionMode::from_cli_name(mode.cli_name()), Some(mode));
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<PermissionMode>(&json).unwrap(), mode);
        }
        assert_eq!(
            serde_json::to_string(&PermissionMode::AutoEdit).unwrap(),
            "\"autoEdit\""
        );
    }
}
