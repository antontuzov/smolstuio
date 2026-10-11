//! The list of what this agent can do, and the gate every call passes through.
//!
//! One object holds both, on purpose. The registry is what builds the tool list
//! that goes into a prompt, what `smoll tools` prints, and the only way to run a
//! tool at all — so "what is this agent capable of" and "what did it just do"
//! cannot drift apart, and a caller cannot reach an implementation without
//! passing the policy that guards it.
//!
//! The order [`Registry::execute`] follows is the contract:
//!
//! 1. is the tool known and enabled,
//! 2. what does this call intend to touch,
//! 3. what does the policy say about that, and has a person already answered it,
//! 4. only then: run it, with a timeout, an output ceiling and a redaction pass,
//! 5. and write down what was decided, including the calls that never ran.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use agent_sandbox::{redact, Action, Decision, Entry, Permission, Policy};
use serde::{Deserialize, Serialize};

use crate::call::{ToolCall, ToolOutput, ToolResult, ToolStatus};
use crate::context::Context;
use crate::tool::{Intent, Tool, ToolDefinition};

/// What one tool is, as a row in a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub permission: Permission,
    pub timeout_seconds: u64,
    pub max_output_bytes: usize,
    /// Roughly how many prompt tokens offering it costs, on every step.
    pub token_cost: usize,
    pub enabled: bool,
}

/// The tools this build ships with.
#[derive(Default)]
pub struct Registry {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The nine tools a first run can use, all of them enabled.
    pub fn with_defaults() -> Self {
        use crate::filesystem::{ListDir, ReadFile};
        use crate::git::{GitDiff, GitStatus};
        use crate::search::{SearchFiles, SearchText};
        use crate::shell::RunCommand;
        use crate::write::{PatchFile, WriteFile};

        let mut registry = Self::new();
        for tool in [
            Box::new(ReadFile) as Box<dyn Tool>,
            Box::new(ListDir),
            Box::new(SearchText),
            Box::new(SearchFiles),
            Box::new(GitStatus),
            Box::new(GitDiff),
            Box::new(RunCommand),
            Box::new(WriteFile),
            Box::new(PatchFile),
        ] {
            registry.register(tool);
        }
        registry
    }

    /// The default set with every tool the configuration disables left out.
    ///
    /// A disabled tool is not merely refused: it is not offered, so the model
    /// never spends a step asking for something this build will not do.
    pub fn from_config(config: &agent_config::Config) -> Self {
        Self::with_defaults().enabled_by(&Policy::from_config(Path::new("."), config))
    }

