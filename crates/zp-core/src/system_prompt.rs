//! System prompt assembly and project context files.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zp_agent::AgentTool;

/// File names checked in each directory, in priority order (the first one found wins).
pub const CONTEXT_FILE_NAMES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// An instructions file included in the system prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFile {
    /// Where it was found.
    pub path: PathBuf,
    /// Its text.
    pub content: String,
}

fn context_file_in(dir: &Path) -> Option<ContextFile> {
    CONTEXT_FILE_NAMES.iter().find_map(|name| {
        let path = dir.join(name);
        if !path.is_file() {
            return None;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => Some(ContextFile {
                content: content.trim_start_matches('\u{FEFF}').to_owned(),
                path,
            }),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not read context file");
                None
            }
        }
    })
}

/// Context files for `cwd`: the global one in `global_dir` (e.g. `~/.zp/AGENTS.md`), then
/// one per directory from the filesystem root down to `cwd`, so more specific
/// instructions come last.
pub fn load_context_files(cwd: &Path, global_dir: Option<&Path>) -> Vec<ContextFile> {
    let mut files: Vec<ContextFile> = global_dir.and_then(context_file_in).into_iter().collect();
    for path in project_context_files(cwd) {
        if let Some(file) = path.parent().and_then(context_file_in)
            && !files.iter().any(|seen| seen.path == file.path)
        {
            files.push(file);
        }
    }
    files
}

/// The context files of `cwd` and its ancestors (without the global one): what an
/// untrusted project could use to instruct the agent.
pub fn project_context_files(cwd: &Path) -> Vec<PathBuf> {
    let mut ancestors: Vec<&Path> = cwd.ancestors().collect();
    ancestors.reverse();
    ancestors
        .into_iter()
        .filter_map(|dir| {
            CONTEXT_FILE_NAMES
                .iter()
                .map(|name| dir.join(name))
                .find(|path| path.is_file())
        })
        .collect()
}

/// Inputs of [`build_system_prompt`].
#[derive(Clone)]
pub struct SystemPromptOptions<'a> {
    /// Working directory.
    pub cwd: &'a Path,
    /// Enabled tools; their snippets and guidelines are listed.
    pub tools: &'a [Arc<dyn AgentTool>],
    /// Project instructions.
    pub context_files: &'a [ContextFile],
    /// Replaces the default preamble, tool list and rules.
    pub custom_prompt: Option<&'a str>,
    /// Extra text appended after the rules.
    pub append: Option<&'a str>,
    /// The skills section (see [`crate::resources::Resources::skills_prompt`]).
    pub skills: Option<&'a str>,
    /// A private directory for the session's temporary files.
    pub scratch_directory: Option<&'a Path>,
}

impl Default for SystemPromptOptions<'_> {
    fn default() -> Self {
        Self {
            cwd: Path::new(""),
            tools: &[],
            context_files: &[],
            custom_prompt: None,
            append: None,
            skills: None,
            scratch_directory: None,
        }
    }
}

const PREAMBLE: &str = "You are a coding assistant running inside ZP, a coding agent harness. \
                        Help users inspect files, run commands, and modify code.";

