//! The loop, and the vocabulary the rest of the agent shares.
//!
//! Task in, context built, plan proposed, tool calls validated and executed,
//! results observed, plan revised, diff proposed, approval asked, change
//! applied, validation commands run, outcome summarised — with a step limit, a
//! token budget, a wall-clock timeout and an event stream a CLI or TUI can
//! render without knowing anything about the reasoning behind it.
//!
//! This crate holds the types (`Task`, `Plan`, `ToolCall`, `AgentEvent`,
//! `Session`, `Outcome`) and the orchestration. It calls providers and tools
//! through their traits, so a run against a scripted mock and a run against a
//! 1.5B GGUF on a laptop go through the same code.

mod agent;
mod approve;
mod error;
mod prompt;
mod session;
mod types;

pub use agent::{Agent, Limits, Observer, Run};
pub use agent_sandbox::Permission;
pub use agent_tools::{ToolCall, ToolOutput, ToolResult, ToolStatus};
pub use approve::{Approver, AutoApprove, RefuseAll, Scripted};
pub use error::{AgentError, Budget};
pub use session::{StoredSession, SESSIONS_DIR};
pub use types::{
    now_ms, AgentEvent, ApprovalDecision, ApprovalRequest, ContextBundle, ContextFile, Outcome,
    Plan, PlanStep, Session, StepStatus,
};
