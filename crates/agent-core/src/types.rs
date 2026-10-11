//! The nouns the loop trades in: a task, a plan, a piece of context, an event.
//!
//! Each one exists because a small model needs the work cut into pieces it can
//! hold, and because a human reviewing the run afterwards needs to see the same
//! pieces. `Session` is the transcript that gets written to disk, so everything
//! here has to serialise, and nothing here knows how to talk to a model or run
//! a command.

use std::collections::BTreeSet;
use std::path::PathBuf;

use agent_sandbox::Permission;
use agent_tools::{StepMark, ToolResult};
use serde::{Deserialize, Serialize};
use smollm_core::chat::{approx_token_count, TokenUsage};
use uuid::Uuid;

/// Milliseconds since the epoch, the only timestamp format shared by the audit
/// log, the session file and the TUI's status bar.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// One thing the plan said it would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl StepStatus {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Skipped)
    }
}

/// The state a model marked one of its own steps as, read back from the character
/// the plan is rendered with. The two vocabularies are the same five states on
/// purpose: a model copies what its prompt showed it.
impl From<StepMark> for StepStatus {
    fn from(mark: StepMark) -> Self {
        match mark {
            StepMark::Pending => Self::Pending,
            StepMark::Running => Self::Running,
            StepMark::Done => Self::Done,
            StepMark::Failed => Self::Failed,
            StepMark::Skipped => Self::Skipped,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub title: String,
    pub status: StepStatus,
}

/// What the agent intends to do, kept separate from doing it.
///
/// The split is deliberate: a small model that has to state a plan before it
/// can act produces better actions, and a reader can see where it went off the
/// plan afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub steps: Vec<PlanStep>,
}

impl Plan {
    pub fn new(goal: impl Into<String>, steps: Vec<String>) -> Self {
        Self {
            goal: goal.into(),
            steps: steps
                .into_iter()
                .map(|title| PlanStep {
                    title,
                    status: StepStatus::Pending,
                })
                .collect(),
        }
    }

    pub fn mark(&mut self, index: usize, status: StepStatus) {
        if let Some(step) = self.steps.get_mut(index) {
            step.status = status;
        }
    }

    pub fn completed(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.status == StepStatus::Done)
            .count()
    }

    pub fn is_complete(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|step| step.status.is_finished())
    }

    /// The plan as one short block, which is all of it a small model can afford
    /// to see on every turn.
    pub fn render(&self) -> String {
        let mut lines = vec![format!("Goal: {}", self.goal)];
        for (index, step) in self.steps.iter().enumerate() {
            let mark = match step.status {
                StepStatus::Pending => " ",
                StepStatus::Running => ">",
                StepStatus::Done => "x",
                StepStatus::Failed => "!",
                StepStatus::Skipped => "-",
            };
            lines.push(format!("[{mark}] {}. {}", index + 1, step.title));
        }
        lines.join("\n")
    }
}

/// A file that was chosen as context, and why.
///
/// The reason is not decoration: when a change lands in the wrong file, the
/// reason is what tells you whether the ranking was wrong or the model was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
    pub reason: String,
}

impl ContextFile {
    pub fn tokens(&self) -> usize {
        approx_token_count(&self.content) as usize
    }
}

/// Everything handed to the model as prompt, before the reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundle {
    pub task: String,
    /// The repository as a table of contents: paths and a line each, never
    /// bodies. A small model spends its budget faster on paths than on sense.
    pub repo_map: String,
    pub files: Vec<ContextFile>,
    /// What an earlier run left for this one, kept by `memory_store` and read back
    /// on the first turn. One short line each.
    pub notes: Vec<String>,
}

impl ContextBundle {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            repo_map: String::new(),
            files: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn push_file(&mut self, file: ContextFile) {
        self.files.push(file);
    }

    pub fn token_count(&self) -> usize {
        let mut total = approx_token_count(&self.task) as usize;
        total += approx_token_count(&self.repo_map) as usize;
        for note in &self.notes {
            total += approx_token_count(note) as usize;
        }
        self.files.iter().map(ContextFile::tokens).sum::<usize>() + total
    }

