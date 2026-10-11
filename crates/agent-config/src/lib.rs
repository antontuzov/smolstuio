//! Layered configuration for the agent: a user TOML file, an optional
//! project-level file that can override it, and environment variables on top.
//!
//! Everything the agent can do is decided by this crate's [`Config`], so that
//! the loop, the tools and the sandbox never read a file or an environment
//! variable themselves — they are handed one validated value. Loading is
//! forgiving about a missing file and strict about a broken one: a config that
//! cannot be parsed is reported with its path and the offending key rather than
//! silently falling back to defaults, because a silently ignored `sandbox =
//! "strict"` is a security failure.
//!
//! ```no_run
//! use std::path::Path;
//!
//! let loaded = agent_config::load(Path::new(".")).expect("a valid configuration");
//! println!("{} {}", loaded.config.agent.approval_mode, loaded.warnings.len());
//! ```

mod error;
mod load;
mod starter;
mod types;

pub use error::{ConfigError, Problem};
pub use load::{from_toml, load, load_with, Loaded, ENV_VARS, PROJECT_FILE};
pub use starter::{starter_toml, write_starter};
pub use types::{
    AgentSection, ApprovalMode, Config, PrivacySection, ProviderConfig, ProviderKind, SandboxMode,
    Theme, ToolPolicy, UiSection,
};

pub use load::user_config_path;
