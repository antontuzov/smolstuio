//! What the screen knows, kept apart from how it looks.
//!
//! Every field here is the result of an event the loop already emitted, folded in
//! the same order a session file records them, and no field is a colour, a
//! rectangle or a widget. That split is the reason this module can be tested
//! without a terminal: a test feeds in the events a scripted provider causes and
//! asserts on the model, and a separate test checks the drawing of that model.
//!
//! It is also where the screen's two promises live.
//!
//! - **Nothing here is invented.** The status bar shows the tokens the loop
//!   reported for the first turn plus what has arrived since, counted with the
//!   same estimator the loop uses, and puts a `~` in front of it. It is an
//!   estimate of what the model is being shown, not the provider's usage line.
//! - **The picture is bounded.** Transcript, log and diff keep their newest rows
//!   up to a cap, because a run that goes 20 steps should not grow without limit
//!   in memory while a person is watching it.

use agent_core::{now_ms, AgentEvent, ApprovalDecision, ApprovalRequest, Outcome, Plan};
use smollm_core::chat::approx_token_count;

use crate::hub::Msg;
use crate::theme::Palette;

/// How many rows each pane keeps. The newest lines are the ones a person is
/// reading, so the oldest are the ones dropped.
const MAX_TRANSCRIPT: usize = 400;
const MAX_LOG: usize = 200;
const MAX_DIFF: usize = 300;

/// What a line on screen means, which is the only thing the renderer needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Something the person typed.
    Task,
    /// The model's own words.
    Agent,
    /// A tool call that completed.
    Tool,
    /// A tool call that failed or was refused, and anything the run says it could
    /// not do.
    Trouble,
    /// A caution that is not a failure.
    Warning,
    /// A heading: a plan, an outcome, the notice that a turn ended.
    Heading,
    /// What happened around the work, in faint type.
    Muted,
}

/// One row of text with its meaning attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    pub text: String,
}

impl Line {
    pub fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }

    /// The glyph that marks whose voice a transcript line is in.
    pub fn prefix(&self) -> &'static str {
        match self.kind {
            Kind::Task => "› ",
            Kind::Agent => "│ ",
            Kind::Tool => "  ",
            Kind::Trouble => "! ",
            Kind::Warning => "* ",
            Kind::Heading => "= ",
            Kind::Muted => ". ",
        }
    }

    pub fn width(&self) -> usize {
        self.text.chars().count()
    }
}

/// What the screen was told about the run before it started, which is not
/// anything the loop emits.
///
/// A TUI has to say who is answering, on which branch, under which approval mode,
/// and how much window there is — and none of that is an event. The CLI that
/// builds the agent fills this in, so the screen never has to guess or ask a
/// process.
#[derive(Debug, Clone)]
pub struct Header {
    /// "smoll 0.1.0", so a screenshot says which build made it.
    pub agent: String,
    /// The provider, with the model in it when the provider reports one.
    pub provider: String,
    pub workspace: String,
    /// The branch from `.git/HEAD`, or "none" when there is nothing true to say.
    pub branch: String,
    pub mode: String,
    /// The window this run plans against, in tokens.
    pub window: usize,
    /// How many tools the run was offered, and what their definitions cost.
    pub tools: usize,
    pub tool_tokens: usize,
    /// The palette the screen opens in; `/theme` and `smoll chat --theme` move it.
    pub palette: Palette,
    /// The line that must be on screen when no real model is answering.
    pub caution: Option<String>,
}

impl Default for Header {
    fn default() -> Self {
        Self {
            agent: "smoll".to_owned(),
            provider: "unknown".to_owned(),
            workspace: ".".to_owned(),
            branch: "none".to_owned(),
            mode: "approve-edits".to_owned(),
            window: 4096,
            tools: 0,
            tool_tokens: 0,
            palette: Palette::default(),
            caution: None,
        }
    }
}

/// Whether the screen may take a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Running,
}

/// The seven numbers a person watches while a run is going.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub started_at_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub current_tool: Option<String>,
    pub context_tokens: usize,
    pub turns: usize,
    pub files: usize,
    pub redactions: usize,
}

impl Status {
    /// The run's clock, in the whole seconds a status bar can show.
    pub fn elapsed_seconds(&self) -> u64 {
        self.elapsed_ms / 1000
    }
}