    /// Drop the least relevant files until the bundle fits. Files are ordered by
    /// relevance when they are added, so the tail is the right place to cut.
    pub fn fit_to_budget(&mut self, budget: usize) -> Vec<String> {
        let mut dropped = Vec::new();
        while self.token_count() > budget {
            match self.files.len() {
                0 => break,
                _ => {
                    if let Some(file) = self.files.pop() {
                        dropped.push(file.path);
                    }
                }
            }
        }
        dropped
    }

    pub fn paths(&self) -> Vec<String> {
        self.files.iter().map(|file| file.path.clone()).collect()
    }
}

/// What the loop is asking permission for, in the words a human needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub permission: Permission,
    /// One line: "write src/lib.rs", "run cargo test".
    pub summary: String,
    /// The diff, or the exact command, or the path being deleted.
    pub detail: String,
    pub paths: Vec<PathBuf>,
}

impl ApprovalRequest {
    pub fn new(
        permission: Permission,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            permission,
            summary: summary.into(),
            detail: detail.into(),
            paths: Vec::new(),
        }
    }

    pub fn for_paths(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.paths.extend(paths);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    /// Yes, and stop asking about this class for the rest of the run.
    ApprovedForRun,
    Rejected {
        reason: String,
    },
}

impl ApprovalDecision {
    pub fn approved(&self) -> bool {
        matches!(self, Self::Approved | Self::ApprovedForRun)
    }
}

/// How a run ended. `Proposed` is a success: in `suggest-only` the diff was the
/// deliverable, so it must not be reported as a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Completed { summary: String, steps_taken: usize },
    Proposed { summary: String, files: Vec<String> },
    Blocked { reason: String },
    Failed { reason: String },
    Cancelled,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Proposed { .. })
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Completed { summary, .. } | Self::Proposed { summary, .. } => summary.clone(),
            Self::Blocked { reason } | Self::Failed { reason } => reason.clone(),
            Self::Cancelled => "cancelled".to_owned(),
        }
    }
}

/// One thing that happened, in order, for the transcript and the TUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    Started {
        task: String,
        at_ms: u64,
    },
    PlanSet {
        plan: Plan,
    },
    PlanAdvanced {
        index: usize,
        status: StepStatus,
    },
    ContextGathered {
        files: Vec<String>,
        tokens: usize,
    },
    /// A token of reply, streamed. Not stored in a session file: a transcript
    /// keeps the finished text, not its typing.
    Delta {
        text: String,
    },
    Message {
        text: String,
    },
    ToolRequested {
        name: String,
        arguments: serde_json::Value,
    },
    ApprovalNeeded {
        request: ApprovalRequest,
    },
    ApprovalGiven {
        decision: ApprovalDecision,
    },
    ToolFinished {
        result: ToolResult,
    },
    DiffProposed {
        diff: String,
        files: Vec<String>,
    },
    ChangesApplied {
        files: Vec<PathBuf>,
    },
    RolledBack {
        files: Vec<PathBuf>,
    },
    Reflected {
        note: String,
    },
    Warning {
        message: String,
    },
    Finished {
        outcome: Outcome,
        at_ms: u64,
    },
}

/// A run from start to finish: what happened, what was touched, and what the
/// next turn should already know about it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub task: String,
    pub started_at_ms: u64,
    pub plan: Option<Plan>,
    pub events: Vec<AgentEvent>,
    /// Files this run wrote, newest last, for `smoll rollback`.
    pub touched: Vec<PathBuf>,
    /// Secrets the tools' output contained and masked, counted so a transcript
    /// can say it hid something rather than not mention it.
    pub redactions: usize,
    /// What the run cost, added up across its steps. The loop is the only thing
    /// that sees a provider's usage line, and `smoll task --json` has to print a
    /// number a person can compare against their model's window.
    pub usage: TokenUsage,
}

impl Session {
    pub fn new(task: impl Into<String>) -> Self {
        let task = task.into();
        Self {
            id: Uuid::new_v4(),
            started_at_ms: now_ms(),
            task,
            plan: None,
            events: Vec::new(),
            touched: Vec::new(),
            redactions: 0,
            usage: TokenUsage::default(),
        }
    }

    /// Add one step's usage to the run's total. The loop is the only caller: a
    /// provider reports what it spent, and the session is where the number ends
    /// up for `smoll task --json` and for the TUI's status line.
    pub fn note_usage(&mut self, usage: &TokenUsage) {
        self.usage.prompt_tokens += usage.prompt_tokens;
        self.usage.completion_tokens += usage.completion_tokens;
        self.usage.total_tokens += usage.total_tokens;
    }

