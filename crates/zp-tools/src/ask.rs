//! `ask`: questions to the user, answered by picking options or typing.
//!
//! The tool only exists where someone can answer: the front end provides an [`Asker`]
//! (the app's question card, a terminal prompt).

use std::sync::Arc;

use anyhow::bail;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zp_agent::{AgentTool, AgentToolResult, ToolContext, ToolExecutionMode};

/// Most questions in one call.
pub const MAX_QUESTIONS: usize = 4;
/// Most options of a question.
pub const MAX_OPTIONS: usize = 6;

/// A choice offered for a question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// One question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub question: String,
    /// Suggested answers; the user can always type their own.
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// Whether several options can be picked.
    #[serde(default)]
    pub multi_select: bool,
}

/// The arguments of an `ask` call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskRequest {
    pub questions: Vec<Question>,
}

/// The answer to one question: picked option labels and/or typed text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

impl Answer {
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty() && self.text.trim().is_empty()
    }

    /// The answer in one line: the picked options, then the typed text.
    pub fn summary(&self) -> String {
        let mut parts = self.selected.clone();
        let text = self.text.trim();
        if !text.is_empty() {
            parts.push(text.to_owned());
        }
        if parts.is_empty() {
            "(no answer)".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

/// The details of an `ask` result: the questions and the user's answers (`None` when
/// they dismissed them).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOutcome {
    pub questions: Vec<Question>,
    pub answers: Option<Vec<Answer>>,
}

impl AskOutcome {
    pub fn from_details(details: &Value) -> Option<Self> {
        serde_json::from_value(details.clone()).ok()
    }
}

/// Puts questions to the user.
pub trait Asker: Send + Sync {
    /// Resolves with one answer per question, or `None` when the user dismissed them
    /// (dropping the request counts as dismissing).
    fn ask(&self, call_id: &str, request: AskRequest) -> BoxFuture<'static, Option<Vec<Answer>>>;
}

/// Asks the user through an [`Asker`].
#[derive(Clone)]
pub struct AskTool {
    asker: Arc<dyn Asker>,
}

impl AskTool {
    pub fn new(asker: Arc<dyn Asker>) -> Self {
        Self { asker }
    }
}

impl std::fmt::Debug for AskTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskTool").finish_non_exhaustive()
    }
}

/// Checks and tidies the questions of a call.
fn validate(mut request: AskRequest) -> anyhow::Result<AskRequest> {
    if request.questions.is_empty() {
        bail!("ask at least one question");
    }
    if request.questions.len() > MAX_QUESTIONS {
        bail!("ask at most {MAX_QUESTIONS} questions at once");
    }
    for question in &mut request.questions {
        question.question = question.question.trim().to_owned();
        if question.question.is_empty() {
            bail!("every question needs text");
        }
        question
            .options
            .retain(|option| !option.label.trim().is_empty());
        if question.options.len() > MAX_OPTIONS {
            bail!("offer at most {MAX_OPTIONS} options per question");
        }
        for option in &mut question.options {
            option.label = option.label.trim().to_owned();
            option.description = option.description.trim().to_owned();
        }
    }
    Ok(request)
}

impl AgentTool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }

    fn label(&self) -> &str {
        "Ask"
    }

    fn description(&self) -> &str {
        "Ask the user one or more questions and wait for the answers. Offer concrete \
         options when there are clear choices (put the recommended one first); the user can \
         always type their own answer instead."
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("Ask the user to decide or clarify")
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Use ask only for decisions that belong to the user (unclear requirements, choices between valid approaches) and that you cannot settle by reading the code; do not ask for permission to run tools.",
        ]
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": format!("1 to {MAX_QUESTIONS} questions"),
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string", "description": "The question, ending with a question mark"},
                            "options": {
                                "type": "array",
                                "description": format!("Up to {MAX_OPTIONS} suggested answers"),
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string", "description": "A short answer (1-5 words)"},
                                        "description": {"type": "string", "description": "What choosing it means"}
                                    },
                                    "required": ["label"]
                                }
                            },
                            "multiSelect": {"type": "boolean", "description": "Allow picking several options"}
                        },
                        "required": ["question"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        // One set of questions at a time, with nothing running meanwhile.
        Some(ToolExecutionMode::Sequential)
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let request = validate(serde_json::from_value(args)?)?;
            let questions = request.questions.clone();
            let answers = tokio::select! {
                () = ctx.cancel.cancelled() => bail!("Operation aborted"),
                answers = self.asker.ask(&ctx.tool_call_id, request) => answers,
            };
            let text = match &answers {
                None => "The user dismissed the questions without answering. Do not ask again; \
                         continue with your best judgment or stop and explain what you need."
                    .to_owned(),
                Some(answers) => questions
                    .iter()
                    .zip(answers.iter().chain(std::iter::repeat(&Answer::default())))
                    .map(|(question, answer)| {
                        format!("Q: {}\nA: {}", question.question, answer.summary())
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            };
            let outcome = AskOutcome { questions, answers };
            Ok(AgentToolResult::text(text).with_details(serde_json::to_value(&outcome)?))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_questions() {
        let request = |value: Value| validate(serde_json::from_value(value).unwrap());
        assert!(request(json!({"questions": []})).is_err());
        assert!(request(json!({"questions": [{"question": "  "}]})).is_err());
        let ok = request(json!({"questions": [{
            "question": " Which one? ",
            "options": [{"label": " A "}, {"label": ""}, {"label": "B", "description": "the other"}]
        }]}))
        .unwrap();
        assert_eq!(ok.questions[0].question, "Which one?");
        assert_eq!(ok.questions[0].options.len(), 2);
        assert_eq!(ok.questions[0].options[0].label, "A");
        assert!(!ok.questions[0].multi_select);
    }

    #[test]
    fn answer_summaries() {
        let answer = Answer {
            selected: vec!["A".into(), "B".into()],
            text: " and more ".into(),
        };
        assert_eq!(answer.summary(), "A, B, and more");
        assert_eq!(Answer::default().summary(), "(no answer)");
    }
}
