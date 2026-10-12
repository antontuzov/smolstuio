//! `smoll eval`: the promises this loop exists to keep, run again on demand.
//!
//! One fixture is one promise, stated as a task, a script standing in for the
//! model's answers, and a check on what the run actually did to a real workspace
//! on disk. Every fixture drives the same agent a person drives, with the same
//! registry, policy, sandbox and session record; the only thing swapped out is the
//! model.
//!
//! That swap is the honest limit of this command, and the report names it on every
//! run. A scripted answer proves the loop honoured the promise — that a refusal
//! wrote nothing, that a secret stopped at the tool, that a barred command never
//! started. It says nothing about whether a real model would have tried to keep it,
//! so nothing here prints a number that could be read as a capability score. No
//! answer from real weights is measured by this file.
//!
//! The cost rows are the second half, and they are the reproducible version of the
//! figures the rest of this repository quotes: the price of the tool list and the
//! size of a first turn, read out of the request the loop actually built rather
//! than out of prose about it. They are counted by the same estimator the loop
//! budgets and refuses with, which makes them exact about the loop's own
//! arithmetic and approximate about a real tokenizer — the most a run with no
//! model in it can honestly measure.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;
use tempfile::TempDir;

use agent_config::ApprovalMode;
use agent_core::{Agent, AgentEvent, ApprovalDecision, Limits, Outcome, Run, Scripted};
use agent_providers::{Capabilities, MockProvider, Reply};
use agent_sandbox::Policy;
use agent_tools::{Context, Registry, ToolStatus};

/// The workspace most fixtures run in: two files, one of them in a directory,
/// because most of the promises are about a file being changed or not.
const REPO: &[(&str, &str)] = &[
    ("notes.md", "old text\n"),
    ("src/lib.rs", "pub fn a() {}\n"),
];

/// The same tree plus a file whose contents a credential appears in, for the
/// fixture whose promise is about the mask. The value is not a key: a `KEY=` name
/// that says `token` and a value that is nothing, which is what the redactor keys
/// on.
const SECRET_REPO: &[(&str, &str)] = &[
    ("notes.md", "old text\n"),
    ("src/lib.rs", "pub fn a() {}\n"),
    ("config/deploy.env", "DEPLOY_TOKEN=abc123def456\n"),
];

const SECRET_VALUE: &str = "abc123def456";

/// The one thing a reader of this report must not lose sight of.
pub const SCOPE: &str = "scripted answers: this measures the loop and its limits, not \
     any model. Nothing below is an answer from real weights, and no row is a capability \
     score.";

/// How a fixture answers the approvals a write needs.
#[derive(Clone, Copy)]
enum Ask {
    /// Nothing is asked of a person; the policy still blocks what it blocks.
    Auto,
    /// A person is asked, and says no.
    No,
    /// `suggest-only`: the diff is the deliverable and the tree is not touched.
    Propose,
}

/// What the run did, as far as a checker can see it. The workspace it ran in is
/// scratch, held here so it survives its checker and goes when the trace does.
struct Trace {
    dir: TempDir,
    outcome: String,
    ran: Vec<String>,
    refused: Vec<(String, String)>,
    touched: Vec<String>,
    prompts: Vec<String>,
    prompt_tokens: Vec<u32>,
    total_tokens: u32,
    redactions: usize,
    warnings: Vec<String>,
    error: Option<String>,
    exit_code: u8,
    ms: u64,
}

impl Trace {
    /// What a file holds now, or the empty string if it is gone.
    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }

    fn ran(&self, name: &str) -> bool {
        self.ran.iter().any(|called| called == name)
    }

    fn stopped_by(&self, what: &str) -> bool {
        self.error
            .as_deref()
            .map(|message| message.contains(what))
            .unwrap_or(false)
    }
}

struct Fixture {
    name: &'static str,
    /// The promise, in the words a reader of the report needs.
    promise: &'static str,
    prompt: &'static str,
    files: &'static [(&'static str, &'static str)],
    replies: fn() -> Vec<Reply>,
    /// A model that will not stop needs the script to keep repeating, and a step
    /// budget to stop it.
    repeat_last: bool,
    /// Whether the model is shown a native tool array or told about its tools in
    /// sentences, and the window it was configured with.
    tool_calling: bool,
    window: usize,
    max_steps: usize,
    ask: Ask,
    /// The ways this run broke the promise. Empty means it kept it.
    check: fn(&Trace) -> Vec<String>,
}

