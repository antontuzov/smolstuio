//! The loop: one task, one repository, one model that cannot hold all three.
//!
//! The order is the one the design asks for — context, plan, ask, act, observe,
//! reflect, and only then stop. Each turn is one request to one provider, so a
//! step can be retried, timed out, logged and repaired as a unit. Nothing here
//! decides whether a write is allowed: the policy in `agent_sandbox` answers that
//! before a tool is entered, and [`Approver`] answers only the question the
//! policy leaves for a person.
//!
//! Four properties the rest of the crate depends on:
//!
//! - **A refusal is an observation, not a crash.** The model is told in its own
//!   conversation what it may not do, and carries on. Only a person saying no, or
//!   a limit being reached, ends a run.
//! - **A write is undoable.** Every applied change is recorded in the [`Context`]
//!   journal and [`Agent::rollback`] puts the tree back. The loop never calls it
//!   on its own: a change a person has been shown is theirs to accept or undo.
//! - **Nothing is invented.** An answer that cannot be read gets one repair round
//!   quoting the model's own words back at it, and after that the run says so
//!   rather than guessing what the cut-off JSON meant to say.
//! - **The plan is the run's own scaffold.** It is written from the repository —
//!   the files the ranking chose, the project's validation command — and advanced
//!   as the steps happen, so a transcript can show where the run departed from it.
//!
//! Streaming is not emitted here: no provider in this build streams, so
//! [`AgentEvent::Delta`] waits for the engine adapter rather than being faked.
//!
//! ```
//! use std::sync::Arc;
//!
//! use agent_core::Agent;
//! use agent_providers::{MockProvider, Provider, Reply};
//! use agent_tools::{Context, Registry};
//! use serde_json::json;
//!
//! let dir = tempfile::tempdir().expect("a temp repo");
//! std::fs::write(dir.path().join("notes.md"), "old text\n").expect("writable");
//!
//! let provider: Arc<dyn Provider> = Arc::new(MockProvider::scripted([
//!     Reply::call("write_file", json!({"path": "notes.md", "content": "new text\n"})),
//!     Reply::text("Rewrote notes.md."),
//! ]));
//!
//! let runtime = tokio::runtime::Runtime::new().expect("a runtime");
//! let mut agent = Agent::new(
//!     provider,
//!     Registry::with_defaults(),
//!     Context::new(dir.path()),
//! )
//! .autoapprove();
//! let run = runtime.block_on(agent.run("replace the text in notes.md"));
//! assert!(run.succeeded(), "the run finishes");
//! let session = run.session;
//!
//! assert_eq!(session.touched.len(), 1, "one file was written");
//! assert_eq!(
//!     std::fs::read_to_string(dir.path().join("notes.md")).expect("readable"),
//!     "new text\n"
//! );
//! assert!(session.recap().starts_with("Rewrote notes.md."), "{}", session.recap());
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use agent_config::{ApprovalMode, Config};
use agent_providers::{
    tool_calls_among, Capabilities, Completion, CompletionRequest, Provider, ProviderError,
    StopReason, ToolSchema,
};
use agent_repo::{map, patch, Index, Project, RepoMap};
use agent_sandbox::Permission;
use agent_tools::{needs_approval, Context, Intent, Registry, ToolCall, ToolResult, ToolStatus};
use smollm_core::chat::{approx_token_count, chat_message, CancelToken, ChatMessage, Role};

use crate::approve::{Approver, AutoApprove, RefuseAll};
use crate::error::{AgentError, Budget};
use crate::prompt;
use crate::types::{
    now_ms, AgentEvent, ApprovalDecision, ApprovalRequest, ContextBundle, ContextFile, Outcome,
    Plan, Session, StepStatus,
};

/// How many file bodies the first turn gets, whatever the window allows. A model
/// that has been shown eight files reasons about none of them.
const MAX_CONTEXT_FILES: usize = 5;
/// Tokens held back for the plan, the task and their restatement.
const PROMPT_RESERVE: usize = 400;
/// Turns of conversation kept when an over-long prompt has to be cut.
const KEEP_TAIL: usize = 4;
/// The share of the window at which the run says it is running out.
const BUDGET_WARNING_AT: usize = 90;
/// What one answer may cost when the configuration does not say.
const DEFAULT_REPLY_TOKENS: u32 = 256;
/// A prompt shorter than this cannot hold a tool list and a question.
const MIN_WINDOW_TOKENS: usize = 600;

/// Where the loop sends what happened, while it happens.
///
/// The session records every event whether or not anyone is listening, so a
/// headless run and a TUI see the same thing; an observer is only the live copy.
pub trait Observer: Send + Sync {
    fn observe(&self, event: &AgentEvent);
}

impl<F> Observer for F
where
    F: Fn(&AgentEvent) + Send + Sync,
{
    fn observe(&self, event: &AgentEvent) {
        self(event)
    }
}