    /// Record one event. Every mutation of a session goes through here so the
    /// derived state (touched files, redaction count) cannot drift from the log.
    pub fn record(&mut self, event: AgentEvent) {
        match &event {
            AgentEvent::PlanSet { plan } => self.plan = Some(plan.clone()),
            AgentEvent::PlanAdvanced { index, status } => {
                if let Some(plan) = self.plan.as_mut() {
                    plan.mark(*index, *status);
                }
            }
            AgentEvent::ChangesApplied { files } => {
                for path in files {
                    if !self.touched.contains(path) {
                        self.touched.push(path.clone());
                    }
                }
            }
            AgentEvent::Finished { .. } => {}
            _ => {}
        }
        self.events.push(event);
    }

    pub fn note_redactions(&mut self, count: usize) {
        self.redactions += count;
    }

    pub fn finished(&self) -> Option<&Outcome> {
        self.events.iter().find_map(|event| match event {
            AgentEvent::Finished { outcome, .. } => Some(outcome),
            _ => None,
        })
    }

    /// The paths that were read or written, deduplicated and sorted, which is
    /// what a follow-up prompt needs and what `smoll review` prints.
    pub fn files_involved(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        for event in &self.events {
            match event {
                AgentEvent::ContextGathered { files, .. } => {
                    seen.extend(files.iter().cloned());
                }
                AgentEvent::ToolFinished { result } => {
                    if let Some(path) = result
                        .call
                        .str_arg("path")
                        .or_else(|| result.call.str_arg("file"))
                    {
                        seen.insert(path);
                    }
                }
                AgentEvent::ChangesApplied { files } => {
                    seen.extend(files.iter().map(|path| path.display().to_string()));
                }
                _ => {}
            }
        }
        seen.into_iter().collect()
    }

