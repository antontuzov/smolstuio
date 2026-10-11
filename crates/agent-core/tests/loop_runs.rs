//! The loop, end to end, against the scripted provider.
//!
//! Every test here drives a real run: a temp workspace on disk, the real
//! registry and policy, and a mock whose script stands in for a small model's
//! answers. What they check is the contract the loop exists to keep — that a
//! change lands and can be undone, that a proposal in `suggest-only` writes
//! nothing, that a person's answer is remembered, and that a run which cannot
//! go on says which limit stopped it rather than inventing a way past it.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::TempDir;

use agent_config::ApprovalMode;
use agent_core::{
    Agent, AgentError, AgentEvent, ApprovalDecision, Budget, Limits, Observer, Outcome, Run,
    Scripted, Session, StepStatus, StoredSession,
};
use agent_providers::{Capabilities, MockProvider, Reply};
use agent_sandbox::Policy;
use agent_tools::{Context, Registry, ToolStatus};
use smollm_core::chat::CancelToken;

/// A workspace with these files in it, deleted when the test ends.
fn repo(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("a temp workspace");
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a directory");
        }
        std::fs::write(path, content).expect("writable");
    }
    dir
}

fn read(dir: &TempDir, name: &str) -> String {
    std::fs::read_to_string(dir.path().join(name)).unwrap_or_else(|source| {
        panic!("{name} is not readable: {source}");
    })
}

/// An agent over the twenty default tools, with the mock as its model.
fn agent(provider: Arc<MockProvider>, ctx: Context) -> Agent {
    Agent::new(provider, Registry::with_defaults(), ctx)
}

/// The scripted provider behind a handle a test can interrogate afterwards.
fn scripted(replies: impl IntoIterator<Item = Reply>) -> Arc<MockProvider> {
    Arc::new(MockProvider::scripted(replies))
}

/// A model that stops talking after one change: read, write, summarise.
fn read_write_summarise(dir: &TempDir, content: &str) -> (Agent, Arc<MockProvider>) {
    let provider = scripted([
        Reply::call("read_file", json!({"path": "notes.md"})),
        Reply::call(
            "write_file",
            json!({"path": "notes.md", "content": content}),
        ),
        Reply::text("Rewrote notes.md."),
    ]);
    let agent = agent(provider.clone(), Context::new(dir.path())).autoapprove();
    (agent, provider)
}

#[derive(Default)]
struct Collector(Mutex<Vec<AgentEvent>>);

impl Observer for Collector {
    fn observe(&self, event: &AgentEvent) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event.clone());
    }
}

