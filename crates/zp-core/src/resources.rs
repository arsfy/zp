//! Prompt templates and skills, in pi's formats.
//!
//! - Prompt templates: `*.md` files in `~/.zp/prompts/` and `<project>/.zp/prompts/`,
//!   with optional frontmatter (`description`, `argument-hint`). `/name args` expands one;
//!   `$1`, `$@`, `$ARGUMENTS`, `${1:-default}`, `${@:N}` and `${@:N:L}` take arguments.
//! - Skills ([Agent Skills](https://agentskills.io/specification)): directories with a
//!   `SKILL.md` (frontmatter `name`, `description`) under `~/.zp/skills/`,
//!   `~/.agents/skills/`, and in projects `.zp/skills/` and `.agents/skills/`. The system
//!   prompt lists them; `/skill:name args` loads one explicitly.
//!
//! Project resources are only loaded for trusted projects.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where a resource comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    User,
    Project,
}

/// A `/name` prompt template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub body: String,
    pub path: PathBuf,
    pub source: Source,
}

/// A skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The `SKILL.md` file.
    pub path: PathBuf,
    /// Hidden from the model's list; only `/skill:name` loads it.
    pub disable_model_invocation: bool,
    pub source: Source,
}

impl Skill {
    pub fn base_dir(&self) -> &Path {
        self.path.parent().unwrap_or(&self.path)
    }
}

/// Templates and skills available in a project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resources {
    pub templates: Vec<PromptTemplate>,
    pub skills: Vec<Skill>,
    /// Problems found while loading (invalid files, name collisions).
    pub diagnostics: Vec<String>,
}

/// Splits `---` frontmatter (flat `key: value` lines) from the body.
pub fn parse_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let text = text.trim_start_matches('\u{FEFF}');
    let mut fields = BTreeMap::new();
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (fields, text.to_owned());
    };
    let Some(end) = rest.find("\n---") else {
        return (fields, text.to_owned());
    };
    for line in rest[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.starts_with(char::is_whitespace) {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        fields.insert(key.trim().to_owned(), value.to_owned());
    }
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']);
    (fields, body.to_owned())
}

/// Splits command arguments like a shell: whitespace separates them; a quote opens only
/// at the start of an argument; `\"` inside a quoted argument keeps the quote; an
/// unclosed quote is literal text.
pub fn parse_command_args(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut args = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut quote: Option<(char, usize, usize)> = None; // (quote, start index, current len at open)
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some((q, _, _)) => {
                if c == '\\' && chars.get(i + 1) == Some(&q) {
                    current.push(q);
                    i += 1;
                } else if c == q {
                    quote = None;
                } else {
                    current.push(c);
                }
            }
            None if (c == '"' || c == '\'') && !has_token => {
                quote = Some((c, i, current.len()));
                has_token = true;
            }
            None if c.is_whitespace() => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => {
                current.push(c);
                has_token = true;
            }
        }
        i += 1;
        if i == chars.len()
            && let Some((_, start, len)) = quote.take()
        {
            // Unclosed: reread the rest literally.
            current.truncate(len);
            i = start;
            let rest: String = chars[i..].iter().collect();
            current.push_str(&rest);
            for word in current.split_whitespace() {
                args.push(word.to_owned());
            }
            return args;
        }
    }
    if has_token {
        args.push(current);
    }
    args
}