    /// Drop the tools a policy switches off.
    pub fn enabled_by(mut self, policy: &Policy) -> Self {
        self.tools.retain(|name, _| policy.is_enabled(name));
        self
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) -> &mut Self {
        let name = tool.definition().name;
        self.tools.insert(name, tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(Box::as_ref)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    /// The rows `smoll tools` prints, and the JSON behind `--json`.
    pub fn inventory(&self, policy: &Policy) -> Vec<ToolInfo> {
        self.tools
            .values()
            .map(|tool| {
                let definition = tool.definition();
                ToolInfo {
                    token_cost: definition.token_cost(),
                    timeout_seconds: policy
                        .timeout_for(&definition.name, definition.timeout)
                        .as_secs(),
                    max_output_bytes: policy
                        .max_output_for(&definition.name, definition.max_output_bytes),
                    enabled: policy.is_enabled(&definition.name),
                    name: definition.name,
                    description: definition.description,
                    permission: definition.permission,
                }
            })
            .collect()
    }

    /// The total prompt cost of offering this list, which is why an agent for
    /// small models exposes fewer tools than it could.
    pub fn token_cost(&self) -> usize {
        self.tools
            .values()
            .map(|tool| tool.definition().token_cost())
            .sum()
    }

    /// Run one call, or explain in a form the model can act on why it did not.
    pub async fn execute(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let Some(tool) = self.get(&call.name) else {
            return ToolResult::refused(
                call.clone(),
                Permission::ReadOnly,
                format!(
                    "there is no tool named {}; this build offers: {}",
                    call.name,
                    self.names().join(", ")
                ),
            );
        };
        let definition = tool.definition();
        if !ctx.policy.is_enabled(&definition.name) {
            return ToolResult::refused(
                call.clone(),
                definition.permission,
                format!("{} is disabled in the configuration", definition.name),
            );
        }
        let intent = match tool.intent(call, ctx) {
            Ok(intent) => intent,
            Err(problem) => {
                return ToolResult::failed(
                    call.clone(),
                    definition.permission,
                    problem,
                    ToolOutput::empty(),
                )
            }
        };
        let permission = intent.permission(definition.permission);
        let decision = ctx
            .policy
            .check(&action(&definition.name, permission, &intent));
        if decision.blocked() {
            let result = ToolResult::refused(call.clone(), permission, reason(&decision));
            record(ctx, &result, &intent, &decision);
            return result;
        }
        if decision.needs_approval() && !ctx.approved(permission, intent.subject().as_deref()) {
            let result = ToolResult::awaiting(
                call.clone(),
                permission,
                format!("{} asks: {}", definition.name, question(&intent)),
            );
            record(ctx, &result, &intent, &decision);
            return result;
        }
        // The policy asked and a person answered, so the line in the log says
        // what resolved the question rather than leaving it open.
        let decision = if decision.needs_approval() {
            Decision::Warn {
                reason: "a person approved this".to_owned(),
            }
        } else {
            decision
        };

        let limit = ctx.policy.timeout_for(&definition.name, definition.timeout);
        let started = Instant::now();
        let mut result = match tokio::time::timeout(limit, tool.run(call, ctx)).await {
            Ok(result) => result,
            Err(_) => ToolResult::failed(
                call.clone(),
                permission,
                format!("{} timed out after {}s", definition.name, limit.as_secs()),
                ToolOutput::empty(),
            ),
        };
        result.duration_ms = started.elapsed().as_millis() as u64;
        let ceiling = ctx
            .policy
            .max_output_for(&definition.name, definition.max_output_bytes);
        if result.output.text.len() > ceiling {
            let text = std::mem::take(&mut result.output.text);
            result.output = ToolOutput::capped(text, ceiling);
        }
        if ctx.redact_secrets {
            let (cleaned, report) = redact(&result.output.text);
            if !report.nothing() {
                result.output.text = cleaned;
                result.redactions = report.masked;
            }
        }
        record(ctx, &result, &intent, &decision);
        result
    }

    /// Several calls from one step, in the order the model asked for them.
    ///
    /// Sequential on purpose: a small model that emits three calls usually means
    /// them as one thought, and running them concurrently would let the third
    /// read a file the first is still writing.
    pub async fn dispatch(&self, calls: &[ToolCall], ctx: &Context) -> Vec<ToolResult> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            results.push(self.execute(call, ctx).await);
        }
        results
    }
}

fn action<'a>(name: &'a str, permission: Permission, intent: &'a Intent) -> Action<'a> {
    let mut action = Action::new(name, permission);
    action.paths = intent.paths.clone();
    if let Some(argv) = &intent.command {
        action = action.command(argv);
    }
    action
}

fn reason(decision: &Decision) -> String {
    match decision {
        Decision::Allow => "the policy allowed it".to_owned(),
        Decision::Warn { reason } | Decision::Ask { reason } | Decision::Block { reason } => {
            reason.clone()
        }
    }
}

/// What a human is shown, which names the thing about to happen rather than the
/// rule that stopped it.
fn question(intent: &Intent) -> String {
    if let Some(argv) = &intent.command {
        return format!("run `{}`?", argv.join(" "));
    }
    match intent.subject() {
        Some(subject) => format!("change {subject}?"),
        None => "do something the policy cannot describe?".to_owned(),
    }
}

fn record(ctx: &Context, result: &ToolResult, intent: &Intent, decision: &Decision) {
    if !ctx.audit.is_enabled() {
        return;
    }
    let entry = Entry::new(
        result.call.name.clone(),
        result.permission,
        decision.clone(),
        intent.subject(),
    )
    .with_timing(result.duration_ms, result.output.full_bytes);
    // A log that cannot be written is worth knowing about and not worth dying
    // over: by this point the action has already happened.
    if let Err(source) = ctx.audit.record(&entry) {
        tracing::warn!(%source, tool = %result.call.name, "the audit log could not be written");
    }
}

/// Whether a result says the run has to stop and ask a person.
pub fn needs_approval(result: &ToolResult) -> bool {
    matches!(result.status, ToolStatus::AwaitingApproval)
}