impl Collector {
    fn events(&self) -> Vec<AgentEvent> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// The messages of the first prompt, joined, for asserting on what the model saw.
fn first_prompt(provider: &MockProvider) -> String {
    let request = &provider.requests()[0];
    request
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn warnings(session: &Session) -> Vec<String> {
    session
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// The transcript of a run that was expected to carry its task through.
fn finished(run: Run) -> Session {
    match run.error {
        None => run.session,
        Some(error) => panic!("the run was expected to finish: {error}"),
    }
}

/// The error a run was expected to stop on.
fn stopped(run: Run) -> AgentError {
    run.error.expect("the run was expected to stop")
}

#[tokio::test]
async fn a_run_reads_then_writes_then_says_what_it_did() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let (mut agent, provider) = read_write_summarise(&dir, "new text\n");
    let session = finished(agent.run("replace the text in notes.md").await);

    assert_eq!(read(&dir, "notes.md"), "new text\n", "the change landed");
    assert_eq!(
        session.touched,
        vec![Path::new("notes.md").to_owned()],
        "and the transcript names it as the repository does"
    );
    let steps = match session.finished() {
        Some(Outcome::Completed { steps_taken, .. }) => *steps_taken,
        other => panic!("expected a completed run, got {other:?}"),
    };
    assert_eq!(steps, 3, "read, write, summarise");
    assert!(
        session.recap().starts_with("Rewrote notes.md."),
        "{}",
        session.recap()
    );
    assert_eq!(provider.remaining(), 0, "the script was used exactly up");

    let plan = session.plan.as_ref().expect("a plan was set");
    assert!(plan.is_complete(), "{}", plan.render());
    assert!(
        session.events.iter().any(
            |event| matches!(event, AgentEvent::ChangesApplied { files } if !files.is_empty())
        ),
        "the write is its own event"
    );
    assert!(session.usage.total_tokens > 0, "the run knows what it cost");
}

#[tokio::test]
async fn a_rollback_puts_the_tree_back_the_way_it_was() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let (mut agent, _) = read_write_summarise(&dir, "new text\n");
    let mut session = finished(agent.run("replace the text in notes.md").await);
    assert_eq!(read(&dir, "notes.md"), "new text\n");

    let undone = agent.rollback(&mut session);
    assert_eq!(undone, vec![Path::new("notes.md").to_owned()]);
    assert_eq!(read(&dir, "notes.md"), "old text\n", "the run is undone");
    assert!(
        session
            .events
            .iter()
            .any(|event| matches!(event, AgentEvent::RolledBack { .. })),
        "and the transcript says so"
    );
}

#[tokio::test]
async fn the_first_prompt_shows_the_map_the_files_and_the_plan() {
    let dir = repo(&[
        ("notes.md", "old text\n"),
        ("src/lib.rs", "pub fn a() {}\n"),
    ]);
    let (mut agent, provider) = read_write_summarise(&dir, "new text\n");
    finished(agent.run("replace the text in notes.md").await);

    let prompt = first_prompt(&provider);
    assert!(
        prompt.contains("Task: replace the text in notes.md"),
        "{prompt}"
    );
    assert!(prompt.contains("Repository map:"), "{prompt}");
    assert!(prompt.contains("notes.md"), "the file is named");
    assert!(
        prompt.find("Repository map:").expect("the map")
            < prompt.find("### notes.md").expect("the body"),
        "the table of contents comes before the bodies"
    );
    assert!(
        prompt.contains("[>] 1."),
        "the plan is already on its first step: {prompt}"
    );
    assert!(prompt.contains("[ ] 4."), "and the rest is still ahead");
    assert!(
        prompt.contains("What does your first step need?"),
        "{prompt}"
    );
}

#[tokio::test]
async fn a_model_that_has_to_be_told_the_tools_in_prose_still_gets_the_map() {
    // The expensive half of the budget: a model with no native tool array is
    // shown every description as sentences, because that list is its only tool
    // documentation. Twenty of them against a 4096-token window is what used to
    // leave no room for the repository at all.
    let dir = repo(&[
        ("notes.md", "old text\n"),
        ("src/lib.rs", "pub fn a() {}\n"),
    ]);
    let provider = Arc::new(
        MockProvider::scripted([
            Reply::text(
                "```json\n{\"tool\": \"read_file\", \"arguments\": {\"path\": \"notes.md\"}}\n```",
            ),
            Reply::text("Read it; the change itself needs a person to make it."),
        ])
        .with_capabilities(Capabilities::basic().window(4096)),
    );
    let mut agent = agent(provider.clone(), Context::new(dir.path())).autoapprove();
    finished(agent.run("replace the text in notes.md").await);

    let prompt = first_prompt(&provider);
    assert!(
        prompt.contains("reply with exactly one JSON object"),
        "the list is prose because nothing else carries it"
    );
    assert!(prompt.contains("Repository map:"), "{prompt}");
    assert!(
        prompt.contains("notes.md") && prompt.contains("src/lib.rs"),
        "the map names both files: {prompt}"
    );
}

#[tokio::test]
async fn a_model_without_function_calling_is_asked_in_prose_and_understood() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let provider = Arc::new(
        MockProvider::scripted([Reply::text(
            "```json\n{\"tool\": \"write_file\", \"arguments\": {\"path\": \"notes.md\", \"content\": \
             \"new text\\n\"}}\n```",
        ), Reply::text("Rewrote notes.md.")])
        // The floor every provider meets: no native calls at all.
        .with_capabilities(Capabilities::basic()),
    );
    let mut agent = agent(provider.clone(), Context::new(dir.path())).autoapprove();
    let session = finished(agent.run("replace the text in notes.md").await);

    let prompt = first_prompt(&provider);
    assert!(
        prompt.contains("reply with exactly one JSON object"),
        "{prompt}"
    );
    assert_eq!(
        read(&dir, "notes.md"),
        "new text\n",
        "and its answer is parsed"
    );
    assert!(session
        .touched
        .iter()
        .any(|path| path == Path::new("notes.md")));
}