/// Fills `$1`, `$@`, `$ARGUMENTS`, `${N:-default}`, `${@:-default}`, `${@:N}` and
/// `${@:N:L}` in `content`.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all = args.join(" ");
    let arg = |n: usize| args.get(n.wrapping_sub(1)).cloned().unwrap_or_default();
    let mut out = String::new();
    let mut rest = content;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        if let Some(body) = tail.strip_prefix('{')
            && let Some(end) = body.find('}')
        {
            let inner = &body[..end];
            let replaced = if let Some((target, default)) = inner.split_once(":-") {
                let value = match target {
                    "@" | "ARGUMENTS" => all.clone(),
                    n => n.parse::<usize>().map(arg).unwrap_or_default(),
                };
                Some(if value.is_empty() {
                    default.to_owned()
                } else {
                    value
                })
            } else if let Some(slice) = inner.strip_prefix("@:") {
                let mut parts = slice.splitn(2, ':');
                let start = parts.next().and_then(|s| s.parse::<usize>().ok());
                let length = parts.next().map(|s| s.parse::<usize>().ok());
                match (start, length) {
                    (Some(start), None) => Some(
                        args.iter()
                            .skip(start.max(1) - 1)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    (Some(start), Some(Some(length))) => Some(
                        args.iter()
                            .skip(start.max(1) - 1)
                            .take(length)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(value) = replaced {
                out.push_str(&value);
                rest = &body[end + 1..];
                continue;
            }
        }
        if let Some(after) = tail.strip_prefix("ARGUMENTS") {
            out.push_str(&all);
            rest = after;
        } else if let Some(after) = tail.strip_prefix('@') {
            out.push_str(&all);
            rest = after;
        } else {
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                out.push('$');
                rest = tail;
            } else {
                out.push_str(&arg(digits.parse().unwrap_or(0)));
                rest = &tail[digits.len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

impl Resources {
    /// Loads the user's resources from `data_dir` and `~/.agents`, and the project's
    /// (from `cwd` up to its repository root) when `trusted`.
    pub fn load(data_dir: Option<&Path>, cwd: &Path, trusted: bool) -> Self {
        let home = dirs::home_dir();
        Self::load_from(data_dir, home.as_deref(), cwd, trusted)
    }

    /// [`Resources::load`] with an explicit home directory (tests).
    pub fn load_from(
        data_dir: Option<&Path>,
        home: Option<&Path>,
        cwd: &Path,
        trusted: bool,
    ) -> Self {
        let mut resources = Self::default();
        let boundary = crate::project::trust_boundary(cwd);
        let project_dirs: Vec<PathBuf> = if trusted {
            cwd.ancestors()
                .take_while(|dir| dir.starts_with(&boundary))
                .map(Path::to_path_buf)
                .collect()
        } else {
            Vec::new()
        };
        // More specific locations first: the first of a name wins.
        for dir in &project_dirs {
            resources.load_templates(&dir.join(".zp/prompts"), Source::Project);
        }
        if let Some(data) = data_dir {
            resources.load_templates(&data.join("prompts"), Source::User);
        }
        for dir in &project_dirs {
            resources.load_skills(&dir.join(".zp/skills"), Source::Project);
            resources.load_skills(&dir.join(".agents/skills"), Source::Project);
        }
        if let Some(data) = data_dir {
            resources.load_skills(&data.join("skills"), Source::User);
        }
        if let Some(home) = home {
            resources.load_skills(&home.join(".agents/skills"), Source::User);
        }
        resources
    }

    fn load_templates(&mut self, dir: &Path, source: Source) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md") && p.is_file())
            .collect();
        paths.sort();
        for path in paths {
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    self.diagnostics
                        .push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            if self.templates.iter().any(|t| t.name == name) {
                self.diagnostics
                    .push(format!("{}: /{name} is already defined", path.display()));
                continue;
            }
            let (fields, body) = parse_frontmatter(&text);
            let description = fields.get("description").cloned().unwrap_or_else(|| {
                body.lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or_default()
                    .chars()
                    .take(80)
                    .collect()
            });
            self.templates.push(PromptTemplate {
                name,
                description,
                argument_hint: fields.get("argument-hint").cloned(),
                body: body.trim().to_owned(),
                path,
                source,
            });
        }
    }

    fn load_skills(&mut self, dir: &Path, source: Source) {
        if !dir.is_dir() {
            return;
        }
        let mut files: Vec<PathBuf> = walk_skill_files(dir, 0);
        files.sort();
        for path in files {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    self.diagnostics
                        .push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let (fields, _) = parse_frontmatter(&text);
            let dir_name = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let name = fields
                .get("name")
                .cloned()
                .unwrap_or_else(|| dir_name.to_owned());
            let Some(description) = fields.get("description").filter(|d| !d.trim().is_empty())
            else {
                self.diagnostics
                    .push(format!("{}: a skill needs a description", path.display()));
                continue;
            };
            if !valid_skill_name(&name) {
                self.diagnostics
                    .push(format!("{}: invalid skill name `{name}`", path.display()));
                continue;
            }
            if self.skills.iter().any(|s| s.name == name) {
                self.diagnostics.push(format!(
                    "{}: skill `{name}` is already defined",
                    path.display()
                ));
                continue;
            }
            self.skills.push(Skill {
                name,
                description: description.chars().take(1024).collect(),
                disable_model_invocation: fields
                    .get("disable-model-invocation")
                    .is_some_and(|v| v == "true"),
                path,
                source,
            });
        }
    }

    pub fn template(&self, name: &str) -> Option<&PromptTemplate> {
        self.templates.iter().find(|t| t.name == name)
    }

    pub fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Expands `/skill:name args` and `/template args`; `None` when `input` is neither.
    pub fn expand(&self, input: &str) -> Option<String> {
        let command = input.strip_prefix('/')?;
        let (name, args) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(n, a)| (n, a.trim()));
        if let Some(skill_name) = name.strip_prefix("skill:") {
            let skill = self.skill(skill_name)?;
            let text = std::fs::read_to_string(&skill.path).ok()?;
            let (_, body) = parse_frontmatter(&text);
            let block = format!(
                "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
                skill.name,
                skill.path.display(),
                skill.base_dir().display(),
                body.trim()
            );
            return Some(if args.is_empty() {
                block
            } else {
                format!("{block}\n\n{args}")
            });
        }
        let template = self.template(name)?;
        Some(substitute_args(&template.body, &parse_command_args(args)))
    }

    /// The skills section of the system prompt (empty without skills).
    pub fn skills_prompt(&self) -> String {
        let visible: Vec<&Skill> = self
            .skills
            .iter()
            .filter(|s| !s.disable_model_invocation)
            .collect();
        if visible.is_empty() {
            return String::new();
        }
        let escape = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let mut lines = vec![
            "The following skills provide specialized instructions for specific tasks.".to_owned(),
            "Use the read tool to load a skill's file when the task matches its description.".to_owned(),
            "Resolve relative paths against the skill's directory (parent of SKILL.md). Use absolute paths in tool calls.".to_owned(),
            String::new(),
            "<available_skills>".to_owned(),
        ];
        for skill in visible {
            lines.push("  <skill>".into());
            lines.push(format!("    <name>{}</name>", escape(&skill.name)));
            lines.push(format!(
                "    <description>{}</description>",
                escape(&skill.description)
            ));
            lines.push(format!(
                "    <location>{}</location>",
                escape(&skill.path.to_string_lossy().replace('\\', "/"))
            ));
            lines.push("  </skill>".into());
        }
        lines.push("</available_skills>".into());
        lines.join("\n")
    }
}

/// `SKILL.md` files under `dir` (a skill's own subdirectories are not searched).
fn walk_skill_files(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let skill = dir.join("SKILL.md");
    if skill.is_file() {
        return vec![skill];
    }
    if depth > 4 {
        return Vec::new();
    }
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .flat_map(|p| walk_skill_files(&p, depth + 1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        parse_command_args(s)
    }

    #[test]
    fn arguments_and_substitution() {
        assert_eq!(
            args(r#"one "two three" doesn't 'four'"#),
            ["one", "two three", "doesn't", "four"]
        );
        assert_eq!(args(r#""the \"fast\" path""#), [r#"the "fast" path"#]);
        assert_eq!(args(r#"a "unclosed b"#), ["a", "\"unclosed", "b"]);
        assert_eq!(args(r#""" x"#), ["", "x"]);
        let a = args("x y z");
        assert_eq!(substitute_args("$1-$2-$9", &a), "x-y-");
        assert_eq!(
            substitute_args("all: $@ / $ARGUMENTS", &a),
            "all: x y z / x y z"
        );
        assert_eq!(
            substitute_args("${1:-none} ${4:-none} ${@:-empty}", &a),
            "x none x y z"
        );
        assert_eq!(substitute_args("${@:2} | ${@:1:2}", &a), "y z | x y");
        assert_eq!(
            substitute_args("costs $ 5 and ${HOME}", &a),
            "costs $ 5 and ${HOME}"
        );
        assert_eq!(substitute_args("${@:-default}", &[]), "default");
    }

    #[test]
    fn frontmatter() {
        let (fields, body) = parse_frontmatter(
            "---\ndescription: \"Review: staged\"\nargument-hint: '[focus]'\n---\nBody $1\n",
        );
        assert_eq!(fields["description"], "Review: staged");
        assert_eq!(fields["argument-hint"], "[focus]");
        assert_eq!(body, "Body $1\n");
        let (fields, body) = parse_frontmatter("No frontmatter");
        assert!(fields.is_empty());
        assert_eq!(body, "No frontmatter");
    }

    #[test]
    fn loads_expands_and_lists() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let project = project.path().canonicalize().unwrap();
        let write = |path: PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            data.path().join("prompts/review.md"),
            "---\ndescription: Review changes\nargument-hint: \"[focus]\"\n---\nReview. Focus on ${1:-correctness}.",
        );
        write(
            data.path().join("prompts/plain.md"),
            "\n\nSay hello to $@\n",
        );
        write(
            project.join(".zp/prompts/review.md"),
            "Project review of $1",
        );
        write(
            data.path().join("skills/pdf/SKILL.md"),
            "---\nname: pdf-tools\ndescription: Read PDFs. Use for PDF files.\n---\n# PDF\nRun scripts/x.sh",
        );
        write(
            home.path().join(".agents/skills/hidden/SKILL.md"),
            "---\ndescription: Only on request\ndisable-model-invocation: true\n---\nSecret",
        );
        write(
            home.path().join(".agents/skills/broken/SKILL.md"),
            "---\nname: broken\n---\nno description",
        );
        write(
            project.join(".agents/skills/deploy/SKILL.md"),
            "---\ndescription: Deploy <the> app\n---\nSteps",
        );

        let untrusted = Resources::load_from(Some(data.path()), Some(home.path()), &project, false);
        assert_eq!(untrusted.template("review").unwrap().source, Source::User);
        assert!(untrusted.skill("deploy").is_none());
        let names: Vec<&str> = untrusted.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["pdf-tools", "hidden"]);
        assert_eq!(
            untrusted.diagnostics.len(),
            1,
            "{:?}",
            untrusted.diagnostics
        );
        assert_eq!(
            untrusted.template("plain").unwrap().description,
            "Say hello to $@"
        );
        assert_eq!(
            untrusted
                .template("review")
                .unwrap()
                .argument_hint
                .as_deref(),
            Some("[focus]")
        );

        let trusted = Resources::load_from(Some(data.path()), Some(home.path()), &project, true);
        assert_eq!(
            trusted.template("review").unwrap().source,
            Source::Project,
            "project first"
        );
        assert_eq!(
            trusted.expand("/review api").unwrap(),
            "Project review of api"
        );
        assert_eq!(
            untrusted.expand("/review").unwrap(),
            "Review. Focus on correctness."
        );
        assert_eq!(
            untrusted.expand("/plain a \"b c\"").unwrap(),
            "Say hello to a b c"
        );
        assert_eq!(untrusted.expand("/nope x"), None);
        assert_eq!(untrusted.expand("not a command"), None);

        let skill = untrusted.expand("/skill:pdf-tools extract a.pdf").unwrap();
        assert!(skill.starts_with("<skill name=\"pdf-tools\" location=\""));
        assert!(skill.contains("# PDF\nRun scripts/x.sh\n</skill>\n\nextract a.pdf"));
        assert!(skill.contains(&format!(
            "References are relative to {}.",
            data.path().join("skills/pdf").display()
        )));

        let prompt = trusted.skills_prompt();
        assert!(prompt.contains("<name>pdf-tools</name>"));
        assert!(prompt.contains("<description>Deploy &lt;the&gt; app</description>"));
        assert!(!prompt.contains("hidden"));
        assert_eq!(Resources::default().skills_prompt(), "");
    }
}