/// A model that reads, writes and reports: the shortest run that changes a file.
fn read_write_summarise() -> Vec<Reply> {
    vec![
        Reply::call("read_file", json!({"path": "notes.md"})),
        Reply::call(
            "write_file",
            json!({"path": "notes.md", "content": "new text\n"}),
        ),
        Reply::text("Rewrote notes.md."),
    ]
}

/// One call, then an answer: the shape most of the safety fixtures use, so the
/// check is about what the one call did or did not do.
fn then_answers(name: &str, arguments: serde_json::Value, closing: &str) -> Vec<Reply> {
    vec![Reply::call(name, arguments), Reply::text(closing)]
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureReport {
    name: &'static str,
    promise: &'static str,
    passed: bool,
    problems: Vec<String>,
    outcome: String,
    tools_ran: Vec<String>,
    files_touched: Vec<String>,
    first_turn_tokens: u32,
    total_tokens: u32,
    redactions: usize,
    warnings: Vec<String>,
    ms: u64,
}

/// One row of the cost table: what a first turn costs at one window.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostRow {
    window: usize,
    /// `native` for a model shown a tool array, `prose` for one told in sentences.
    tool_block: &'static str,
    /// `None` when the window is too small for the loop to ask anything at all:
    /// it refused before building a request, so there is no first turn to price.
    first_turn_tokens: Option<u32>,
    /// The loop's reason for a row with no number.
    problem: Option<String>,
    /// The schemas in the request's own tool array: 24 for a native block, 0 for
    /// prose, which pays for the same descriptions inside its system message.
    native_schemas: usize,
    /// Whether the window was wide enough for the map of the repository, and for
    /// the bodies of the files the map names. These are what a small window
    /// trades away first, which is why the table runs down to one that cannot fit.
    repository_map_in_first_turn: bool,
    file_bodies_in_first_turn: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// What the report measures, printed at the head of every run of it.
    pub measures: &'static str,
    pub fixtures: Vec<FixtureReport>,
    pub passed: usize,
    pub failed: usize,
    pub list_tokens: usize,
    pub tools_offered: usize,
    pub cost: Vec<CostRow>,
}

impl Report {
    pub fn failed_any(&self) -> bool {
        self.failed > 0
    }
}

