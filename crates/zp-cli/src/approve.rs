//! Tool call approvals on the terminal.

use std::io::Write as _;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::io::{BufReader, Lines, Stdin};
use zp_core::permissions::{Approver, Decision, PermissionRequest};

use crate::run::Style;

/// Lines of stdin, shared by the interactive prompt and approvals.
pub(crate) type InputLines = Arc<tokio::sync::Mutex<Lines<BufReader<Stdin>>>>;

/// Asks on stderr and reads the answer from stdin; denies when stdin is not a terminal.
pub(crate) struct TerminalApprover {
    input: Option<InputLines>,
    style: Style,
}

impl TerminalApprover {
    pub(crate) fn new(input: Option<InputLines>, style: Style) -> Self {
        Self { input, style }
    }
}

/// Reads `y`, `a`, `n` or `n <reason>` (anything else denies).
pub(crate) fn parse_answer(line: &str) -> Decision {
    let line = line.trim();
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    match word.to_lowercase().as_str() {
        "y" | "yes" => Decision::Allow,
        "a" | "always" => Decision::AllowForSession,
        "n" | "no" | "" => {
            Decision::Deny((!rest.trim().is_empty()).then(|| rest.trim().to_owned()))
        }
        _ => Decision::Deny(Some(line.to_owned())),
    }
}

impl Approver for TerminalApprover {
    fn approve(&self, request: PermissionRequest) -> BoxFuture<'static, Decision> {
        let input = self.input.clone();
        let style = self.style;
        Box::pin(async move {
            let reasons: Vec<String> = request.reasons.iter().map(|r| r.describe()).collect();
            eprintln!(
                "{} {}",
                style.paint("1;33", "Approval needed:"),
                request.command.as_deref().unwrap_or(&request.summary)
            );
            eprintln!("{}", style.paint("2", &format!("  {}", reasons.join("; "))));
            if let Some(description) = &request.description {
                eprintln!(
                    "{}",
                    style.paint("2", &format!("  What it does: {description}"))
                );
            }
            let Some(input) = input else {
                eprintln!(
                    "{}",
                    style.paint(
                        "31",
                        "  denied: stdin is not a terminal (see --permissions)"
                    )
                );
                return Decision::Deny(Some(
                    "ZP runs non-interactively and cannot ask the user for approval.".into(),
                ));
            };
            let session = if request.grants.is_empty() {
                String::new()
            } else {
                let grants: Vec<String> = request.grants.iter().map(|g| g.describe()).collect();
                format!(" · [a] always allow {} this session", grants.join(", "))
            };
            eprint!(
                "{}",
                style.paint(
                    "1",
                    &format!("  [y] approve{session} · [n] deny (n <reason>) › ")
                )
            );
            let _ = std::io::stderr().flush();
            let line = input.lock().await.next_line().await;
            match line {
                Ok(Some(line)) => {
                    let decision = parse_answer(&line);
                    if decision == Decision::AllowForSession && request.grants.is_empty() {
                        Decision::Allow
                    } else {
                        decision
                    }
                }
                _ => Decision::Deny(None),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(parse_answer("y"), Decision::Allow);
        assert_eq!(parse_answer(" Yes "), Decision::Allow);
        assert_eq!(parse_answer("a"), Decision::AllowForSession);
        assert_eq!(parse_answer(""), Decision::Deny(None));
        assert_eq!(
            parse_answer("n use the test script instead"),
            Decision::Deny(Some("use the test script instead".into()))
        );
        assert_eq!(
            parse_answer("please don't"),
            Decision::Deny(Some("please don't".into()))
        );
    }
}
