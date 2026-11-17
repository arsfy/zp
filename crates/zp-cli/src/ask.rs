//! The agent's questions (`ask`) on the terminal.

use std::io::Write as _;

use futures::future::BoxFuture;
use zp_tools::{Answer, AskRequest, Asker, Question};

use crate::approve::InputLines;
use crate::run::Style;

/// Asks on stderr and reads answers from stdin (only used when stdin is a terminal).
pub(crate) struct TerminalAsker {
    input: InputLines,
    style: Style,
}

impl TerminalAsker {
    pub(crate) fn new(input: InputLines, style: Style) -> Self {
        Self { input, style }
    }
}

/// Reads an answer: option numbers (`2`, `1,3`) pick options, anything else is typed text.
pub(crate) fn parse_answer(question: &Question, line: &str) -> Answer {
    let line = line.trim();
    let numbers: Option<Vec<usize>> = line
        .split([',', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<usize>()
                .ok()
                .filter(|n| (1..=question.options.len()).contains(n))
        })
        .collect();
    match numbers.filter(|numbers| !numbers.is_empty()) {
        Some(mut numbers) => {
            if !question.multi_select {
                numbers.truncate(1);
            }
            numbers.dedup();
            Answer {
                selected: numbers
                    .into_iter()
                    .map(|n| question.options[n - 1].label.clone())
                    .collect(),
                text: String::new(),
            }
        }
        None => Answer {
            selected: Vec::new(),
            text: line.to_owned(),
        },
    }
}

impl Asker for TerminalAsker {
    fn ask(&self, _call_id: &str, request: AskRequest) -> BoxFuture<'static, Option<Vec<Answer>>> {
        let input = self.input.clone();
        let style = self.style;
        Box::pin(async move {
            let count = request.questions.len();
            let mut answers = Vec::new();
            for (index, question) in request.questions.iter().enumerate() {
                let number = if count > 1 {
                    format!(" ({}/{count})", index + 1)
                } else {
                    String::new()
                };
                eprintln!(
                    "{} {}",
                    style.paint("1;36", &format!("Question{number}:")),
                    question.question
                );
                for (n, option) in question.options.iter().enumerate() {
                    let description = if option.description.is_empty() {
                        String::new()
                    } else {
                        style.paint("2", &format!(" — {}", option.description))
                    };
                    eprintln!("  {}. {}{description}", n + 1, option.label);
                }
                let hint = match (question.options.is_empty(), question.multi_select) {
                    (true, _) => "type your answer",
                    (false, false) => "pick a number or type your answer",
                    (false, true) => "pick numbers (1,3) or type your answer",
                };
                eprint!("{}", style.paint("1", &format!("  {hint} › ")));
                let _ = std::io::stderr().flush();
                // End of input dismisses the questions.
                let line = input.lock().await.next_line().await.ok().flatten()?;
                answers.push(parse_answer(question, &line));
            }
            Some(answers)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zp_tools::QuestionOption;

    #[test]
    fn answers() {
        let mut question = Question {
            question: "Which?".into(),
            options: ["A", "B", "C"]
                .map(|label| QuestionOption {
                    label: label.into(),
                    description: String::new(),
                })
                .to_vec(),
            multi_select: false,
        };
        assert_eq!(parse_answer(&question, " 2 ").selected, ["B"]);
        assert_eq!(parse_answer(&question, "1,3").selected, ["A"]);
        let typed = parse_answer(&question, "neither, use D");
        assert_eq!(
            (typed.selected.len(), typed.text.as_str()),
            (0, "neither, use D")
        );
        assert_eq!(
            parse_answer(&question, "4").text,
            "4",
            "out of range is text"
        );
        question.multi_select = true;
        assert_eq!(parse_answer(&question, "1, 3").selected, ["A", "C"]);
        assert!(parse_answer(&question, "").is_empty());
    }
}