/// The fixtures. Each one is here because the loop makes a promise a plain test
/// could assert once and a reader could then forget; this command keeps them in
/// one table and runs them on demand.
const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "write_lands",
        promise: "a change the model asks for is written, and the run records that it wrote it",
        prompt: "replace the text in notes.md with new text",
        files: REPO,
        replies: read_write_summarise,
        repeat_last: false,
        tool_calling: true,
        window: 8192,
        max_steps: 8,
        ask: Ask::Auto,
        check: |trace| {
            let mut problems = vec![];
            if trace.outcome != "completed" {
                problems.push(format!("the run ended {}", trace.outcome));
            }
            if !trace.ran("write_file") {
                problems.push("write_file never ran".to_owned());
            }
            if trace.file("notes.md") != "new text\n" {
                problems.push(format!(
                    "notes.md holds {:?}, not what the model wrote",
                    trace.file("notes.md")
                ));
            }
            if !trace.touched.iter().any(|path| path.ends_with("notes.md")) {
                problems.push("the session did not record notes.md as touched".to_owned());
            }
            problems
        },
    },
    Fixture {
        name: "suggest_only_touches_nothing",
        promise: "in suggest-only the diff is the answer and the tree is not modified",
        prompt: "replace the text in notes.md with new text",
        files: REPO,
        replies: || {
            then_answers(
                "write_file",
                json!({"path": "notes.md", "content": "new text\n"}),
                "Here is the change I would make.",
            )
        },
        repeat_last: false,
        tool_calling: true,
        window: 8192,
        max_steps: 8,
        ask: Ask::Propose,
        check: |trace| {
            let mut problems = vec![];
            if trace.file("notes.md") != "old text\n" {
                problems.push("a suggest-only run wrote to the file".to_owned());
            }
            if !trace.touched.is_empty() {
                problems.push(format!("it recorded {:?} as touched", trace.touched));
            }
            if trace.outcome != "proposed" {
                problems.push(format!(
                    "a suggest-only run succeeds by proposing, it ended {}",
                    trace.outcome
                ));
            }
            problems
        },
    },
    Fixture {
        name: "a_refusal_writes_nothing",
        promise: "when a person says no, nothing is written and the run says why it stopped",
        prompt: "replace the text in notes.md with new text",
        files: REPO,
        replies: || {
            then_answers(
                "write_file",
                json!({"path": "notes.md", "content": "new text\n"}),
                "I could not do that.",
            )
        },
        repeat_last: false,
        tool_calling: true,
        window: 8192,
        max_steps: 8,
        ask: Ask::No,
        check: |trace| {
            let mut problems = vec![];
            if trace.file("notes.md") != "old text\n" {
                problems.push("the write happened after a refusal".to_owned());
            }
            if trace.outcome != "blocked" {
                problems.push(format!(
                    "a refusal should end the run blocked, it ended {}",
                    trace.outcome
                ));
            }
            if trace.exit_code != 2 {
                problems.push(format!(
                    "a person's refusal is exit 2, not {}",
                    trace.exit_code
                ));
            }
            problems
        },
    },
    Fixture {
        name: "prose_model_still_acts",
        promise: "a model with no native tool array still gets the call it wrote in prose run",
        prompt: "read notes.md and tell me what is in it",
        files: REPO,
        replies: || {
            vec![
                Reply::text(
                    "I will read it.\n```json\n{\"tool\": \"read_file\", \"arguments\": \
                     {\"path\": \"notes.md\"}}\n```",
                ),
                Reply::text("notes.md holds old text."),
            ]
        },
        repeat_last: false,
        tool_calling: false,
        window: 8192,
        max_steps: 8,
        ask: Ask::Auto,
        check: |trace| {
            if trace.ran("read_file") {
                vec![]
            } else {
                vec![format!(
                    "the fenced call never ran; the tools that did were {:?}",
                    trace.ran
                )]
            }
        },
    },
    Fixture {
        name: "an_invented_tool_never_runs",
        promise: "a call naming a tool that was not offered runs nothing at all",
        prompt: "make this repository perfect",
        files: REPO,
        replies: || {
            then_answers(
                "delete_repo",
                json!({}),
                "There was nothing I could do there.",
            )
        },
        repeat_last: false,
        tool_calling: true,
        window: 8192,
        max_steps: 8,
        ask: Ask::Auto,
        check: |trace| {
            let mut problems = vec![];
            if trace.ran.iter().any(|name| name.contains("delete")) {
                problems.push("a delete ran, on a name nobody offered".to_owned());
            }
            // Refusing is not enough on its own: the answer has to tell the model
            // the name does not exist, or it will try the same name again.
            let named_as_missing = trace
                .refused
                .iter()
                .any(|(name, reason)| name == "delete_repo" && reason.contains("no tool named"));
            if !named_as_missing {
                problems.push("nothing told the model the name was not one it has".to_owned());
            }
            if trace.file("notes.md") != "old text\n"
                || trace.file("src/lib.rs") != "pub fn a() {}\n"
            {
                problems.push("a file changed that no offered tool was asked to change".to_owned());
            }
            problems
        },
    },
    Fixture {
        name: "a_repaired_call_still_lands",
        promise: "a call with one obvious JSON defect is repaired and then really runs",
        prompt: "read notes.md and tell me what is in it",
        files: REPO,
        replies: || {
            vec![
                // A trailing comma: the defect the repair step is specified to fix,
                // and the one a small model produces most often.
                Reply::text(
                    "```json\n{\"tool\": \"read_file\", \"arguments\": {\"path\": \"notes.md\",}}\n```",
                ),
                Reply::text("notes.md holds old text."),
            ]
        },
        repeat_last: false,
        tool_calling: false,
        window: 8192,
        max_steps: 8,
        ask: Ask::Auto,
        check: |trace| {
            if trace.ran("read_file") {
                vec![]
            } else {
                vec![format!(
                    "the malformed call was neither repaired nor reported; ran {:?}",
                    trace.ran
                )]
            }
        },
    },
    Fixture {
        name: "a_barred_command_never_starts",
        promise: "a program the sandbox bars is refused before it runs, whoever approved it",
        prompt: "delete notes.md by running a shell command",
        files: REPO,
        replies: || {
            // The bar is on the program, not on its argument: `sudo` is refused in
            // every approval mode, and no allowlist can make it run.
            then_answers(
                "run_command",
                json!({"command": "sudo rm notes.md"}),
                "That is barred.",
            )
        },
        repeat_last: false,
        tool_calling: true,
        window: 8192,
        max_steps: 8,
        ask: Ask::Auto,
        check: |trace| {
            let mut problems = vec![];
            if trace.file("notes.md") != "old text\n" {
                problems.push("a command the policy bars deleted the file".to_owned());
            }
            let refused_for_a_reason = trace
                .refused
                .iter()
                .any(|(_name, reason)| !reason.trim().is_empty());
            if !refused_for_a_reason && !trace.stopped_by("denylist") {
                problems.push("nothing reported why the command did not run".to_owned());
            }
            problems
        },
    },
    Fixture {
        name: "a_secret_stops_at_the_tool",
        promise: "a credential in a tool's output is masked before the model is shown it",
        prompt: "read config/deploy.env and tell me what it sets",
        files: SECRET_REPO,
        replies: || {
            then_answers(
                "read_file",
                json!({"path": "config/deploy.env"}),
                "It sets a deployment token, which came back masked.",
            )
        },
        repeat_last: false,
        tool_calling: true,
        window: 8192,
        max_steps: 8,
        ask: Ask::Auto,
        check: |trace| {
            let mut problems = vec![];
            if trace.redactions == 0 {
                problems.push("the run reported no redaction at all".to_owned());
            }
            for (step, prompt) in trace.prompts.iter().enumerate().skip(1) {
                if prompt.contains(SECRET_VALUE) {
                    problems.push(format!(
                        "step {} was asked with the secret still in its prompt",
                        step + 1
                    ));
                }
            }
            problems
        },
    },
    Fixture {
        name: "a_loop_is_stopped_by_its_budget",
        promise: "a model that will not finish is stopped by its step budget, and says so",
        prompt: "read notes.md over and over",
        files: REPO,
        replies: || vec![Reply::call("read_file", json!({"path": "notes.md"}))],
        repeat_last: true,
        tool_calling: true,
        window: 8192,
        max_steps: 3,
        ask: Ask::Auto,
        check: |trace| {
            let mut problems = vec![];
            if !trace.stopped_by("tool calls") {
                problems.push(format!(
                    "the step budget did not stop the run: {:?}",
                    trace.error.as_deref().unwrap_or("no error")
                ));
            }
            if trace.ran.len() < 3 {
                problems.push(format!(
                    "the run ran {} of the 3 steps it was given",
                    trace.ran.len()
                ));
            }
            if trace.exit_code != 1 {
                problems.push(format!("a budget is exit 1, not {}", trace.exit_code));
            }
            problems
        },
    },
];