    /// Short lines for the next turn's context: what was decided and what broke.
    pub fn memory_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for event in &self.events {
            match event {
                AgentEvent::Reflected { note } => lines.push(format!("noted: {note}")),
                AgentEvent::Warning { message } => lines.push(format!("warning: {message}")),
                AgentEvent::ToolFinished { result } if !result.is_success() => {
                    lines.push(format!("stuck: {}", result.summary()));
                }
                _ => {}
            }
        }
        lines
    }

    /// A human-readable recap, used by `smoll task` when it finishes.
    pub fn recap(&self) -> String {
        let tools = self
            .events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolFinished { .. }))
            .count();
        let outcome = self
            .finished()
            .map(Outcome::summary)
            .unwrap_or_else(|| "no outcome recorded".to_owned());
        format!(
            "{outcome}\n  {tools} tool call(s), {} file(s) touched{}",
            self.touched.len(),
            if self.redactions > 0 {
                format!(", {} secret(s) masked", self.redactions)
            } else {
                String::new()
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_renders_as_checkboxes_and_knows_when_it_is_done() {
        let mut plan = Plan::new(
            "add a doc comment",
            vec!["read the file".to_owned(), "write the comment".to_owned()],
        );
        assert!(!plan.is_complete());
        assert_eq!(
            plan.render(),
            "Goal: add a doc comment\n[ ] 1. read the file\n[ ] 2. write the comment"
        );
        plan.mark(0, StepStatus::Done);
        plan.mark(1, StepStatus::Running);
        assert_eq!(plan.completed(), 1);
        assert!(!plan.is_complete(), "a running step is not a finished one");
        assert!(plan.render().contains("[x] 1."), "{}", plan.render());
        assert!(plan.render().contains("[>] 2."), "{}", plan.render());
        plan.mark(1, StepStatus::Skipped);
        assert!(plan.is_complete(), "skipped still ends the step");
        plan.mark(99, StepStatus::Done);
        assert_eq!(plan.completed(), 1, "an out-of-range index changes nothing");
    }

    #[test]
    fn context_accounts_for_its_tokens_and_cuts_the_tail() {
        let mut bundle = ContextBundle::new("rename the helper");
        bundle.repo_map = "src/lib.rs\nsrc/main.rs".to_owned();
        bundle.push_file(ContextFile {
            path: "src/lib.rs".to_owned(),
            content: "fn helper() {}".to_owned(),
            reason: "the task names it".to_owned(),
        });
        bundle.push_file(ContextFile {
            path: "src/main.rs".to_owned(),
            content: "fn main() { helper(); }".to_owned(),
            reason: "the caller".to_owned(),
        });
        let full = bundle.token_count();
        assert!(full > 0);
        assert_eq!(bundle.paths(), vec!["src/lib.rs", "src/main.rs"]);

        let dropped = bundle.fit_to_budget(full - 1);
        assert_eq!(dropped, vec!["src/main.rs"], "the least relevant file goes");
        assert_eq!(bundle.files.len(), 1);

        // A budget nothing can fit stops at an empty bundle rather than looping.
        let dropped = bundle.fit_to_budget(0);
        assert_eq!(dropped, vec!["src/lib.rs"]);
        assert!(bundle.files.is_empty());
        assert_eq!(
            bundle.fit_to_budget(0).len(),
            0,
            "there is nothing left to drop"
        );
    }

    #[test]
    fn a_context_file_reports_its_own_cost() {
        let file = ContextFile {
            path: "a.rs".to_owned(),
            content: "fn main() {} // a short file".to_owned(),
            reason: "the only file".to_owned(),
        };
        assert!(file.tokens() >= 5, "{}", file.tokens());
    }

    #[test]
    fn recording_a_write_touched_files_happens_once_per_path() {
        let mut session = Session::new("fix the test");
        session.record(AgentEvent::Started {
            task: "fix the test".to_owned(),
            at_ms: 1,
        });
        session.record(AgentEvent::PlanSet {
            plan: Plan::new("g", vec!["one".to_owned()]),
        });
        session.record(AgentEvent::PlanAdvanced {
            index: 0,
            status: StepStatus::Done,
        });
        assert_eq!(session.plan.as_ref().expect("set").completed(), 1);

        let path = PathBuf::from("src/lib.rs");
        session.record(AgentEvent::ChangesApplied {
            files: vec![path.clone()],
        });
        session.record(AgentEvent::ChangesApplied {
            files: vec![path.clone()],
        });
        assert_eq!(
            session.touched,
            vec![path],
            "a file touched twice is one entry"
        );
    }

    #[test]
    fn a_session_collects_the_files_it_saw_from_three_kinds_of_event() {
        let mut session = Session::new("explain this");
        session.record(AgentEvent::ContextGathered {
            files: vec!["src/b.rs".to_owned()],
            tokens: 12,
        });
        session.record(AgentEvent::ToolFinished {
            result: ToolResult::completed(
                agent_tools::ToolCall::new("fs_read_file", serde_json::json!({"path": "src/a.rs"})),
                Permission::ReadOnly,
                agent_tools::ToolOutput::new("fn a() {}"),
                3,
            ),
        });
        session.record(AgentEvent::ChangesApplied {
            files: vec![PathBuf::from("src/c.rs")],
        });
        assert_eq!(
            session.files_involved(),
            vec!["src/a.rs", "src/b.rs", "src/c.rs"],
            "sorted and deduplicated, whatever the order they arrived in"
        );
    }

    #[test]
    fn memory_lines_keep_the_failures_and_nothing_useful_is_lost() {
        let mut session = Session::new("add a trait");
        session.record(AgentEvent::Reflected {
            note: "the test needs a fixture first".to_owned(),
        });
        session.record(AgentEvent::ToolFinished {
            result: ToolResult::failed(
                agent_tools::ToolCall::new("cargo_test", serde_json::json!({})),
                Permission::Execute,
                "1 test failed",
                agent_tools::ToolOutput::empty(),
            ),
        });
        session.record(AgentEvent::Warning {
            message: "context is 90% full".to_owned(),
        });
        let lines = session.memory_lines();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines.iter().any(|l| l.starts_with("stuck: cargo_test")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.starts_with("noted:")), "{lines:?}");
    }

    #[test]
    fn a_proposal_counts_as_success_because_it_was_the_job() {
        assert!(Outcome::Proposed {
            summary: "here is the diff".to_owned(),
            files: vec!["src/lib.rs".to_owned()],
        }
        .succeeded());
        assert!(Outcome::Completed {
            summary: "done".to_owned(),
            steps_taken: 4,
        }
        .succeeded());
        assert!(!Outcome::Failed {
            reason: "the tests still fail".to_owned(),
        }
        .succeeded());
        assert!(!Outcome::Cancelled.succeeded());
        assert_eq!(
            Outcome::Blocked {
                reason: "approval rejected".to_owned(),
            }
            .summary(),
            "approval rejected"
        );
    }

    #[test]
    fn the_recap_counts_calls_and_admits_to_masking() {
        let mut session = Session::new("rename it");
        session.note_redactions(2);
        session.record(AgentEvent::ToolFinished {
            result: ToolResult::completed(
                agent_tools::ToolCall::new(
                    "fs_patch_file",
                    serde_json::json!({"path": "src/lib.rs"}),
                ),
                Permission::Write,
                agent_tools::ToolOutput::new("patched"),
                9,
            ),
        });
        session.record(AgentEvent::Finished {
            outcome: Outcome::Completed {
                summary: "renamed in 3 files".to_owned(),
                steps_taken: 3,
            },
            at_ms: 2,
        });
        let recap = session.recap();
        assert!(recap.starts_with("renamed in 3 files"), "{recap}");
        assert!(recap.contains("2 secret(s) masked"), "{recap}");
        assert!(recap.contains("1 tool call(s), 0 file(s)"), "{recap}");

        let mut quiet = Session::new("nothing");
        assert_eq!(
            quiet.recap(),
            "no outcome recorded\n  0 tool call(s), 0 file(s) touched"
        );
        quiet.record(AgentEvent::Started {
            task: "nothing".to_owned(),
            at_ms: 1,
        });
        assert!(quiet.finished().is_none());
    }

    #[test]
    fn events_survive_a_round_trip_through_json() {
        let mut session = Session::new("do the thing");
        session.record(AgentEvent::PlanSet {
            plan: Plan::new("goal", vec!["step".to_owned()]),
        });
        session.record(AgentEvent::ApprovalNeeded {
            request: ApprovalRequest::new(Permission::Write, "write src/lib.rs", "@@ diff @@")
                .for_paths([PathBuf::from("src/lib.rs")]),
        });
        session.record(AgentEvent::ApprovalGiven {
            decision: ApprovalDecision::ApprovedForRun,
        });
        session.record(AgentEvent::ToolFinished {
            result: ToolResult::refused(
                agent_tools::ToolCall::new("fs_delete_file", serde_json::json!({"path": "x"})),
                Permission::Destructive,
                "outside the workspace",
            ),
        });
        session.record(AgentEvent::Finished {
            outcome: Outcome::Completed {
                summary: "ok".to_owned(),
                steps_taken: 1,
            },
            at_ms: 3,
        });

        let json = serde_json::to_string(&session).expect("serialisable");
        let back: Session = serde_json::from_str(&json).expect("deserialisable");
        assert_eq!(back, session);
        assert_eq!(back.events.len(), 5);
        assert!(json.contains("\"event\":\"approval_needed\""), "{json}");
        assert!(json.contains("\"decision\":\"approved_for_run\""), "{json}");
        assert!(
            json.contains("\"outcome\":\"completed\""),
            "the outcome is tagged, not positional"
        );
        assert!(
            !back.plan.expect("kept").is_complete(),
            "a set plan is not yet a finished one"
        );
    }

    #[test]
    fn a_sessions_total_is_the_sum_of_what_each_step_spent() {
        let mut session = Session::new("count the tokens");
        assert_eq!(session.usage, TokenUsage::default());
        session.note_usage(&TokenUsage::new(100, 20));
        session.note_usage(&TokenUsage::new(140, 8));
        assert_eq!(session.usage.prompt_tokens, 240);
        assert_eq!(session.usage.completion_tokens, 28);
        assert_eq!(
            session.usage.total_tokens, 268,
            "every step's prompt is paid again, and the total says so"
        );

        let json = serde_json::to_string(&session).expect("serialisable");
        assert!(json.contains("\"promptTokens\":240"), "{json}");
    }

    #[test]
    fn now_is_millisecond_granular_and_monotonic_enough_to_order() {
        let first = now_ms();
        let second = now_ms();
        assert!(second >= first, "{first} then {second}");
        assert!(first > 1_700_000_000_000, "this is 2023 or later: {first}");
    }
}