/// Builds the system prompt: preamble, `<tools>`, `<rules>`, optional addendum,
/// `<project_context>` and `<cwd>` sections.
pub fn build_system_prompt(options: &SystemPromptOptions<'_>) -> String {
    let mut sections: Vec<(&str, String)> = Vec::new();
    let preamble = match options.custom_prompt {
        Some(custom) => custom.to_owned(),
        None => {
            let tools: Vec<String> = options
                .tools
                .iter()
                .filter_map(|tool| Some(format!("- {}: {}", tool.name(), tool.prompt_snippet()?)))
                .collect();
            if !tools.is_empty() {
                sections.push(("tools", tools.join("\n")));
            }
            sections.push(("rules", rules(options.tools)));
            PREAMBLE.to_owned()
        }
    };
    if let Some(append) = options.append.filter(|text| !text.trim().is_empty()) {
        sections.push(("addendum", append.trim().to_owned()));
    }
    if !options.context_files.is_empty() {
        let mut text = String::from("Project-specific instructions and guidelines:");
        for file in options.context_files {
            text.push_str(&format!(
                "\n\n<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                file.path.to_string_lossy().replace('\\', "/"),
                file.content.trim_end()
            ));
        }
        sections.push(("project_context", text));
    }
    if let Some(skills) = options.skills.filter(|text| !text.trim().is_empty()) {
        sections.push(("skills", skills.trim().to_owned()));
    }
    if let Some(path) = options
        .scratch_directory
        .filter(|_| options.tools.iter().any(|tool| tool.name() == "bash"))
    {
        sections.push((
            "session_scratch",
            format!(
                "Use {} for temporary files specific to this session. Use literal paths for shell cleanup.",
                path.display()
            ),
        ));
    }
    sections.push(("cwd", options.cwd.to_string_lossy().replace('\\', "/")));

    let mut prompt = preamble;
    for (name, body) in sections {
        prompt.push_str(&format!("\n\n<{name}>\n{body}\n</{name}>"));
    }
    prompt
}

fn rules(tools: &[Arc<dyn AgentTool>]) -> String {
    let has = |name: &str| tools.iter().any(|tool| tool.name() == name);
    let mut rules: Vec<String> = Vec::new();
    let mut add = |rule: &str| {
        let rule = rule.trim();
        if !rule.is_empty() && !rules.iter().any(|r| r == rule) {
            rules.push(rule.to_owned());
        }
    };
    if has("bash") && !has("grep") && !has("find") && !has("ls") {
        add("Use bash for file operations like ls, rg, find");
    }
    for tool in tools {
        for rule in tool.prompt_guidelines() {
            add(rule);
        }
    }
    add("Be concise in your responses");
    add("Show file paths clearly when working with files");
    rules
        .iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_files_from_global_and_ancestors() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let project = root.path().join("repo");
        let nested = project.join("crates").join("app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(global.join("AGENTS.md"), "global rules").unwrap();
        std::fs::write(project.join("AGENTS.md"), "\u{FEFF}repo rules").unwrap();
        std::fs::write(project.join("CLAUDE.md"), "ignored: AGENTS.md wins").unwrap();
        std::fs::write(nested.join("CLAUDE.md"), "app rules").unwrap();

        let files = load_context_files(&nested, Some(&global));
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, ["global rules", "repo rules", "app rules"]);
    }

    #[test]
    fn prompt_lists_tools_rules_context_and_cwd() {
        let cwd = Path::new("/work/app");
        let tools = zp_tools::coding_tools(cwd);
        let files = [ContextFile {
            path: PathBuf::from("/work/app/AGENTS.md"),
            content: "Use tabs.\n".into(),
        }];
        let prompt = build_system_prompt(&SystemPromptOptions {
            cwd,
            tools: &tools,
            context_files: &files,
            append: Some("Be extra careful."),
            ..SystemPromptOptions::default()
        });
        assert!(prompt.starts_with(PREAMBLE));
        assert!(prompt.contains("<tools>\n- read: Read file contents\n- bash: "));
        assert!(prompt.contains("- Use read to examine files instead of cat or sed."));
        assert!(
            !prompt.contains("Use bash for file operations"),
            "grep/find/ls exist"
        );
        assert!(prompt.contains("- Be concise in your responses\n- Show file paths clearly when working with files\n</rules>"));
        assert!(prompt.contains("<addendum>\nBe extra careful.\n</addendum>"));
        assert!(prompt.contains("<project_instructions path=\"/work/app/AGENTS.md\">\nUse tabs.\n</project_instructions>"));
        assert!(prompt.ends_with("<cwd>\n/work/app\n</cwd>"));

        let custom = build_system_prompt(&SystemPromptOptions {
            cwd,
            tools: &tools,
            custom_prompt: Some("Custom."),
            ..SystemPromptOptions::default()
        });
        assert_eq!(custom, "Custom.\n\n<cwd>\n/work/app\n</cwd>");
    }
}