/// A run's own word for how it ended, or the absence of one. The mapping is the
/// CLI's, so `smoll eval` and `smoll review` cannot disagree about a verdict.
fn label(outcome: Option<&Outcome>) -> &'static str {
    outcome.map(crate::outcome_kind).unwrap_or("unfinished")
}

/// A workspace with these files in it, for one fixture run.
fn make_repo(dir: &Path, files: &[(&str, &str)]) -> Result<(), String> {
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| format!("cannot make {}: {source}", parent.display()))?;
        }
        std::fs::write(&path, content)
            .map_err(|source| format!("cannot write {}: {source}", path.display()))?;
    }
    Ok(())
}

/// One fixture, run.
async fn execute(fixture: &Fixture) -> Result<Trace, String> {
    let dir = TempDir::new().map_err(|source| format!("no workspace to test in: {source}"))?;
    make_repo(dir.path(), fixture.files)?;

    let mut provider =
        MockProvider::scripted((fixture.replies)()).with_capabilities(Capabilities {
            streaming: false,
            tool_calling: fixture.tool_calling,
            context_tokens: fixture.window,
        });
    if fixture.repeat_last {
        // A model that never stops asking is what a step budget exists for.
        provider = provider.repeat_last();
    }
    let provider = Arc::new(provider);

    let ctx = match fixture.ask {
        Ask::Propose => Context::new(dir.path())
            .with_policy(Policy::new(dir.path()).with_approval(ApprovalMode::SuggestOnly)),
        _ => Context::new(dir.path()),
    };
    let mut agent =
        Agent::new(provider.clone(), Registry::with_defaults(), ctx).with_limits(Limits {
            max_steps: fixture.max_steps,
            max_context_tokens: fixture.window,
            timeout: Duration::from_secs(30),
            ..Limits::default()
        });
    agent = match fixture.ask {
        Ask::Auto | Ask::Propose => agent.autoapprove(),
        Ask::No => agent.with_approver(Arc::new(Scripted::always(ApprovalDecision::Rejected {
            reason: "smoll eval asks whether a refusal is obeyed, and refuses".to_owned(),
        }))),
    };

    let started = Instant::now();
    let Run { session, error } = agent.run(fixture.prompt).await;
    let ms = started.elapsed().as_millis() as u64;

    let requests = provider.requests();
    let prompts = requests
        .iter()
        .map(|request| {
            request
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    let prompt_tokens = requests
        .iter()
        .map(|request| request.prompt_tokens())
        .collect();

    let mut ran = Vec::new();
    let mut refused = Vec::new();
    for event in &session.events {
        if let AgentEvent::ToolFinished { result } = event {
            // A call that was refused never reached a tool, so it is not evidence
            // that something ran. `Failed` is: the tool executed and what it
            // attempted did not work.
            match &result.status {
                ToolStatus::Completed | ToolStatus::Failed { .. } => {
                    ran.push(result.call.name.clone())
                }
                ToolStatus::Refused { reason } => {
                    refused.push((result.call.name.clone(), reason.clone()))
                }
                ToolStatus::AwaitingApproval => {}
            }
        }
    }
    let warnings = session
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect();

    Ok(Trace {
        outcome: label(session.finished()).to_owned(),
        ran,
        dir,
        refused,
        touched: session
            .touched
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        prompts,
        prompt_tokens,
        total_tokens: session.usage.total_tokens,
        redactions: session.redactions,
        warnings,
        exit_code: error.as_ref().map(|source| source.exit_code()).unwrap_or(0),
        error: error.map(|source| source.to_string()),
        ms,
    })
}

/// Run every fixture, then the cost rows.
pub async fn run_all() -> Report {
    let mut fixtures = Vec::new();
    let mut passed = 0;
    let mut failed = 0;
    for fixture in FIXTURES {
        let report = match execute(fixture).await {
            Ok(trace) => {
                let problems = (fixture.check)(&trace);
                let first_turn_tokens = trace.prompt_tokens.first().copied().unwrap_or(0);
                let passed_now = problems.is_empty();
                // The workspace goes with the rest of the trace, which is after its
                // checker has read the files out of it.
                let Trace {
                    outcome,
                    ran,
                    touched,
                    total_tokens,
                    redactions,
                    warnings,
                    ms,
                    ..
                } = trace;
                FixtureReport {
                    name: fixture.name,
                    promise: fixture.promise,
                    passed: passed_now,
                    problems,
                    outcome,
                    tools_ran: ran,
                    files_touched: touched,
                    first_turn_tokens,
                    total_tokens,
                    redactions,
                    warnings,
                    ms,
                }
            }
            Err(problem) => FixtureReport {
                name: fixture.name,
                promise: fixture.promise,
                passed: false,
                problems: vec![problem],
                outcome: "not run".to_owned(),
                tools_ran: vec![],
                files_touched: vec![],
                first_turn_tokens: 0,
                total_tokens: 0,
                redactions: 0,
                warnings: vec![],
                ms: 0,
            },
        };
        if report.passed {
            passed += 1;
        } else {
            failed += 1;
        }
        fixtures.push(report);
    }
    let registry = Registry::with_defaults();
    Report {
        measures: SCOPE,
        fixtures,
        passed,
        failed,
        list_tokens: registry.token_cost(),
        tools_offered: registry.len(),
        cost: cost_rows().await,
    }
}

/// Every first turn the documentation quotes, produced by one command. The
/// windows run from one a 4B model would be configured with down to one too small
/// to hold the tool list, because the interesting number is where the map and the
/// file bodies fall out.
async fn cost_rows() -> Vec<CostRow> {
    let mut rows = Vec::new();
    for window in [8192, 4096, 3584, 3072, 2048] {
        for tool_calling in [true, false] {
            rows.push(cost_at(window, tool_calling).await);
        }
    }
    rows
}

/// The cost of one first turn at one window, for a model that can hold a tool
/// array and one that has to be told about its tools in sentences.
async fn cost_at(window: usize, tool_calling: bool) -> CostRow {
    let mut row = CostRow {
        window,
        tool_block: if tool_calling { "native" } else { "prose" },
        first_turn_tokens: None,
        problem: None,
        native_schemas: 0,
        repository_map_in_first_turn: false,
        file_bodies_in_first_turn: false,
    };
    let Ok(dir) = TempDir::new() else {
        return row;
    };
    if make_repo(dir.path(), REPO).is_err() {
        return row;
    }
    let provider = Arc::new(
        MockProvider::scripted([Reply::text("Noted.")]).with_capabilities(Capabilities {
            streaming: false,
            tool_calling,
            context_tokens: window,
        }),
    );
    let mut agent = Agent::new(
        provider.clone(),
        Registry::with_defaults(),
        Context::new(dir.path()),
    )
    .with_limits(Limits {
        max_steps: 2,
        max_context_tokens: window,
        timeout: Duration::from_secs(30),
        ..Limits::default()
    })
    .autoapprove();
    let Run { error, .. } = agent.run("say something about notes.md").await;
    if let Some(request) = provider.requests().first() {
        row.first_turn_tokens = Some(request.prompt_tokens());
        row.native_schemas = request.tools.len();
        let asked = request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        row.repository_map_in_first_turn = asked.contains("Repository map:");
        row.file_bodies_in_first_turn = asked.contains("### notes.md");
    } else {
        row.problem = error.map(|source| source.to_string());
    }
    row
}

fn yes(flag: bool) -> &'static str {
    if flag {
        "yes"
    } else {
        "no"
    }
}

/// `smoll eval` for a human: one line per fixture, then the cost table.
pub fn print(report: &Report) {
    println!("measures: {}", report.measures);
    println!();
    for fixture in &report.fixtures {
        let verdict = if fixture.passed { "ok  " } else { "FAIL" };
        println!(
            "[{verdict}] {:<34} {:<10} {:>2} tools, {:>5} tokens, {} ms",
            fixture.name,
            fixture.outcome,
            fixture.tools_ran.len(),
            fixture.first_turn_tokens,
            fixture.ms
        );
        println!("       {}", fixture.promise);
        for problem in &fixture.problems {
            println!("       - {problem}");
        }
        for warning in &fixture.warnings {
            println!("       ! {warning}");
        }
    }
    println!();
    println!(
        "{} passed, {} failed. The default tool list costs {} tokens on every step of a \
         run, across {} tools.",
        report.passed, report.failed, report.list_tokens, report.tools_offered
    );
    println!();
    println!("what one first turn costs, counted the way the loop counts it");
    println!(
        "{:>8}  {:>7}  {:>11}  {:>7}  {:>6}  bodies",
        "window", "block", "first turn", "schemas", "map"
    );
    for row in &report.cost {
        println!(
            "{:>8}  {:>7}  {:>11}  {:>7}  {:>6}  {}",
            row.window,
            row.tool_block,
            match row.first_turn_tokens {
                Some(tokens) => tokens.to_string(),
                None => "-".to_owned(),
            },
            row.native_schemas,
            yes(row.repository_map_in_first_turn),
            yes(row.file_bodies_in_first_turn)
        );
        if let Some(problem) = &row.problem {
            println!("       {problem}");
        }
    }
    println!();
    println!(
        "The first-turn column is the loop's own arithmetic: the messages it built, plus \
         each tool's name, description and parameter schema, at four characters to a \
         token. It is what the loop budgets and refuses with, and it is an estimate of \
         what a real tokenizer would say — the JSON a server wraps each schema in is not \
         in the sum, which is why the budget keeps a reserve. The last two columns say \
         whether the window was wide enough to keep the repository map, and the bodies \
         of the files it names; a run that drops them is not told about the repository \
         any more. No model was run to make this table."
    );
}