#[tokio::test]
async fn a_write_in_suggest_only_becomes_a_proposal_and_touches_nothing() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let ctx = Context::new(dir.path())
        .with_policy(Policy::new(dir.path()).with_approval(ApprovalMode::SuggestOnly));
    let provider = scripted([
        Reply::call(
            "write_file",
            json!({"path": "notes.md", "content": "new text\n"}),
        ),
        Reply::text("Here is the change I would make."),
    ]);
    let mut agent = agent(provider, ctx).autoapprove();
    let session = finished(agent.run("replace the text in notes.md").await);

    assert_eq!(read(&dir, "notes.md"), "old text\n", "nothing was written");
    assert!(session.touched.is_empty());
    let diff = session
        .events
        .iter()
        .find_map(|event| match event {
            AgentEvent::DiffProposed { diff, files } => Some((diff.clone(), files.clone())),
            _ => None,
        })
        .expect("the refused write is offered as a diff");
    assert!(diff.0.contains("-old text"), "{}", diff.0);
    assert!(diff.0.contains("+new text"), "{}", diff.0);
    assert_eq!(diff.1, vec!["notes.md".to_owned()]);
    match session.finished() {
        Some(Outcome::Proposed { files, .. }) => assert_eq!(files, &vec!["notes.md".to_owned()]),
        other => panic!("a suggest-only run succeeds by proposing, got {other:?}"),
    }
}

