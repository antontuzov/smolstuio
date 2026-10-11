//! A run's conversation with the person who started it.
//!
//! Two halves: the questions the loop stops to ask, and the running account of
//! what it did while it was going. Both go to stderr, so stdout keeps holding
//! only the command's answer — which for `smoll --json` is a document a script
//! parses, and a parseable document cannot have a "[y]es?" in the middle of it.
//!
//! An approver here never decides *whether* to be asked. The policy settled that
//! before the tool was reached; this module only speaks to the person.

use std::io::IsTerminal;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};

use agent_core::{
    AgentEvent, ApprovalDecision, ApprovalRequest, Approver, Observer, Outcome, ToolResult,
    ToolStatus,
};

/// What a run does when it needs an answer and there is a terminal to ask on.
///
/// Without one — a pipe, a CI job, `smoll task … < /dev/null` — it refuses and
/// says why, because "the agent could not reach me" and "the agent asked and I
/// did not answer" must not collapse into the same silence.
pub struct Terminal {
    interactive: bool,
}

impl Terminal {
    pub fn new() -> Self {
        Self {
            interactive: std::io::stdin().is_terminal(),
        }
    }

    async fn ask(&self, request: &ApprovalRequest) -> ApprovalDecision {
        if !self.interactive {
            return ApprovalDecision::Rejected {
                reason: format!(
                    "{} asks to be answered and there is no terminal here; rerun with --yes \
                     where that is what you mean",
                    request.summary
                ),
            };
        }
        eprintln!();
        eprintln!("  {}", request.summary);
        for line in request.detail.lines() {
            eprintln!("    {line}");
        }
        eprint!("  [y] yes  [a] yes for the rest of this run  [n] no: ");
        let mut line = String::new();
        match BufReader::new(tokio::io::stdin())
            .read_line(&mut line)
            .await
        {
            // An input that closed while the question was on screen has said
            // nothing, and a run must not read silence as consent.
            Ok(0) => ApprovalDecision::Rejected {
                reason: format!("{} was asked and no answer came", request.summary),
            },
            Ok(_) => decide(&line),
            Err(source) => ApprovalDecision::Rejected {
                reason: format!("the answer could not be read: {source}"),
            },
        }
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

fn decide(line: &str) -> ApprovalDecision {
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Approved,
        "a" | "always" => ApprovalDecision::ApprovedForRun,
        "" => ApprovalDecision::Rejected {
            reason: "no answer was given".to_owned(),
        },
        other => ApprovalDecision::Rejected {
            reason: format!("the answer was {other:?}, which is not a yes"),
        },
    }
}

#[async_trait]
impl Approver for Terminal {
    async fn approve(&self, request: &ApprovalRequest) -> ApprovalDecision {
        self.ask(request).await
    }
}

/// The live account of a run, one line per event worth a person's attention.
///
/// A TUI will render the same events as widgets; this prints them, which is why
/// it holds no state of its own beyond whether to speak at all.
pub struct Printer {
    /// Silent for `--json`, whose stdout is a document.
    quiet: bool,
}

impl Printer {
    pub fn new(quiet: bool) -> Self {
        Self { quiet }
    }
}

impl Observer for Printer {
    fn observe(&self, event: &AgentEvent) {
        if self.quiet {
            return;
        }
        match event {
            AgentEvent::Started { task, .. } => eprintln!("task      {task}"),
            AgentEvent::PlanSet { plan } => {
                for line in plan.render().lines() {
                    eprintln!("  {line}");
                }
            }
            AgentEvent::PlanAdvanced { .. } => {}
            AgentEvent::ContextGathered { files, tokens } => {
                eprintln!("context   {} file(s), {tokens} token(s)", files.len());
            }
            // A run that does not stream has nothing to print per token.
            AgentEvent::Delta { .. } => {}
            AgentEvent::Message { text } => {
                for line in text.lines() {
                    eprintln!("  {line}");
                }
            }
            AgentEvent::ToolRequested { name, arguments } => {
                eprintln!("call      {name} {}", brief(arguments));
            }
            // The approver prints the question; printing it twice reads as two.
            AgentEvent::ApprovalNeeded { .. } => {}
            AgentEvent::ApprovalGiven { decision } => {
                eprintln!("answered  {}", wording(decision));
            }
            AgentEvent::ToolFinished { result } => eprintln!("{}", line(result)),
            AgentEvent::DiffProposed { diff, files } => {
                eprintln!("proposed change to {}:", files.join(", "));
                for line in diff.lines() {
                    eprintln!("  {line}");
                }
            }
            AgentEvent::ChangesApplied { files } => {
                for file in files {
                    eprintln!("wrote     {file:?}");
                }
            }
            AgentEvent::RolledBack { files } => {
                for file in files {
                    eprintln!("restored  {file:?}");
                }
            }
            AgentEvent::Reflected { note } => eprintln!("noted     {note}"),
            AgentEvent::Warning { message } => eprintln!("warning   {message}"),
            AgentEvent::Finished { outcome, .. } => {
                eprintln!("finished  {}", outcome_kind(outcome));
            }
        }
    }
}

/// One call, in the few characters that say what it was: the path or the
/// program, not the whole argument tree.
fn brief(arguments: &serde_json::Value) -> String {
    for key in ["path", "file", "directory", "command", "pattern", "goal"] {
        if let Some(value) = arguments.get(key).and_then(|value| value.as_str()) {
            return value.to_owned();
        }
    }
    let text = arguments.to_string();
    if text.chars().count() > 60 {
        return format!("{}…", text.chars().take(60).collect::<String>());
    }
    text
}

fn line(result: &ToolResult) -> String {
    let tail = if result.redactions > 0 {
        format!(", {} secret(s) masked", result.redactions)
    } else {
        String::new()
    };
    format!(
        "{:9} {} ({} ms{tail})",
        status_word(&result.status),
        result.summary(),
        result.duration_ms,
    )
}

fn status_word(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Completed => "ok",
        ToolStatus::Failed { .. } => "failed",
        ToolStatus::Refused { .. } => "refused",
        ToolStatus::AwaitingApproval => "asked",
    }
}