/// The whole screen, as data.
#[derive(Debug, Clone)]
pub struct Screen {
    pub header: Header,
    pub palette: Palette,
    pub transcript: Vec<Line>,
    pub log: Vec<Line>,
    pub plan: Option<Plan>,
    pub diff: Vec<String>,
    pub status: Status,
    /// The question the run is stopped on, if any. Set only by the approver's
    /// path, because that is the one that is actually waiting.
    pub pending: Option<ApprovalRequest>,
    pub input: String,
    pub phase: Phase,
}

impl Screen {
    pub fn new(header: Header) -> Self {
        let palette = header.palette;
        let mut screen = Self {
            header,
            palette,
            transcript: Vec::new(),
            log: Vec::new(),
            plan: None,
            diff: Vec::new(),
            status: Status::default(),
            pending: None,
            input: String::new(),
            phase: Phase::Idle,
        };
        screen.push(
            Kind::Muted,
            format!(
                "{} · {} · {} · mode {} · {} tools costing {} tokens a step",
                screen.header.agent,
                screen.header.workspace,
                screen.header.branch,
                screen.header.mode,
                screen.header.tools,
                screen.header.tool_tokens,
            ),
        );
        if let Some(caution) = screen.header.caution.clone() {
            screen.push(Kind::Warning, caution);
        }
        screen.push(
            Kind::Muted,
            "type a task and press Enter · /help lists the commands · q quits",
        );
        screen
    }

    /// Fold one hub message into the model. Approvals arrive here only as events;
    /// the question itself comes from [`Screen::ask`], which is what the loop
    /// blocks on.
    pub fn accept(&mut self, msg: Msg) {
        match msg {
            Msg::Event(event) => self.note(&event),
            Msg::Approval { request, .. } => self.ask(request),
        }
    }

    /// The loop stopped and asked. One question at a time is the loop's own
    /// discipline, so a second question before an answer replaces the first — and
    /// the caller that holds the answer channel must know that happened.
    pub fn ask(&mut self, request: ApprovalRequest) {
        self.push(
            Kind::Heading,
            format!("{} — answer with y, a or n", request.summary),
        );
        self.pending = Some(request);
    }

    /// The person answered. Returns nothing: the caller has the reply channel.
    pub fn answered(&mut self, decision: &ApprovalDecision) {
        self.pending = None;
        self.log(Kind::Muted, format!("answered  {}", wording(decision)));
    }

    /// A turn ended, in the words the caller wants on screen.
    pub fn turn_done(&mut self, message: impl Into<String>, failed: bool) {
        let message = message.into();
        self.status.turns += 1;
        self.status.current_tool = None;
        self.status.started_at_ms = None;
        self.phase = Phase::Idle;
        if message.is_empty() {
            return;
        }
        self.push(if failed { Kind::Trouble } else { Kind::Muted }, message);
    }

    pub fn running(&mut self) {
        self.phase = Phase::Running;
        self.diff.clear();
    }

    /// Empty the panes for a person who wants the next run on a clean screen.
    ///
    /// What the screen said about itself when it opened is put back, because that
    /// is the part that is still true — who is answering, on which branch, under
    /// which mode. The counts are not: they describe the run that was just
    /// cleared away, and a status bar that keeps showing a finished run's numbers
    /// is how a screen lies.
    pub fn clear(&mut self) {
        self.transcript.clear();
        self.log.clear();
        self.diff.clear();
        self.plan = None;
        self.status = Status::default();
        self.push(
            Kind::Muted,
            format!(
                "{} · {} · {} · mode {} · {} tools costing {} tokens a step",
                self.header.agent,
                self.header.workspace,
                self.header.branch,
                self.header.mode,
                self.header.tools,
                self.header.tool_tokens,
            ),
        );
        if let Some(caution) = self.header.caution.clone() {
            self.push(Kind::Warning, caution);
        }
    }