/// What a run may spend, and what it must stop at.
#[derive(Debug, Clone)]
pub struct Limits {
    /// The step budget: one provider round-trip each, however many calls it asks
    /// for. This is the number that decides whether a 1.5B model can finish.
    pub max_steps: usize,
    /// The prompt budget. The model's own window still caps it: the loop uses the
    /// smaller of the two rather than finding out by failing.
    pub max_context_tokens: usize,
    /// The wall clock for the whole run, shared between the model and the tools.
    pub timeout: Duration,
    /// Extra attempts for a provider that may answer next time.
    pub retries: u32,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop: Vec<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 20,
            max_context_tokens: 8_000,
            timeout: Duration::from_secs(120),
            retries: 2,
            max_tokens: Some(DEFAULT_REPLY_TOKENS),
            temperature: Some(0.1),
            top_p: None,
            stop: Vec::new(),
        }
    }
}

impl Limits {
    /// From the `[agent]` section, plus the `[providers.<name>]` the agent names.
    pub fn from_config(config: &Config) -> Self {
        let agent = &config.agent;
        let mut limits = Self {
            max_steps: agent.max_steps,
            max_context_tokens: agent.max_context_tokens,
            timeout: Duration::from_secs(agent.timeout_seconds),
            retries: agent.retries,
            ..Self::default()
        };
        if let Some(provider) = agent
            .provider
            .as_deref()
            .and_then(|name| config.providers.get(name))
        {
            limits.max_tokens = provider.max_tokens;
            limits.temperature = provider.temperature;
            limits.top_p = provider.top_p;
            limits.stop = provider.stop.clone();
        }
        // An answer that eats a third of the window leaves nothing to look at.
        if limits.reply_tokens() as usize > agent.max_context_tokens / 3 {
            limits.max_tokens = Some((agent.max_context_tokens / 6) as u32);
        }
        limits
    }

    /// What one answer may cost.
    fn reply_tokens(&self) -> u32 {
        self.max_tokens.unwrap_or(DEFAULT_REPLY_TOKENS)
    }

    /// The window this run can use: what the configuration allows, within what
    /// the model can actually hold.
    fn window(&self, capabilities: &Capabilities) -> usize {
        self.max_context_tokens
            .min(capabilities.context_tokens)
            .max(MIN_WINDOW_TOKENS)
    }
}

/// A run, and what stopped it if anything did.
///
/// A run that was refused halfway still has a transcript and still may have
/// written files, so the session is not carried inside the error: the caller
/// writes the session file either way and reads `error` for the exit code.
#[derive(Debug)]
pub struct Run {
    pub session: Session,
    pub error: Option<AgentError>,
}

impl Run {
    /// How the run said it ended, once it had said.
    pub fn outcome(&self) -> Option<&Outcome> {
        self.session.finished()
    }

    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }
}

/// The agent: a provider, the tools it may use, and the workspace it is bound to.
pub struct Agent {
    provider: Arc<dyn Provider>,
    registry: Registry,
    ctx: Context,
    limits: Limits,
    approver: Arc<dyn Approver>,
    observer: Option<Arc<dyn Observer>>,
    cancel: CancelToken,
}

impl Agent {
    /// An agent bounded to the workspace in `ctx`, with the loop's own defaults
    /// and an approver that asks a person who is not there — which is to say, one
    /// that will not write until it is told.
    pub fn new(provider: Arc<dyn Provider>, registry: Registry, ctx: Context) -> Self {
        Self {
            provider,
            registry,
            ctx,
            limits: Limits::default(),
            approver: Arc::new(RefuseAll::default()),
            observer: None,
            cancel: CancelToken::new(),
        }
    }