fn wording(decision: &ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Approved => "yes",
        ApprovalDecision::ApprovedForRun => "yes, for the rest of the run",
        ApprovalDecision::Rejected { .. } => "no",
    }
}

fn outcome_kind(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Completed { .. } => "completed",
        Outcome::Proposed { .. } => "proposed",
        Outcome::Blocked { .. } => "blocked",
        Outcome::Failed { .. } => "failed",
        Outcome::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use agent_core::{Permission, ToolCall, ToolOutput};
    use serde_json::json;

    use super::*;

    fn request() -> ApprovalRequest {
        ApprovalRequest::new(
            Permission::Write,
            "write_file asks: change notes.md",
            "+new text",
        )
    }

    #[test]
    fn the_answers_a_person_can_give_are_three_and_a_half() {
        assert!(decide("y\n").approved());
        assert!(matches!(decide("A"), ApprovalDecision::ApprovedForRun));
        assert!(!decide("n").approved());
        assert!(!decide("maybe later").approved());
        assert!(
            !decide("").approved(),
            "pressing enter is not a yes: silence is not consent"
        );
    }

    #[tokio::test]
    async fn a_question_with_no_terminal_to_ask_on_is_answered_no() {
        // Built with the field rather than `new`, so this does not depend on
        // whether whoever ran the test has a terminal.
        let approver = Terminal { interactive: false };
        let decision = approver.approve(&request()).await;
        match decision {
            ApprovalDecision::Rejected { reason } => {
                assert!(reason.contains("no terminal"), "{reason}");
                assert!(reason.contains("--yes"), "and says what to do: {reason}");
            }
            other => panic!("an unanswered question is a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_call_is_shown_by_the_argument_that_names_it() {
        assert_eq!(
            brief(&json!({"path": "src/lib.rs", "content": "a very long body"})),
            "src/lib.rs"
        );
        assert_eq!(brief(&json!({"only": "this"})), r#"{"only":"this"}"#);
        let long = brief(&json!({"junk": "x".repeat(200)}));
        assert!(long.chars().count() <= 61, "{long}");
        assert!(long.ends_with('…'), "an elision says it was cut");
    }

    #[test]
    fn a_result_line_names_the_tool_and_admits_to_masking() {
        let mut result = ToolResult::completed(
            ToolCall::new("fs_read_file", json!({"path": "notes.md"})),
            Permission::ReadOnly,
            ToolOutput::new("body"),
            12,
        );
        result.redactions = 2;
        let line = line(&result);
        assert!(line.contains("fs_read_file"), "{line}");
        assert!(line.contains("ok"), "{line}");
        assert!(line.contains("2 secret(s) masked"), "{line}");
    }
}
