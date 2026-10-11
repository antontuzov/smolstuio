//! What the agent can actually do, as data.
//!
//! A tool is a name, a description the model reads, a JSON Schema its arguments
//! are validated against, an intent the sandbox checks before anything runs, a
//! timeout and an output ceiling. The registry is the same list the CLI prints,
//! the set of schemas a provider embeds in its prompt, and the gate a call
//! passes through on its way to an implementation — so "what this agent is
//! capable of" has exactly one answer.
//!
//! This crate owns the implementations and nothing about the agent's reasoning:
//! a tool never decides whether it should have been called, and cannot widen
//! what it is allowed to touch. Twelve of the twenty-four tools read. Six run a
//! program: `run_command` runs whatever arguments the model wrote, and the five
//! project commands — `test_run`, `check_run`, `lint_run`, `format_run` and
//! `build_run` — run the argv the repository's own manifests declare and take no
//! arguments at all, so a check cannot be steered into a deletion.
//! `format_run` narrows that even
//! further and only reports. The three that change the tree — `write_file`,
//! `patch_file` and `delete_file` — delegate the diff and the rollback to
//! `agent-repo`, and record what they replaced in [`Context::rollback`] so a run
//! that went wrong can be
//! undone. `delete_file` refuses anything that journal could not put back. The
//! two that change what git has recorded — `git_add` and `git_commit` — are
//! writes too, so an approval mode that asks before writing asks before them, and
//! neither is undone by a rollback: they are git's to undo, and this crate offers
//! no git command that discards work. `memory_store` is a write for the same
//! reason, into the one file the workspace keeps for itself, and `smoll rollback`
//! leaves that note where it put it for the next run. The two plan tools change
//! only what a run intends, so they are reads: an agent should not have to be
//! approved to think.
//!
//! ```
//! use agent_tools::{Context, Registry, ToolCall};
//! use serde_json::json;
//!
//! let registry = Registry::with_defaults();
//! let context = Context::new(env!("CARGO_MANIFEST_DIR"));
//! let runtime = tokio::runtime::Runtime::new().expect("a runtime to run one call in");
//!
//! // A path that climbs out of the repository is refused before the filesystem
//! // is touched, and the answer says why.
//! let call = ToolCall::new("read_file", json!({"path": "../../../../etc/passwd"}));
//! let result = runtime.block_on(registry.execute(&call, &context));
//! assert!(!result.is_success());
//! assert!(result.summary().contains("climbs out"), "{}", result.summary());
//!
//! // A tool that does not exist is not an error to retry blindly: the answer
//! // names the ones that do.
//! let call = ToolCall::new("delete_repo", json!({}));
//! let result = runtime.block_on(registry.execute(&call, &context));
//! assert!(result.summary().contains("no tool named"), "{}", result.summary());
//! ```

mod call;
mod context;
mod filesystem;
mod git;
mod memory;
mod plan;
mod registry;
mod schema;
mod search;
mod shell;
mod tool;
mod validate;
mod write;

pub use agent_sandbox::Permission;
pub use call::{ToolCall, ToolOutput, ToolResult, ToolStatus};
pub use context::Context;
pub use memory::{
    count as notes_kept, recent as recent_notes, MemoryRecall, MemoryStore, Note, MAX_NOTE_CHARS,
    MEMORY_FILE,
};
pub use plan::{
    authored, PlanDoc, PlannedStep, StepMark, TaskPlanCreate, TaskPlanUpdate, MAX_PLAN_STEPS,
    MAX_STEP_CHARS,
};
pub use registry::{needs_approval, summarise, Registry, ToolInfo};
pub use tool::{Intent, Tool, ToolDefinition, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_SECONDS};
pub use validate::ProjectCommand;
pub use write::{PatchFile, WriteFile};
