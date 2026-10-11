//! Every way a run can stop, split by whether stopping is worth trying again.
//!
//! A coding agent meets three kinds of problem: the model said something
//! unusable, the machine or provider refused to answer, and the policy said no.
//! They are different variants precisely because they want different handling —
//! a refused action must not be retried, and a provider that timed out should
//! be.

use agent_tools::ToolCall;
use thiserror::Error;

/// Which limit stopped the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Budget {
    #[error("{used} of {max} tool calls used")]
    Steps { used: usize, max: usize },
    /// `used` is the whole thing the window was asked to hold — the prompt and
    /// the room reserved for the answer — so it is the number that lost the
    /// comparison to `max`, not a count of what was spent.
    #[error("{used} of {max} context tokens needed")]
    Tokens { used: usize, max: usize },
    #[error("{elapsed} of {max} seconds elapsed")]
    Seconds { elapsed: u64, max: u64 },
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Config(#[from] agent_config::ConfigError),

    /// The provider would not or could not answer. `retryable` is what the loop
    /// reads: a 429 is worth another try, a model that does not exist is not.
    #[error("provider: {message}")]
    Provider { message: String, retryable: bool },

    /// The provider answered with something that is not a reply: no JSON where a
    /// tool call belongs, an unterminated block, a tool name that is not here.
    #[error("the model's answer could not be read: {message}")]
    MalformedAnswer {
        message: String,
        /// The text that was rejected, kept so a repair attempt can quote it
        /// back to the model.
        raw: String,
    },

    #[error("no tool named {name}")]
    UnknownTool {
        name: String,
        /// Names that do exist, close to the one asked for, because a model that
        /// writes `read_file` is one underscore away from working.
        suggestions: Vec<String>,
    },

    #[error("{tool} was given bad arguments: {message}")]
    InvalidArguments {
        tool: String,
        message: String,
        call: Box<ToolCall>,
    },

    /// A tool ran and the world said no: a test failed, a path does not exist.
    #[error("{tool} failed: {message}")]
    Tool { tool: String, message: String },

    /// The sandbox refused. Retrying cannot help, and the reason has to reach
    /// the transcript so the reader learns what the boundary was.
    #[error("refused by policy: {reason}")]
    Policy { reason: String },

    /// A human said no, which is a decision and not an error, but it does end
    /// the run.
    #[error("rejected by the user: {reason}")]
    Rejected { reason: String },

    #[error(transparent)]
    BudgetExhausted(#[from] Budget),

    #[error("the task ran out of its {seconds} second limit")]
    TimedOut { seconds: u64 },

    #[error("the run was cancelled")]
    Cancelled,

    #[error("the patch does not apply to {path}: {detail}")]
    PatchConflict { path: String, detail: String },

    #[error("cannot read or write {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("json: {source}")]
    Json {
        #[source]
        source: serde_json::Error,
    },
}

impl AgentError {
    /// Whether the loop may ask the provider again. Everything else ends the
    /// run, because an agent that retries a refusal is worse than one that stops.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Provider { retryable, .. } => *retryable,
            Self::MalformedAnswer { .. } | Self::InvalidArguments { .. } => true,
            Self::UnknownTool { .. } => true,
            Self::BudgetExhausted(_) | Self::TimedOut { .. } | Self::Cancelled => false,
            Self::Policy { .. } | Self::Rejected { .. } => false,
            Self::Tool { .. } | Self::PatchConflict { .. } => false,
            Self::Config(_) | Self::Io { .. } | Self::Json { .. } => false,
        }
    }

    /// The exit code `smoll` leaves behind, so a CI job can tell "the agent
    /// could not do this" apart from "the agent should not have been asked".
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) | Self::Policy { .. } | Self::Rejected { .. } => 2,
            _ => 1,
        }
    }

    pub fn budget(&self) -> Option<&Budget> {
        match self {
            Self::BudgetExhausted(budget) => Some(budget),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json { source }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_never_retried() {
        // This is the whole point of the split: an agent that loops on "no" is
        // worse than one that stops and reports.
        let refused = AgentError::Policy {
            reason: "rm -rf is denylisted".to_owned(),
        };
        assert!(!refused.retryable());
        assert_eq!(refused.exit_code(), 2);

        let rejected = AgentError::Rejected {
            reason: "not this way".to_owned(),
        };
        assert!(!rejected.retryable());
        assert_eq!(rejected.exit_code(), 2);
    }

    #[test]
    fn a_provider_that_might_work_is_tried_again() {
        assert!(AgentError::Provider {
            message: "429 too many requests".to_owned(),
            retryable: true
        }
        .retryable());
        assert!(!AgentError::Provider {
            message: "model not found".to_owned(),
            retryable: false
        }
        .retryable());
    }

    #[test]
    fn a_misread_answer_gets_another_chance_and_keeps_its_text() {
        let error = AgentError::MalformedAnswer {
            message: "expected a JSON object".to_owned(),
            raw: "I will now edit the file".to_owned(),
        };
        assert!(error.retryable(), "small models produce this constantly");
        match error {
            AgentError::MalformedAnswer { raw, .. } => assert_eq!(raw, "I will now edit the file"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_budget_says_which_limit_it_was() {
        let steps = AgentError::from(Budget::Steps { used: 21, max: 20 });
        assert_eq!(
            steps.exit_code(),
            1,
            "a budget is not a configuration error"
        );
        assert!(!steps.retryable());
        assert!(
            steps
                .budget()
                .is_some_and(|budget| format!("{budget}") == "21 of 20 tool calls used"),
            "{steps}"
        );
        assert_eq!(
            format!(
                "{}",
                Budget::Seconds {
                    elapsed: 130,
                    max: 120
                }
            ),
            "130 of 120 seconds elapsed"
        );
    }

    #[test]
    fn an_unknown_tool_names_what_was_close() {
        let error = AgentError::UnknownTool {
            name: "read_file".to_owned(),
            suggestions: vec!["fs_read_file".to_owned()],
        };
        assert!(error.to_string().contains("read_file"), "{error}");
        assert!(error.retryable(), "the model can be told the right name");
    }
}