#[tokio::test]
async fn one_persons_answer_carries_the_rest_of_the_run() {
    let dir = repo(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
    let approver = Arc::new(Scripted::yes());
    let provider = scripted([
        Reply::call(
            "write_file",
            json!({"path": "a.rs", "content": "fn a2() {}\n"}),
        ),
        Reply::call(
            "write_file",
            json!({"path": "b.rs", "content": "fn b2() {}\n"}),
        ),
        Reply::text("Renamed both helpers."),
    ]);
    let mut agent = agent(provider, Context::new(dir.path())).with_approver(approver.clone());
    let session = finished(agent.run("rename the helpers").await);

    assert_eq!(approver.times_asked(), 1, "the class was agreed to once");
    let asks = approver.asks();
    assert!(
        asks[0].summary.starts_with("write_file asks: change"),
        "a person is told which tool and which file: {}",
        asks[0].summary
    );
    assert_eq!(read(&dir, "a.rs"), "fn a2() {}\n");
    assert_eq!(read(&dir, "b.rs"), "fn b2() {}\n");
    assert_eq!(session.touched.len(), 2);
}

/// The claim `delete_file` makes in its own description: it asks a person, and
/// `smoll rollback` puts the file back. Both halves are what a reviewer of a
/// destructive tool needs to see proven, over the same registry the binary uses.
#[tokio::test]
async fn a_deletion_is_asked_about_and_the_rollback_puts_the_file_back() {
    let dir = repo(&[("notes.md", "old text\n"), ("keep.rs", "fn keep() {}\n")]);
    let approver = Arc::new(Scripted::yes());
    let provider = scripted([
        Reply::call("delete_file", json!({"path": "notes.md"})),
        Reply::text("Removed the stale note."),
    ]);
    let ctx = Context::new(dir.path());
    let mut agent = agent(provider, ctx.clone()).with_approver(approver.clone());
    let session = finished(agent.run("delete the stale note").await);

    assert_eq!(
        approver.times_asked(),
        1,
        "a deletion is never run on the agent's own say-so"
    );
    assert!(
        approver.asks()[0].summary.contains("delete_file"),
        "a person is told which tool wants it: {}",
        approver.asks()[0].summary
    );
    assert!(
        !dir.path().join("notes.md").exists(),
        "the one file asked about went"
    );
    assert!(dir.path().join("keep.rs").is_file(), "and only that one");
    assert_eq!(session.touched.len(), 1, "{:?}", session.touched);

    let restored = ctx.rollback();
    assert_eq!(restored.len(), 1, "{restored:?}");
    assert_eq!(
        read(&dir, "notes.md"),
        "old text\n",
        "the run can undo what it deleted"
    );
}

#[tokio::test]
async fn a_deletion_is_refused_rather_than_asked_when_nothing_may_be_removed() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let ctx = Context::new(dir.path())
        .with_policy(Policy::new(dir.path()).with_approval(ApprovalMode::SuggestOnly));
    let provider = scripted([
        Reply::call("delete_file", json!({"path": "notes.md"})),
        Reply::text("It is still there; this mode removes nothing."),
    ]);
    let mut agent = agent(provider, ctx).autoapprove();
    let session = finished(agent.run("delete the note").await);

    assert!(
        dir.path().join("notes.md").is_file(),
        "a mode that writes nothing deletes nothing"
    );
    assert!(session.touched.is_empty(), "{:?}", session.touched);
    let refused = session
        .events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolFinished { result } => match &result.status {
                ToolStatus::Refused { reason } => Some(reason.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the model is told why its call did not run");
    assert!(
        refused.contains("does not let anything be removed"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_yes_about_one_file_is_still_a_question_about_the_next() {
    let dir = repo(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
    let approver = Arc::new(Scripted::always(ApprovalDecision::Approved));
    let provider = scripted([
        Reply::call(
            "write_file",
            json!({"path": "a.rs", "content": "fn a2() {}\n"}),
        ),
        Reply::call(
            "write_file",
            json!({"path": "b.rs", "content": "fn b2() {}\n"}),
        ),
        Reply::text("Renamed both helpers."),
    ]);
    let mut agent = agent(provider, Context::new(dir.path())).with_approver(approver.clone());
    finished(agent.run("rename the helpers").await);

    assert_eq!(
        approver.times_asked(),
        2,
        "an approval for one subject does not speak for another"
    );
    let subjects: Vec<String> = approver.asks().into_iter().map(|ask| ask.summary).collect();
    assert!(subjects[0].contains("a.rs"), "{subjects:?}");
    assert!(subjects[1].contains("b.rs"), "{subjects:?}");
}

#[tokio::test]
async fn a_person_saying_no_ends_the_run_without_writing() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let observer = Arc::new(Collector::default());
    let approver = Arc::new(Scripted::no("not this way"));
    let provider = scripted([Reply::call(
        "write_file",
        json!({"path": "notes.md", "content": "new text\n"}),
    )]);
    let mut agent = agent(provider, Context::new(dir.path()))
        .with_approver(approver.clone())
        .with_observer(observer.clone());
    let error = stopped(agent.run("replace the text in notes.md").await);

    assert!(matches!(error, AgentError::Rejected { .. }), "{error}");
    assert_eq!(error.exit_code(), 2, "a refusal is not a crash");
    assert_eq!(read(&dir, "notes.md"), "old text\n", "nothing was written");
    assert_eq!(approver.times_asked(), 1);
    assert!(
        observer.events().iter().any(|event| matches!(
            event,
            AgentEvent::Finished {
                outcome: Outcome::Blocked { .. },
                ..
            }
        )),
        "the transcript ends with the reason"
    );
}

#[tokio::test]
async fn a_run_that_will_not_finish_is_stopped_by_its_step_budget() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let provider = Arc::new(
        MockProvider::scripted([Reply::call("read_file", json!({"path": "notes.md"}))])
            .repeat_last(),
    );
    let mut agent = agent(provider.clone(), Context::new(dir.path()))
        .with_limits(Limits {
            max_steps: 3,
            ..Limits::default()
        })
        .autoapprove();
    let error = stopped(agent.run("read the file over and over").await);

    match error {
        AgentError::BudgetExhausted(Budget::Steps { used, max }) => {
            assert_eq!((used, max), (3, 3), "{used} of {max}")
        }
        other => panic!("expected the step budget, got {other:?}"),
    }
    assert_eq!(
        error.exit_code(),
        1,
        "a budget is not a configuration error"
    );
    assert_eq!(provider.answers_given(), 3);
}

#[tokio::test]
async fn a_cancelled_run_stops_at_the_next_step() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let cancel = CancelToken::new();
    cancel.cancel();
    let observer = Arc::new(Collector::default());
    let provider = scripted([Reply::text("never asked")]);
    let mut agent = agent(provider, Context::new(dir.path()))
        .with_cancel(cancel)
        .with_observer(observer.clone());
    let error = stopped(agent.run("anything").await);

    assert!(matches!(error, AgentError::Cancelled), "{error}");
    assert_eq!(
        observer
            .events()
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolRequested { .. }))
            .count(),
        0,
        "a cancelled run asks its model for nothing"
    );
    assert!(
        observer.events().iter().any(|event| matches!(
            event,
            AgentEvent::Finished {
                outcome: Outcome::Cancelled,
                ..
            }
        )),
        "cancelled is its own ending, not a failure"
    );
}

#[tokio::test]
async fn a_provider_that_was_down_is_asked_again_and_the_run_says_so() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let provider = scripted([
        Reply::Unreachable,
        Reply::call(
            "write_file",
            json!({"path": "notes.md", "content": "new text\n"}),
        ),
        Reply::text("Rewrote notes.md."),
    ]);
    let mut agent = agent(provider, Context::new(dir.path())).autoapprove();
    let session = finished(agent.run("replace the text in notes.md").await);

    assert_eq!(read(&dir, "notes.md"), "new text\n");
    let said = warnings(&session).join("\n");
    assert!(said.contains("mock did not answer"), "{said}");
    assert!(
        said.contains("trying again"),
        "the wait is admitted to: {said}"
    );
}