    pub fn push(&mut self, kind: Kind, text: impl Into<String>) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        for line in text.lines() {
            self.transcript.push(Line::new(kind, line));
        }
        trim(&mut self.transcript, MAX_TRANSCRIPT);
    }

    pub fn log(&mut self, kind: Kind, text: impl Into<String>) {
        self.log.push(Line::new(kind, text.into()));
        trim(&mut self.log, MAX_LOG);
    }

    /// The rows the right-hand pane shows: what a question is about while one is
    /// open, otherwise the change the run last proposed.
    pub fn detail(&self) -> (&'static str, Vec<String>) {
        match &self.pending {
            Some(request) => (
                if request.detail.is_empty() {
                    "approval"
                } else {
                    "asking about"
                },
                lines_of(&request.detail),
            ),
            None if !self.diff.is_empty() => ("proposed diff", self.diff.clone()),
            None => ("diff", Vec::new()),
        }
    }

    /// The plan pane, rendered the way the loop renders it for a model.
    pub fn plan_lines(&self) -> Vec<String> {
        self.plan.as_ref().map(|plan| lines_of(&plan.render())).unwrap_or_else(|| lines_of("no plan yet — the scaffold runs until the model writes one, and it can call task_plan_create to replace it"))
    }

    pub fn tick(&mut self) {
        if let Some(started) = self.status.started_at_ms {
            self.status.elapsed_ms = now_ms().saturating_sub(started);
        }
    }

    /// One event at a time, in the order the loop emitted it.
    pub fn note(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Started { task, at_ms } => {
                self.push(Kind::Task, task.clone());
                self.status.started_at_ms = Some(*at_ms);
                self.status.elapsed_ms = 0;
                self.phase = Phase::Running;
            }
            AgentEvent::PlanSet { plan } => {
                self.plan = Some(plan.clone());
                self.push(Kind::Heading, "plan");
            }
            AgentEvent::PlanAdvanced { index, status } => {
                if let Some(plan) = self.plan.as_mut() {
                    plan.mark(*index, *status);
                }
            }
            AgentEvent::ContextGathered { files, tokens } => {
                self.status.context_tokens = *tokens;
                self.status.files = files.len();
                self.log(
                    Kind::Muted,
                    format!("context   {} file(s), {tokens} token(s)", files.len()),
                );
            }
            // A run that does not stream has no deltas; the loop's engine adapter
            // will fill this in when one does, and it goes to the same line.
            AgentEvent::Delta { text } => self.append(text),
            AgentEvent::Message { text } => {
                self.status.context_tokens += approx_token_count(text) as usize;
                self.push(Kind::Agent, text.clone());
            }
            AgentEvent::ToolRequested { name, arguments } => {
                self.status.current_tool = Some(name.clone());
                self.log(Kind::Tool, format!("call      {name} {}", brief(arguments)));
            }
            // The approver's own path opens the question, so printing it here
            // would show one question twice.
            AgentEvent::ApprovalNeeded { .. } => {}
            AgentEvent::ApprovalGiven { decision } => {
                self.log(Kind::Muted, format!("answered  {}", wording(decision)));
            }
            AgentEvent::ToolFinished { result } => {
                self.status.current_tool = None;
                let kind = match result.status {
                    agent_core::ToolStatus::Completed => Kind::Tool,
                    _ => Kind::Trouble,
                };
                self.log.push(Line::new(kind, line_of(result)));
                trim(&mut self.log, MAX_LOG);
                if result.redactions > 0 {
                    self.status.redactions += result.redactions;
                }
            }
            AgentEvent::DiffProposed { diff, files } => {
                self.diff = lines_of(diff);
                trim(&mut self.diff, MAX_DIFF);
                self.log(Kind::Muted, format!("proposed  {}", files.join(", ")));
            }
            AgentEvent::ChangesApplied { files } => {
                for file in files {
                    self.push(Kind::Muted, format!("wrote {:?}", file));
                }
            }
            AgentEvent::RolledBack { files } => {
                for file in files {
                    self.push(Kind::Muted, format!("restored {:?}", file));
                }
            }
            AgentEvent::Reflected { note } => self.log(Kind::Muted, format!("noted     {note}")),
            AgentEvent::Warning { message } => self.push(Kind::Warning, message.clone()),
            AgentEvent::Finished { outcome, at_ms } => {
                let failed = !outcome.succeeded();
                if let Some(started) = self.status.started_at_ms {
                    self.status.elapsed_ms = at_ms.saturating_sub(started);
                }
                self.turn_done(outcome_line(outcome, *at_ms, &self.status), failed);
            }
        }
    }

    /// Streamed text joins the agent line it belongs to rather than starting a
    /// new row per token.
    fn append(&mut self, text: &str) {
        match self.transcript.last_mut() {
            Some(line) if line.kind == Kind::Agent => line.text.push_str(text),
            _ => self.transcript.push(Line::new(Kind::Agent, text)),
        }
        trim(&mut self.transcript, MAX_TRANSCRIPT);
    }
}