/// Several results as one line, for a transcript or a status bar.
pub fn summarise(results: &[ToolResult]) -> String {
    results
        .iter()
        .map(ToolResult::summary)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_registry_offers_nothing_and_costs_nothing() {
        let registry = Registry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.token_cost(), 0);
        assert!(registry.names().is_empty());
    }

    #[test]
    fn the_default_set_is_the_nine_tools_a_first_run_can_use() {
        let registry = Registry::with_defaults();
        assert_eq!(
            registry.names(),
            vec![
                "git_diff",
                "git_status",
                "list_dir",
                "patch_file",
                "read_file",
                "run_command",
                "search_files",
                "search_text",
                "write_file",
            ]
        );
        let cost = registry.token_cost();
        assert!(cost > 0, "offering a tool costs prompt tokens");
        assert!(
            cost < 3_000,
            "the whole list is {cost} tokens of the window"
        );
    }

    /// A `[tools.<name>]` section for a name nobody registers is not a setting
    /// that is switched off: it is a setting that does not exist, which is the
    /// worse way to read a file whose denylist is supposed to be what keeps
    /// `rm -rf` out of a run.
    #[test]
    fn the_starter_template_gates_tools_that_exist() {
        let config = agent_config::from_toml(agent_config::starter_toml())
            .expect("the template `smoll init` writes is a valid configuration");
        let known = Registry::with_defaults().names();
        assert!(
            !config.tools.is_empty(),
            "the template is supposed to show the shape"
        );
        let orphaned: Vec<&str> = config
            .tools
            .keys()
            .filter(|name| !known.iter().any(|tool| tool == *name))
            .map(String::as_str)
            .collect();
        assert!(orphaned.is_empty(), "{orphaned:?} gate nothing");
    }

    #[tokio::test]
    async fn a_question_a_person_has_already_answered_is_not_asked_twice() {
        let dir = tempfile::tempdir().expect("a temp repo");
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("writable");
        let registry = Registry::with_defaults();
        let ctx = Context::new(dir.path());
        let call = ToolCall::new(
            "write_file",
            json!({"path": "a.rs", "content": "fn b() {}\n"}),
        );

        // The default approval mode asks about a write.
        let first = registry.execute(&call, &ctx).await;
        assert!(needs_approval(&first), "{}", first.summary());

        ctx.approve(Permission::Write, None, true);
        let second = registry.execute(&call, &ctx).await;
        assert!(second.is_success(), "{}", second.summary());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).expect("readable"),
            "fn b() {}\n"
        );
    }

    #[tokio::test]
    async fn a_masked_secret_is_counted_on_its_way_out() {
        let dir = tempfile::tempdir().expect("a temp repo");
        std::fs::write(
            dir.path().join("creds.toml"),
            "api_token = \"hf_AAAAAAAAAAAAAAAAAAAAAAAA\"\n",
        )
        .expect("writable");
        let ctx = Context::new(dir.path());
        let result = Registry::with_defaults()
            .execute(
                &ToolCall::new("read_file", json!({"path": "creds.toml"})),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            !result.output.text.contains("AAAA"),
            "the credential did not reach the caller: {}",
            result.output.text
        );
        assert!(result.redactions > 0, "and the run knows it hid one");
    }

    #[test]
    fn a_disabled_tool_is_not_offered_at_all() {
        let mut config = agent_config::Config::default();
        config.tools.insert(
            "run_command".to_owned(),
            agent_config::ToolPolicy {
                enabled: false,
                ..agent_config::ToolPolicy::default()
            },
        );
        let registry = Registry::from_config(&config);
        let names = registry.names();
        assert!(!names.contains(&"run_command".to_owned()));
        assert!(names.contains(&"read_file".to_owned()));
    }

    #[test]
    fn an_inventory_row_names_the_limits_that_will_be_applied() {
        let mut config = agent_config::Config::default();
        config.tools.insert(
            "read_file".to_owned(),
            agent_config::ToolPolicy {
                max_output_bytes: Some(512),
                timeout_seconds: Some(3),
                ..agent_config::ToolPolicy::default()
            },
        );
        let policy = Policy::from_config(Path::new("."), &config);
        let registry = Registry::with_defaults();
        let row = registry
            .inventory(&policy)
            .into_iter()
            .find(|row| row.name == "read_file")
            .expect("read_file is in the default set");
        assert_eq!(row.max_output_bytes, 512);
        assert_eq!(row.timeout_seconds, 3);
        assert!(row.enabled);
        assert_eq!(row.permission, Permission::ReadOnly);
    }

    #[test]
    fn an_intent_with_no_path_and_no_command_has_no_subject() {
        assert_eq!(Intent::default().subject(), None);
        assert_eq!(
            question(&Intent::default()),
            "do something the policy cannot describe?"
        );
    }
}