#[tokio::test]
async fn an_answer_cut_off_mid_call_is_repaired_from_the_models_own_words() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let cut_off =
        "{\"tool\": \"write_file\", \"arguments\": {\"path\": \"notes.md\", \"content\": \"";
    let provider = scripted([
        Reply::truncated(cut_off),
        Reply::call(
            "write_file",
            json!({"path": "notes.md", "content": "new text\n"}),
        ),
        Reply::text("Rewrote notes.md."),
    ]);
    let mut agent = agent(provider.clone(), Context::new(dir.path())).autoapprove();
    let session = finished(agent.run("replace the text in notes.md").await);

    assert_eq!(
        read(&dir, "notes.md"),
        "new text\n",
        "the repaired call ran"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3, "the cut-off answer cost one extra round");
    let repair = requests[1].messages.last().expect("a repair turn");
    assert!(
        repair.content.contains("could not be used"),
        "the model is told what went wrong: {}",
        repair.content
    );
    assert_eq!(
        requests[1].temperature,
        Some(0.0),
        "a repair is not a second guess"
    );
    assert!(
        requests[1].messages[requests[1].messages.len() - 2]
            .content
            .contains(cut_off),
        "and it is shown its own truncated text"
    );
    assert!(warnings(&session).join("\n").contains("asking once more"));
}

#[tokio::test]
async fn a_second_unreadable_answer_is_reported_rather_than_guessed_at() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let cut_off = "{\"tool\": \"write_file\", \"arguments\": {\"path\": \"notes.md\"";
    let provider = scripted([Reply::truncated(cut_off), Reply::truncated(cut_off)]);
    let mut agent = agent(provider.clone(), Context::new(dir.path())).autoapprove();
    let error = stopped(agent.run("replace the text in notes.md").await);

    match error {
        AgentError::MalformedAnswer { message, raw } => {
            assert!(message.contains("ran out of tokens"), "{message}");
            assert!(raw.contains("write_file"), "{raw}");
        }
        other => panic!("expected a malformed answer, got {other:?}"),
    }
    assert_eq!(
        read(&dir, "notes.md"),
        "old text\n",
        "a guess is not a write"
    );
    assert_eq!(provider.answers_given(), 2, "one repair round, not two");
}

#[tokio::test]
async fn a_tool_that_is_not_here_is_said_so_with_the_names_that_are() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let provider = scripted([
        Reply::call("fs_delete_everything", json!({"path": "."})),
        Reply::call("fs_delete_everything", json!({"path": "."})),
    ]);
    let mut agent = agent(provider, Context::new(dir.path())).autoapprove();
    let error = stopped(agent.run("delete the repository").await);

    match error {
        AgentError::UnknownTool { name, suggestions } => {
            assert_eq!(name, "fs_delete_everything");
            assert!(
                suggestions.contains(&"write_file".to_owned()),
                "{suggestions:?}"
            );
            assert!(
                !suggestions.contains(&"fs_delete_everything".to_owned()),
                "only tools that exist are offered"
            );
        }
        other => panic!("expected an unknown tool, got {other:?}"),
    }
    assert!(
        dir.path().join("notes.md").is_file(),
        "and nothing was deleted"
    );
}