/// Drop the oldest rows until the pane fits its cap.
fn trim<T>(rows: &mut Vec<T>, max: usize) {
    if rows.len() > max {
        rows.drain(0..rows.len() - max);
    }
}

/// A source text as rows, with the trailing empty row a `lines()` call would
/// drop kept out, so a pane does not show a blank line at the end.
fn lines_of(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

/// One tool call, in the few characters that name it: the path or the program,
/// not the whole argument tree.
fn brief(arguments: &serde_json::Value) -> String {
    for key in ["path", "file", "directory", "command", "pattern", "goal"] {
        if let Some(value) = arguments.get(key).and_then(|value| value.as_str()) {
            return truncate(value, 60);
        }
    }
    truncate(&arguments.to_string(), 60)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    format!("{}…", text.chars().take(max).collect::<String>())
}

/// A finished call as one row: what it was, how it ended, and what it cost.
fn line_of(result: &agent_core::ToolResult) -> String {
    let tail = if result.redactions > 0 {
        format!(", {} secret(s) masked", result.redactions)
    } else {
        String::new()
    };
    let detail = match &result.status {
        agent_core::ToolStatus::Failed { message } => format!(" — {message}"),
        agent_core::ToolStatus::Refused { reason } => format!(" — {reason}"),
        _ => String::new(),
    };
    format!(
        "{:9} {} ({} ms{tail}){detail}",
        status_word(&result.status),
        result.summary(),
        result.duration_ms,
    )
}

fn status_word(status: &agent_core::ToolStatus) -> &'static str {
    match status {
        agent_core::ToolStatus::Completed => "ok",
        agent_core::ToolStatus::Failed { .. } => "failed",
        agent_core::ToolStatus::Refused { .. } => "refused",
        agent_core::ToolStatus::AwaitingApproval => "asked",
    }
}

fn wording(decision: &ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Approved => "yes",
        ApprovalDecision::ApprovedForRun => "yes, for the rest of the run",
        ApprovalDecision::Rejected { .. } => "no",
    }
}

/// How a run said it ended, with the two counts a person checks it against.
fn outcome_line(outcome: &Outcome, at_ms: u64, status: &Status) -> String {
    let took = status
        .started_at_ms
        .map(|started| (at_ms.saturating_sub(started) / 1000).to_string())
        .unwrap_or_else(|| "0".to_owned());
    format!(
        "{} in {took}s · {} file(s) touched{}",
        outcome.summary(),
        status.files,
        if status.redactions > 0 {
            format!(", {} secret(s) masked", status.redactions)
        } else {
            String::new()
        }
    )
}

#[cfg(test)]
mod tests {
    use agent_core::{Permission, StepStatus, ToolCall, ToolOutput, ToolResult};
    use serde_json::json;

    use super::*;

    /// A screen with a scripted provider's caution on it, which is what the tests
    /// below read.
    fn fresh() -> Screen {
        Screen::new(Header {
            agent: "smoll test".to_owned(),
            tools: 24,
            tool_tokens: 2641,
            caution: Some("no model ran: this provider is scripted".to_owned()),
            ..Default::default()
        })
    }

    fn plan() -> Plan {
        Plan::new(
            "fix the test",
            vec!["read it".to_owned(), "change it".to_owned()],
        )
    }

    #[test]
    fn a_new_screen_says_who_is_answering_and_what_it_costs() {
        let screen = fresh();
        let text: Vec<&str> = screen
            .transcript
            .iter()
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(screen.phase, Phase::Idle);
        assert!(
            text.iter()
                .any(|line| line.contains("24 tools costing 2641 tokens")),
            "{text:?}"
        );
        assert!(
            screen
                .transcript
                .iter()
                .any(|line| line.kind == Kind::Warning && line.text.contains("scripted")),
            "the caution is on screen, not in a README: {text:?}"
        );
    }