    /// What a CLI builds from one loaded configuration: limits, policy and audit
    /// setting from the same `Config`, and the approver the approval mode implies.
    ///
    /// Only `autonomous-safe` gets [`AutoApprove`], and that is not this code
    /// being permissive: in that mode the policy already allows the writes and
    /// commands it counts safe, so an approver is rarely reached at all.
    pub fn configured(
        provider: Arc<dyn Provider>,
        registry: Registry,
        config: &Config,
        ctx: Context,
    ) -> Self {
        let approver: Arc<dyn Approver> = match config.agent.approval_mode {
            ApprovalMode::AutonomousSafe => Arc::new(AutoApprove),
            _ => Arc::new(RefuseAll::default()),
        };
        Self::new(provider, registry, ctx)
            .with_limits(Limits::from_config(config))
            .with_approver(approver)
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    pub fn with_observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// The handle a TUI's Esc key flips.
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Write and run without asking anyone, for `smoll --yes`. The policy still
    /// blocks what it blocks.
    pub fn autoapprove(mut self) -> Self {
        self.approver = Arc::new(AutoApprove);
        self
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The workspace handle: policy, audit log, and everything the run wrote.
    pub fn context(&self) -> &Context {
        &self.ctx
    }

    /// Who is answering, for a status line.
    pub fn provider_name(&self) -> String {
        match self.provider.model() {
            Some(model) => format!("{} ({model})", self.provider.name()),
            None => self.provider.name().to_owned(),
        }
    }

    /// One task, start to finish.
    ///
    /// Every event lands in the returned session, and `Run::error` says whether
    /// the run could be carried out — a limit reached, a provider that would not
    /// answer, a person who said no. A refusal by the policy is not one of those:
    /// the model is told about it and the run goes on.
    ///
    /// The transcript comes back whether or not the run erred, because a run
    /// that stopped at its step budget may already have written files, and the
    /// session file is where the record of what those writes replaced lives.
    ///
    /// The loop reads and writes the filesystem through the tools, so a caller
    /// that cannot spare a thread runs this on its own task.
    pub async fn run(&mut self, task: &str) -> Run {
        let mut state = State::new(task);
        self.emit(
            &mut state,
            AgentEvent::Started {
                task: task.to_owned(),
                at_ms: now_ms(),
            },
        );
        let driven = self.drive(task, &mut state).await;
        self.close_plan(&mut state);
        match driven {
            Ok(outcome) => {
                self.emit(
                    &mut state,
                    AgentEvent::Finished {
                        outcome,
                        at_ms: now_ms(),
                    },
                );
                Run {
                    session: state.session,
                    error: None,
                }
            }
            Err(error) => {
                self.emit(
                    &mut state,
                    AgentEvent::Finished {
                        outcome: outcome_for(&error),
                        at_ms: now_ms(),
                    },
                );
                Run {
                    session: state.session,
                    error: Some(error),
                }
            }
        }
    }

    /// Put every file this run wrote back the way it found it, newest write first.
    ///
    /// A file a person has edited since is left alone and named in a warning
    /// rather than overwritten again.
    pub fn rollback(&self, session: &mut Session) -> Vec<PathBuf> {
        let restored = self.ctx.rollback();
        // Named as the repository names them, the same way `Session::touched`
        // does: two spellings for one file in a transcript is one too many.
        let files: Vec<PathBuf> = restored
            .iter()
            .flat_map(|snapshot| snapshot.paths.iter())
            .map(|path| PathBuf::from(self.ctx.relative(path)))
            .collect();
        for path in restored
            .iter()
            .flat_map(|snapshot| snapshot.left_alone.iter())
        {
            self.warn(
                session,
                format!("{path:?} changed after this run wrote it, so it was left as you had it"),
            );
        }
        let event = AgentEvent::RolledBack {
            files: files.clone(),
        };
        self.announce(&event);
        session.record(event);
        files
    }

    async fn drive(&mut self, task: &str, state: &mut State) -> Result<Outcome, AgentError> {
        let capabilities = self.provider.capabilities();
        let window = self.limits.window(&capabilities);
        let schemas = self.schemas();
        let tool_calling = capabilities.tool_calling && !schemas.is_empty();
        let survey = self.survey(task, &schemas, tool_calling, window);
        // The plan is read from the survey before its pieces move into the run:
        // the files the ranking chose are what make it specific to this task.
        let (plan, validate_step) = self.plan_for(task, &survey);
        state.system = survey.system;
        state.bundle = survey.bundle;
        state.validate_argv = survey.validate;
        state.validate_step = validate_step;
        state.plan = plan;
        self.emit(
            state,
            AgentEvent::ContextGathered {
                files: state.bundle.paths(),
                tokens: state.bundle.token_count(),
            },
        );
        self.emit(
            state,
            AgentEvent::PlanSet {
                plan: state.plan.clone(),
            },
        );
        self.advance(state, 0, StepStatus::Running);

        let reply = self.limits.reply_tokens() as usize;
        for _step in 1..=self.limits.max_steps {
            self.check_stop(state)?;
            state.steps_taken += 1;

            let mut request = self.request(state, &schemas, tool_calling);
            if !request.fits(&capabilities, reply) {
                let trimmed = self.trim(state, &schemas, tool_calling, &capabilities, reply);
                request = self.request(state, &schemas, tool_calling);
                self.emit(
                    state,
                    AgentEvent::Warning {
                        message: format!(
                            "the prompt did not fit this model's {} token window, so {trimmed}",
                            capabilities.context_tokens
                        ),
                    },
                );
                if !request.fits(&capabilities, reply) {
                    return Err(AgentError::BudgetExhausted(Budget::Tokens {
                        used: request.prompt_tokens() as usize,
                        max: capabilities.context_tokens,
                    }));
                }
            }

            let answer = self.ask(&request, state).await?;
            state.session.note_usage(&answer.usage);
            self.warn_at_budget(state, &capabilities);
            if !answer.text.trim().is_empty() {
                self.emit(
                    state,
                    AgentEvent::Message {
                        text: answer.text.clone(),
                    },
                );
            }
            state
                .tail
                .push(chat_message(Role::Assistant, answer.text.clone()));

            let calls = self.calls_for(&answer, &schemas);
            if calls.is_empty() {
                let summary = self.summary(&answer.text, state);
                if self.validate_if_due(state).await? && state.steps_taken < self.limits.max_steps {
                    // The command's output is in the transcript now, and this run
                    // can still afford a step to act on it.
                    continue;
                }
                return Ok(self.outcome(state, summary));
            }

            if calls
                .iter()
                .all(|call| self.registry.get(&call.name).is_none())
            {
                state.unnamed_streak += 1;
                if state.unnamed_streak >= 2 {
                    // The registry answers the first one by naming the tools that
                    // exist. A model that cannot use the list is not helped by
                    // being shown it again, and the step budget is not the place
                    // to find out.
                    return Err(AgentError::UnknownTool {
                        name: calls[0].name.clone(),
                        suggestions: self.registry.names(),
                    });
                }
            } else {
                state.unnamed_streak = 0;
            }

            let results = self.run_calls(&calls, state).await?;
            self.reflect(state, &results);
        }

        Err(AgentError::BudgetExhausted(Budget::Steps {
            used: state.steps_taken,
            max: self.limits.max_steps,
        }))
    }

    /// What the repository is, and what the first turn gets to see of it.
    fn survey(
        &self,
        task: &str,
        schemas: &[ToolSchema],
        tool_calling: bool,
        window: usize,
    ) -> Survey {
        let index = Index::walk(&self.ctx.root);
        let project = Project::detect(&index);
        let about = format!("{} · {}", project.describe(), index.describe());
        let system = prompt::system(schemas, tool_calling, &about);

        // Every step pays for the rules and the tool list, so the repository gets
        // what is left of the window after them.
        let mut fixed = approx_token_count(&system) as usize;
        if tool_calling {
            fixed += schemas.iter().map(ToolSchema::token_cost).sum::<usize>();
        }
        let available =
            window.saturating_sub(fixed + self.limits.reply_tokens() as usize + PROMPT_RESERVE);
        let repo_map = RepoMap::build(&index, &project, Some(task), (available / 4).max(1));
        let mut bundle = ContextBundle::new(task);
        bundle.repo_map = repo_map.text;
        for candidate in map::context(
            &index,
            task,
            available.saturating_sub(repo_map.tokens),
            MAX_CONTEXT_FILES,
        ) {
            bundle.push_file(ContextFile {
                path: candidate.path,
                content: candidate.content,
                reason: candidate.reason,
            });
        }
        Survey {
            system,
            bundle,
            validate: project.validate().map(|suggestion| suggestion.argv.clone()),
        }
    }

    /// The scaffold this run advances, and which step of it validation owns.
    ///
    /// It is written from the repository rather than asked of the model: a 1.5B
    /// model told to plan spends its window restating the task, and a loop cannot
    /// mark a plan it did not own as it went. The files the ranking chose and the
    /// project's own validation command are what make it specific to this run.
    fn plan_for(&self, task: &str, survey: &Survey) -> (Plan, Option<usize>) {
        let named: Vec<String> = survey.bundle.paths().into_iter().take(3).collect();
        let read = if named.is_empty() {
            "Find the files the task is about".to_owned()
        } else {
            format!("Read {}", named.join(", "))
        };
        let mut steps = vec![
            read,
            "Make the change, and diff it against the file".to_owned(),
        ];
        let validate = match &survey.validate {
            Some(argv) => {
                steps.push(format!("Validate with `{}`", argv.join(" ")));
                Some(steps.len() - 1)
            }
            None => {
                steps.push("Say how this could be checked".to_owned());
                None
            }
        };
        steps.push("Summarise what changed and what did not".to_owned());
        (Plan::new(one_line(task), steps), validate)
    }

    fn schemas(&self) -> Vec<ToolSchema> {
        self.registry
            .definitions()
            .into_iter()
            .map(|definition| {
                ToolSchema::new(
                    definition.name,
                    definition.description,
                    definition.parameters,
                )
            })
            .collect()
    }

    /// The whole prompt, rebuilt from the plan and the bundle every step, so the
    /// model sees its own checkboxes advance rather than a plan from step one.
    fn request(
        &self,
        state: &State,
        schemas: &[ToolSchema],
        tool_calling: bool,
    ) -> CompletionRequest {
        let mut messages = vec![
            chat_message(Role::System, state.system.clone()),
            chat_message(Role::User, prompt::open(&state.bundle, &state.plan)),
        ];
        messages.extend(state.tail.iter().cloned());
        let mut request = CompletionRequest::new(messages);
        if tool_calling {
            request.tools = schemas.to_vec();
        }
        request.max_tokens = Some(self.limits.reply_tokens());
        request.temperature = self.limits.temperature;
        request.top_p = self.limits.top_p;
        request.stop = self.limits.stop.clone();
        request
    }

    /// Cut the prompt until the model can hold it, and say what went.
    ///
    /// File bodies go before the conversation: a model that has seen the map can
    /// still ask for the file it needs, and one that has only ever seen three
    /// bodies does not know the other three hundred exist.
    fn trim(
        &self,
        state: &mut State,
        schemas: &[ToolSchema],
        tool_calling: bool,
        capabilities: &Capabilities,
        reply: usize,
    ) -> String {
        let mut files = 0usize;
        let mut turns = 0usize;
        while !self
            .request(state, schemas, tool_calling)
            .fits(capabilities, reply)
        {
            let spent = state.bundle.token_count();
            if state.bundle.files.len() > 1 && spent > 100 {
                let cut = state.bundle.fit_to_budget(spent * 3 / 4).len();
                if cut == 0 {
                    break;
                }
                files += cut;
                continue;
            }
            if state.tail.len() > KEEP_TAIL {
                // An observation whose call has gone is still readable: it names
                // the tool that answered.
                state.tail.remove(0);
                turns += 1;
                continue;
            }
            break;
        }
        let mut said = Vec::new();
        if files > 0 {
            said.push(format!("{files} file body(ies) were dropped"));
        }
        if turns > 0 {
            said.push(format!("{turns} older observation(s) were dropped"));
        }
        if said.is_empty() {
            "there was nothing left to drop".to_owned()
        } else {
            said.join(" and ")
        }
    }

    fn check_stop(&self, state: &State) -> Result<(), AgentError> {
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        if state.started.elapsed() >= self.limits.timeout {
            return Err(AgentError::TimedOut {
                seconds: self.limits.timeout.as_secs(),
            });
        }
        Ok(())
    }

    fn warn_at_budget(&self, state: &mut State, capabilities: &Capabilities) {
        if state.warned_budget || capabilities.context_tokens == 0 {
            return;
        }
        let spent = state.session.usage.total_tokens as usize;
        if spent * 100 >= capabilities.context_tokens * BUDGET_WARNING_AT {
            state.warned_budget = true;
            self.emit(
                state,
                AgentEvent::Warning {
                    message: format!(
                        "this run has spent {spent} of the model's {} tokens",
                        capabilities.context_tokens
                    ),
                },
            );
        }
    }

    /// One step's request, with the provider's own retries and, if the answer
    /// came back unreadable, the single repair round.
    async fn ask(
        &self,
        request: &CompletionRequest,
        state: &mut State,
    ) -> Result<Completion, AgentError> {
        let answer = self.attempt(request, state, false).await?;
        let Some(problem) = unreadable(&answer) else {
            return Ok(answer);
        };
        self.emit(
            state,
            AgentEvent::Warning {
                message: format!("the answer could not be used ({problem}); asking once more"),
            },
        );
        let repair = prompt::repair(request, &answer.text, &problem);
        let again = self.attempt(&repair, state, true).await?;
        match unreadable(&again) {
            None => Ok(again),
            Some(second) => Err(AgentError::MalformedAnswer {
                message: second,
                raw: again.text.clone(),
            }),
        }
    }

    async fn attempt(
        &self,
        request: &CompletionRequest,
        state: &mut State,
        repair: bool,
    ) -> Result<Completion, AgentError> {
        let mut tries = 0u32;
        loop {
            if self.cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            let remaining = self.limits.timeout.saturating_sub(state.started.elapsed());
            if remaining.is_zero() {
                return Err(AgentError::TimedOut {
                    seconds: self.limits.timeout.as_secs(),
                });
            }
            match tokio::time::timeout(remaining, self.complete(request, repair)).await {
                Err(_) => {
                    return Err(AgentError::TimedOut {
                        seconds: self.limits.timeout.as_secs(),
                    })
                }
                Ok(Ok(completion)) => return Ok(completion),
                Ok(Err(error)) => {
                    if error.retryable() && tries < self.limits.retries {
                        tries += 1;
                        let wait = backoff(&error, tries);
                        self.emit(
                            state,
                            AgentEvent::Warning {
                                message: format!(
                                    "{} did not answer ({error}); trying again in {} ms",
                                    self.provider.name(),
                                    wait.as_millis()
                                ),
                            },
                        );
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(provider_error(error));
                }
            }
        }
    }

    /// The request, and whether this is the repair round. Split out because the
    /// two provider methods return different opaque futures, and a timeout needs
    /// to wrap one of them rather than an already-awaited answer.
    async fn complete(
        &self,
        request: &CompletionRequest,
        repair: bool,
    ) -> Result<Completion, ProviderError> {
        if repair {
            self.provider.complete_again(request).await
        } else {
            self.provider.complete(request).await
        }
    }

    /// The calls this answer means: the native ones if the provider parsed them,
    /// otherwise the same reading a mock goes through.
    fn calls_for(&self, answer: &Completion, schemas: &[ToolSchema]) -> Vec<ToolCall> {
        if !answer.calls.is_empty() {
            return answer.calls.clone();
        }
        tool_calls_among(&answer.text, schemas)
    }

    async fn run_calls(
        &self,
        calls: &[ToolCall],
        state: &mut State,
    ) -> Result<Vec<ToolResult>, AgentError> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            results.push(self.run_one(call, state).await?);
        }
        state
            .tail
            .push(chat_message(Role::User, prompt::observation(&results)));
        Ok(results)
    }

    async fn run_one(&self, call: &ToolCall, state: &mut State) -> Result<ToolResult, AgentError> {
        self.emit(
            state,
            AgentEvent::ToolRequested {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
        );
        let before = self.ctx.changes();
        let mut result = self.registry.execute(call, &self.ctx).await;
        if needs_approval(&result) {
            result = self.settle(call, result, state).await?;
        }
        state.session.note_redactions(result.redactions);
        match &result.status {
            ToolStatus::Completed if result.permission == Permission::ReadOnly => {
                self.advance(state, 0, StepStatus::Done)
            }
            ToolStatus::Completed if result.permission == Permission::Write => {
                self.advance(state, 1, StepStatus::Done)
            }
            ToolStatus::Refused { .. } => self.record_proposal(call, state),
            _ => {}
        }
        let applied: Vec<PathBuf> = self
            .ctx
            .changes()
            .into_iter()
            .filter(|path| !before.contains(path))
            .collect();
        self.emit(
            state,
            AgentEvent::ToolFinished {
                result: result.clone(),
            },
        );
        if !applied.is_empty() {
            self.advance(state, 0, StepStatus::Done);
            self.emit(state, AgentEvent::ChangesApplied { files: applied });
        }
        Ok(result)
    }

    /// Ask a person about one call, and run it if they say yes.
    ///
    /// The first pass through the registry wrote nothing, so running the call
    /// again after the answer is safe — and it is what puts both the question and
    /// the decision in the audit log.
    async fn settle(
        &self,
        call: &ToolCall,
        awaiting: ToolResult,
        state: &mut State,
    ) -> Result<ToolResult, AgentError> {
        let intent = self.intent_of(call);
        let paths = intent
            .as_ref()
            .map(|intent| intent.paths.clone())
            .unwrap_or_default();
        let request = ApprovalRequest::new(
            awaiting.permission,
            awaiting.output.text.clone(),
            self.detail(call),
        )
        .for_paths(paths);
        self.emit(
            state,
            AgentEvent::ApprovalNeeded {
                request: request.clone(),
            },
        );
        let decision = self.approver.approve(&request).await;
        self.emit(
            state,
            AgentEvent::ApprovalGiven {
                decision: decision.clone(),
            },
        );
        match decision {
            ApprovalDecision::Approved | ApprovalDecision::ApprovedForRun => {
                let for_run = decision == ApprovalDecision::ApprovedForRun;
                let subject = intent.as_ref().ok().and_then(|intent| intent.subject());
                self.ctx
                    .approve(awaiting.permission, subject.as_deref(), for_run);
                Ok(self.registry.execute(call, &self.ctx).await)
            }
            ApprovalDecision::Rejected { reason } => Err(AgentError::Rejected { reason }),
        }
    }

    fn intent_of(&self, call: &ToolCall) -> Result<Intent, String> {
        let Some(tool) = self.registry.get(&call.name) else {
            return Err(format!("there is no tool named {}", call.name));
        };
        tool.intent(call, &self.ctx)
    }

    /// What a person is shown beside the question: the change itself, or the exact
    /// command.
    fn detail(&self, call: &ToolCall) -> String {
        for key in ["patch", "content"] {
            if let Some(text) = call.arguments.get(key).and_then(Value::as_str) {
                return truncated(text, 1_200);
            }
        }
        if let Some(argv) = call.str_list_arg("argv") {
            return argv.join(" ");
        }
        truncated(&call.arguments.to_string(), 400)
    }

    /// The diff a refused write meant, computed against the file on disk without
    /// touching it. In `suggest-only` that diff is the deliverable.
    fn record_proposal(&self, call: &ToolCall, state: &mut State) {
        let Some((diff, files)) = self.proposal_for(call) else {
            return;
        };
        for file in &files {
            if !state.proposal.contains(file) {
                state.proposal.push(file.clone());
            }
        }
        self.advance(state, 1, StepStatus::Skipped);
        self.emit(state, AgentEvent::DiffProposed { diff, files });
    }

    fn proposal_for(&self, call: &ToolCall) -> Option<(String, Vec<String>)> {
        match call.name.as_str() {
            "write_file" => {
                let path = call.str_arg("path")?;
                let content = call.arguments.get("content").and_then(Value::as_str)?;
                let diff = patch::diff_against_disk(&self.ctx.root, &path, content).ok()?;
                (!diff.is_empty()).then_some((diff, vec![path]))
            }
            "patch_file" => {
                let text = call.str_arg("patch")?;
                let files = match patch::touched_paths(&text) {
                    Ok(names) => names,
                    // A hunk with no headers: the `path` argument names its file.
                    Err(_) => call
                        .str_arg("path")
                        .map(|path| vec![path])
                        .unwrap_or_default(),
                };
                (!text.trim().is_empty() && !files.is_empty()).then_some((text, files))
            }
            _ => None,
        }
    }

    /// Run the repository's own validation command, if it has one and this run has
    /// changed something.
    ///
    /// Whether it ran is what the `Ok` says, so the caller can hand the model the
    /// step to act on the result. A person who declines it is not a failed run:
    /// the change stands and the warning says the result was never checked, which
    /// is the honest version.
    async fn validate_if_due(&self, state: &mut State) -> Result<bool, AgentError> {
        if state.validated || state.validate_step.is_none() || self.ctx.changes().is_empty() {
            return Ok(false);
        }
        let Some(argv) = state.validate_argv.clone() else {
            return Ok(false);
        };
        let index = state.validate_step.expect("a step the plan holds");
        state.validated = true;
        self.advance(state, index, StepStatus::Running);
        let call = ToolCall::new("run_command", json!({"argv": argv.clone()}));
        let result = match self.run_one(&call, state).await {
            Ok(result) => result,
            Err(AgentError::Rejected { reason }) => {
                self.advance(state, index, StepStatus::Skipped);
                self.emit(
                    state,
                    AgentEvent::Warning {
                        message: format!(
                            "nobody answered the validation command ({reason}), so this change is \
                         unvalidated"
                        ),
                    },
                );
                return Ok(false);
            }
            Err(other) => return Err(other),
        };
        let outcome = prompt::observation(std::slice::from_ref(&result));
        state.tail.push(chat_message(Role::User, outcome));
        if result.is_success() {
            self.advance(state, index, StepStatus::Done);
            state.validation_failed = None;
        } else {
            self.advance(state, index, StepStatus::Failed);
            state.validation_failed = Some(format!("{} — {}", argv.join(" "), result.summary()));
        }
        Ok(true)
    }

    /// The answer the run reports. A model that stopped with nothing to say is
    /// quoted as having stopped with nothing to say.
    fn summary(&self, text: &str, state: &State) -> String {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
        let changed = if self.ctx.changes().is_empty() {
            "changed nothing"
        } else {
            "changed files"
        };
        format!(
            "{} gave no answer after {} step(s) and {changed}",
            self.provider.name(),
            state.steps_taken
        )
    }

    fn outcome(&self, state: &State, summary: String) -> Outcome {
        if let Some(failure) = &state.validation_failed {
            return Outcome::Failed {
                reason: format!("{summary}\nThe repository's validation command fails: {failure}"),
            };
        }
        if !self.ctx.changes().is_empty() {
            return Outcome::Completed {
                summary,
                steps_taken: state.steps_taken,
            };
        }
        if !state.proposal.is_empty() {
            return Outcome::Proposed {
                summary,
                files: state.proposal.clone(),
            };
        }
        Outcome::Completed {
            summary,
            steps_taken: state.steps_taken,
        }
    }

    /// One line for the transcript and the next turn's memory: what worked, and
    /// what did not.
    fn reflect(&self, state: &mut State, results: &[ToolResult]) {
        let failed: Vec<&str> = results
            .iter()
            .filter(|result| !result.is_success())
            .map(|result| result.call.name.as_str())
            .collect();
        let note = if failed.is_empty() {
            format!("{} call(s) answered as asked", results.len())
        } else {
            format!(
                "{} of {} call(s) did not work ({})",
                failed.len(),
                results.len(),
                failed.join(", ")
            )
        };
        self.emit(state, AgentEvent::Reflected { note });
    }

    /// A plan the run never finished is reported as skipped rather than left open:
    /// whoever reads the transcript should be able to tell what did not happen.
    fn close_plan(&self, state: &mut State) {
        let last = state.plan.steps.len().saturating_sub(1);
        for index in 0..state.plan.steps.len() {
            let status = match state.plan.steps[index].status {
                StepStatus::Pending => StepStatus::Skipped,
                StepStatus::Running if index == last => StepStatus::Done,
                StepStatus::Running => StepStatus::Skipped,
                finished => finished,
            };
            self.advance(state, index, status);
        }
    }

    fn emit(&self, state: &mut State, event: AgentEvent) {
        self.announce(&event);
        state.session.record(event);
    }

    /// Move one step of the plan and put the move in the transcript.
    ///
    /// The session marks its own copy when the event lands; this marks the copy
    /// the next prompt is built from, so the model sees its checkboxes filled in
    /// rather than a plan frozen at step one.
    fn advance(&self, state: &mut State, index: usize, status: StepStatus) {
        let Some(step) = state.plan.steps.get(index) else {
            return;
        };
        if step.status == status {
            return;
        }
        state.plan.mark(index, status);
        self.emit(state, AgentEvent::PlanAdvanced { index, status });
    }

    fn warn(&self, session: &mut Session, message: impl Into<String>) {
        let event = AgentEvent::Warning {
            message: message.into(),
        };
        self.announce(&event);
        session.record(event);
    }

    fn announce(&self, event: &AgentEvent) {
        if let Some(observer) = &self.observer {
            observer.observe(event);
        }
    }
}

/// What the first turn was built from.
struct Survey {
    system: String,
    bundle: ContextBundle,
    /// The command the repository says validates it, if it says.
    validate: Option<Vec<String>>,
}

/// One run's moving parts, kept out of `Agent` so a second `run` starts clean.
struct State {
    session: Session,
    started: Instant,
    system: String,
    bundle: ContextBundle,
    plan: Plan,
    /// Everything after the system and first-turn messages, which are rebuilt
    /// from `bundle` and `plan` on every step.
    tail: Vec<ChatMessage>,
    steps_taken: usize,
    /// What a refused write asked for: the proposal a suggest-only run leaves
    /// behind.
    proposal: Vec<String>,
    validate_argv: Option<Vec<String>>,
    validate_step: Option<usize>,
    validated: bool,
    validation_failed: Option<String>,
    unnamed_streak: usize,
    warned_budget: bool,
}

impl State {
    fn new(task: &str) -> Self {
        Self {
            session: Session::new(task),
            started: Instant::now(),
            system: String::new(),
            bundle: ContextBundle::new(task),
            plan: Plan::new(task, Vec::new()),
            tail: Vec::new(),
            steps_taken: 0,
            proposal: Vec::new(),
            validate_argv: None,
            validate_step: None,
            validated: false,
            validation_failed: None,
            unnamed_streak: 0,
            warned_budget: false,
        }
    }
}

/// When the run stopped because it could not go on, what it stopped as.
fn outcome_for(error: &AgentError) -> Outcome {
    match error {
        AgentError::Cancelled => Outcome::Cancelled,
        AgentError::Rejected { reason } | AgentError::Policy { reason } => Outcome::Blocked {
            reason: reason.clone(),
        },
        other => Outcome::Failed {
            reason: other.to_string(),
        },
    }
}

/// An answer the model said was a tool call, and this loop cannot read.
///
/// Only the two unambiguous cases: the provider reported a tool call and handed
/// over nothing, or the reply budget cut the call off mid-argument. A prose
/// answer is never malformed — it is the final answer, and the loop stops on it.
fn unreadable(answer: &Completion) -> Option<String> {
    if !answer.calls.is_empty() {
        return None;
    }
    if answer.stop == StopReason::ToolCalls {
        return Some(
            "the answer is meant to be a tool call and carries nothing that can be read as one"
                .to_owned(),
        );
    }
    if answer.stop.is_truncated() && has_call_shape(&answer.text) {
        return Some(
            "the reply ran out of tokens before the tool call closed, so its arguments are \
             missing"
                .to_owned(),
        );
    }
    None
}

/// Whether text looks like an interrupted call rather than a sentence.
fn has_call_shape(text: &str) -> bool {
    let haystack = text.to_lowercase();
    if haystack.contains("<tool_call") {
        return true;
    }
    haystack.contains('{')
        && [
            "\"tool\"",
            "\"name\"",
            "\"function\"",
            "\"arguments\"",
            "\"args\"",
        ]
        .iter()
        .any(|key| haystack.contains(key))
}

/// The server's own wait when it named one, otherwise a short doubling backoff.
fn backoff(error: &ProviderError, attempt: u32) -> Duration {
    match error.backoff_hint() {
        Some(seconds) => Duration::from_secs(seconds.min(30)),
        None => Duration::from_millis(100u64 << attempt.min(4)),
    }
}

fn provider_error(error: ProviderError) -> AgentError {
    match error {
        ProviderError::Cancelled => AgentError::Cancelled,
        other => AgentError::Provider {
            message: other.to_string(),
            retryable: other.retryable(),
        },
    }
}

/// A task is one line in the transcript's heading, however it was pasted in.
fn one_line(task: &str) -> String {
    let flat = task
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    truncated(&flat, 120)
}

fn truncated(text: &str, max_chars: usize) -> String {
    let taken: String = text.chars().take(max_chars).collect();
    if taken.chars().count() < text.chars().count() {
        format!("{taken}…")
    } else {
        taken
    }
}