#[tokio::test]
async fn a_validation_command_a_person_declines_is_a_warning_not_a_failure() {
    let dir = repo(&[
        (
            "Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("src/lib.rs", "pub fn a() {}\n"),
    ]);
    // Writes apply on their own here, so the only question this run asks is the
    // one about running the project's own command.
    let ctx = Context::new(dir.path())
        .with_policy(Policy::new(dir.path()).with_approval(ApprovalMode::ApproveCommands));
    let approver = Arc::new(Scripted::no("I run the tests myself"));
    let provider = scripted([
        Reply::call(
            "write_file",
            json!({"path": "src/lib.rs", "content": "pub fn b() {}\n"}),
        ),
        Reply::text("Renamed the helper."),
    ]);
    let mut agent = agent(provider, ctx).with_approver(approver.clone());
    let session = finished(agent.run("rename the helper in src/lib.rs").await);

    assert_eq!(
        read(&dir, "src/lib.rs"),
        "pub fn b() {}\n",
        "the change stands"
    );
    assert_eq!(approver.times_asked(), 1, "the write did not ask");
    let asked = &approver.asks()[0];
    assert!(asked.summary.contains("run `cargo"), "{}", asked.summary);
    assert!(asked.summary.contains("test"), "{}", asked.summary);
    let said = warnings(&session).join("\n");
    assert!(said.contains("unvalidated"), "{said}");
    assert!(
        matches!(session.finished(), Some(Outcome::Completed { .. })),
        "{:?}",
        session.finished()
    );
    let plan = session.plan.as_ref().expect("a plan");
    assert!(
        plan.steps
            .iter()
            .any(|step| step.status == StepStatus::Skipped),
        "the step it could not check is marked as such: {}",
        plan.render()
    );
}

#[tokio::test]
async fn the_observer_sees_every_event_the_session_records() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let observer = Arc::new(Collector::default());
    let (agent, _) = read_write_summarise(&dir, "new text\n");
    let mut agent = agent.with_observer(observer.clone());
    let session = finished(agent.run("replace the text in notes.md").await);

    let live = observer.events();
    assert_eq!(
        live.len(),
        session.events.len(),
        "a TUI and a transcript must not disagree about what happened"
    );
    assert_eq!(live, session.events);
    assert!(live
        .iter()
        .any(|event| matches!(event, AgentEvent::Reflected { .. })));
}

#[tokio::test]
async fn a_run_stopped_halfway_leaves_a_record_that_can_undo_it() {
    let dir = repo(&[("notes.md", "old text\n")]);
    let provider = Arc::new(
        MockProvider::scripted([
            Reply::call(
                "write_file",
                json!({"path": "notes.md", "content": "new text\n"}),
            ),
            Reply::call("read_file", json!({"path": "notes.md"})),
        ])
        .repeat_last(),
    );
    let mut agent = agent(provider, Context::new(dir.path()))
        .with_limits(Limits {
            max_steps: 2,
            ..Limits::default()
        })
        .autoapprove();
    let run = agent.run("change the file, then keep looking").await;

    assert!(
        matches!(run.error, Some(AgentError::BudgetExhausted(_))),
        "the stop is the point of the test: {:?}",
        run.error
    );
    assert_eq!(
        read(&dir, "notes.md"),
        "new text\n",
        "the write happened first"
    );
    assert_eq!(
        run.session.touched,
        vec![Path::new("notes.md").to_owned()],
        "and the stopped transcript names it"
    );

    // A session file written from a run that never finished is what the next
    // process rolls back from.
    let stored = StoredSession::new(run.session, agent.context().snapshots());
    stored.write(dir.path()).expect("writable");
    let read_back = StoredSession::latest(dir.path())
        .expect("a directory")
        .expect("the stopped run is recorded");
    read_back.restore(dir.path());
    assert_eq!(
        read(&dir, "notes.md"),
        "old text\n",
        "undoing works from the file, not from the run"
    );
}