    #[test]
    fn the_events_a_run_makes_become_the_panes_a_person_reads() {
        let mut screen = fresh();
        screen.note(&AgentEvent::Started {
            task: "rename the helper".to_owned(),
            at_ms: 1_000,
        });
        screen.note(&AgentEvent::ContextGathered {
            files: vec!["src/lib.rs".to_owned()],
            tokens: 1_200,
        });
        screen.note(&AgentEvent::PlanSet { plan: plan() });
        screen.note(&AgentEvent::PlanAdvanced {
            index: 0,
            status: StepStatus::Done,
        });
        screen.note(&AgentEvent::ToolRequested {
            name: "fs_read_file".to_owned(),
            arguments: json!({"path": "src/lib.rs"}),
        });
        screen.note(&AgentEvent::Message {
            text: "the helper is used twice".to_owned(),
        });
        screen.note(&AgentEvent::DiffProposed {
            diff: "@@ -1 +1 @@\n-old\n+new".to_owned(),
            files: vec!["src/lib.rs".to_owned()],
        });

        assert_eq!(screen.phase, Phase::Running);
        assert!(
            screen.status.context_tokens > 1_200,
            "the estimate starts from what the first turn gathered (1200) and grows with the \
             reply: {}",
            screen.status.context_tokens
        );
        assert!(
            screen
                .log
                .iter()
                .any(|line| line.text.contains("1200 token(s)")),
            "the number the loop reported is on screen"
        );
        assert_eq!(screen.plan.as_ref().expect("a plan").completed(), 1);
        assert_eq!(screen.diff.len(), 3);
        assert_eq!(screen.detail().0, "proposed diff");
        assert!(screen
            .log
            .iter()
            .any(|line| line.text.contains("fs_read_file src/lib.rs")));
        assert!(screen
            .transcript
            .iter()
            .any(|line| { line.kind == Kind::Agent && line.text == "the helper is used twice" }));
    }

    #[test]
    fn a_question_opens_the_dialog_and_the_answer_closes_it() {
        let mut screen = fresh();
        screen.ask(ApprovalRequest::new(
            Permission::Write,
            "write_file asks: change notes.md",
            "+new text\n-more text",
        ));
        assert!(screen.pending.is_some());
        assert_eq!(screen.detail().0, "asking about");
        assert_eq!(screen.detail().1, vec!["+new text", "-more text"]);

        screen.answered(&ApprovalDecision::Rejected {
            reason: "not that file".to_owned(),
        });
        assert!(screen.pending.is_none());
        assert!(
            screen.log.iter().any(|line| line.text.contains("no")),
            "the answer is visible where the question was"
        );
    }

    #[test]
    fn a_hub_message_is_folded_whether_it_is_an_event_or_a_question() {
        let mut screen = fresh();
        screen.accept(Msg::Event(AgentEvent::Warning {
            message: "context is 90% full".to_owned(),
        }));
        screen.accept(Msg::Approval {
            request: ApprovalRequest::new(Permission::Execute, "run cargo test", "cargo test"),
            reply: tokio::sync::oneshot::channel().0,
        });
        assert!(screen.pending.is_some());
        assert!(screen
            .transcript
            .iter()
            .any(|line| line.text.contains("90% full")));
    }

    #[test]
    fn a_refused_call_is_not_drawn_as_a_completed_one() {
        let mut screen = fresh();
        let call = ToolCall::new("fs_delete_file", json!({"path": "../secrets"}));
        screen.note(&AgentEvent::ToolFinished {
            result: ToolResult::refused(
                call.clone(),
                Permission::Destructive,
                "outside the workspace",
            ),
        });
        screen.note(&AgentEvent::ToolFinished {
            result: ToolResult::completed(call, Permission::ReadOnly, ToolOutput::new("body"), 12),
        });
        let refused = &screen.log[screen.log.len() - 2];
        let done = screen.log.last().expect("a line");
        assert_eq!(refused.kind, Kind::Trouble);
        assert!(refused.text.contains("refused"), "{refused:?}");
        assert!(
            refused.text.contains("outside the workspace"),
            "a refusal says why: {refused:?}"
        );
        assert_eq!(done.kind, Kind::Tool);
        assert!(done.text.contains("ok"), "{done:?}");
    }

