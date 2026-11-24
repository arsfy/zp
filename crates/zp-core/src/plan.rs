//! The final Markdown deliverable of a read-only planning run.

use futures::future::BoxFuture;
use serde_json::{Value, json};
use zp_agent::{AgentTool, AgentToolResult, ToolContext};

pub const SUBMIT_PLAN: &str = "submit_plan";

pub(crate) const INSTRUCTIONS: &str = concat!(
    "The user enabled Plan mode. Investigate and design the requested change without implementing it. ",
    "Use the read-only tools to inspect relevant files, and use ask to resolve decisions that materially ",
    "change the plan. Use the user's language. When the plan is ready, call submit_plan by itself with ",
    "the full Markdown document. Include the goal, the proposed steps and affected areas, and how to ",
    "verify the result. Include any decisions or assumptions the implementer needs, so the plan can ",
    "be executed in a fresh session. The app will ask the user what to do next.",
);

pub(crate) struct SubmitPlan;

impl AgentTool for SubmitPlan {
    fn name(&self) -> &str {
        SUBMIT_PLAN
    }

    fn label(&self) -> &str {
        "Plan"
    }

    fn description(&self) -> &str {
        "Submit the complete Markdown implementation plan for the user's review, ending the run."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "markdown": {
                    "type": "string",
                    "description": "The full Markdown plan with a title, implementation steps and verification."
                }
            },
            "required": ["markdown"],
            "additionalProperties": false
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let markdown = args["markdown"].as_str().unwrap_or_default().trim();
            anyhow::ensure!(!markdown.is_empty(), "The Markdown plan is empty");
            Ok(AgentToolResult {
                terminate: true,
                ..AgentToolResult::text(markdown)
            })
        })
    }
}