    #[test]
    fn the_panes_keep_their_newest_rows_and_no_more() {
        let mut screen = fresh();
        for index in 0..(MAX_TRANSCRIPT + 200) {
            screen.push(Kind::Agent, format!("line {index}"));
        }
        assert_eq!(screen.transcript.len(), MAX_TRANSCRIPT);
        assert_eq!(
            screen.transcript.last().expect("kept").text,
            format!("line {}", MAX_TRANSCRIPT + 199),
            "the newest row is the one a person is looking for"
        );
        assert_eq!(screen.transcript[0].text, "line 200");

        let mut screen = fresh();
        for index in 0..(MAX_LOG + 50) {
            screen.log(Kind::Muted, format!("row {index}"));
        }
        assert_eq!(screen.log.len(), MAX_LOG);
    }

    #[test]
    fn a_finished_run_reports_its_own_counts_rather_than_a_smiley() {
        let mut screen = fresh();
        screen.note(&AgentEvent::Started {
            task: "do the thing".to_owned(),
            at_ms: 1_000,
        });
        screen.note(&AgentEvent::ContextGathered {
            files: vec!["a.rs".to_owned(), "b.rs".to_owned()],
            tokens: 10,
        });
        screen.note(&AgentEvent::Finished {
            outcome: Outcome::Completed {
                summary: "renamed in two files".to_owned(),
                steps_taken: 3,
            },
            at_ms: 7_000,
        });
        assert_eq!(screen.phase, Phase::Idle);
        assert_eq!(screen.status.turns, 1);
        assert_eq!(screen.status.elapsed_ms, 6_000, "the run's own clock");
        let last = screen.transcript.last().expect("the outcome");
        assert!(last.text.contains("renamed in two files"), "{last:?}");
        assert!(last.text.contains("in 6s"), "{last:?}");
        assert!(last.text.contains("2 file(s)"), "{last:?}");
    }

    #[test]
    fn a_call_is_named_by_the_argument_that_says_what_it_was() {
        assert_eq!(brief(&json!({"path": "src/lib.rs"})), "src/lib.rs");
        assert_eq!(brief(&json!({"goal": "x".repeat(200)})).chars().count(), 61);
        assert!(brief(&json!({"goal": "x".repeat(200)})).ends_with('…'));
        assert_eq!(brief(&json!({"only": "this"})), r#"{"only":"this"}"#);
    }

    #[test]
    fn streamed_text_joins_the_line_it_is_part_of() {
        let mut screen = fresh();
        screen.note(&AgentEvent::Delta {
            text: "look ".to_owned(),
        });
        screen.note(&AgentEvent::Delta {
            text: "at this".to_owned(),
        });
        let agent_lines: Vec<&Line> = screen
            .transcript
            .iter()
            .filter(|line| line.kind == Kind::Agent)
            .collect();
        assert_eq!(agent_lines.len(), 1, "one reply, not two rows");
        assert_eq!(agent_lines[0].text, "look at this");
    }

    #[test]
    fn a_plan_the_model_never_wrote_says_so_instead_of_showing_an_empty_box() {
        let screen = fresh();
        assert!(screen.plan.is_none());
        assert!(screen.plan_lines().join(" ").contains("task_plan_create"));
        let mut screen = fresh();
        screen.note(&AgentEvent::PlanSet { plan: plan() });
        assert!(screen.plan_lines()[0].starts_with("Goal: fix the test"));
    }

    #[test]
    fn every_kind_of_line_has_a_prefix_that_distinguishes_it() {
        let mut seen = std::collections::HashSet::new();
        for kind in [
            Kind::Task,
            Kind::Agent,
            Kind::Tool,
            Kind::Trouble,
            Kind::Warning,
            Kind::Heading,
            Kind::Muted,
        ] {
            let prefix = Line::new(kind, "x").prefix().to_owned();
            assert!(
                seen.insert(prefix.clone()),
                "{kind:?} shares its prefix: {prefix:?}"
            );
        }
    }

    #[test]
    fn the_tick_only_moves_the_clock_while_a_run_is_going() {
        let mut screen = fresh();
        screen.tick();
        assert_eq!(screen.status.elapsed_ms, 0);
        screen.status.started_at_ms = Some(now_ms() - 2_000);
        screen.tick();
        assert!(screen.status.elapsed_seconds() >= 2);
        assert_eq!(
            screen.status.elapsed_seconds(),
            screen.status.elapsed_ms / 1000
        );
    }

    #[test]
    fn a_message_that_is_empty_is_not_a_row() {
        let mut screen = fresh();
        let before = screen.transcript.len();
        screen.push(Kind::Agent, "");
        assert_eq!(screen.transcript.len(), before);
    }
}
