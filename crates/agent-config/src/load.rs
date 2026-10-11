//! Reading a configuration from files and the environment.
//!
//! Three layers, later ones winning:
//!
//! 1. the user's file: `$SMOLL_CONFIG`, else `$XDG_CONFIG_HOME/smoll/config.toml`,
//!    else `~/.config/smoll/config.toml`
//! 2. the project's file: `./smoll.toml`, so a repository can carry its own
//!    approval mode and tool policy
//! 3. `SMOLL_*` environment variables, documented in [`crate::load::ENV_VARS`]
//!
//! A missing file is normal and is not an error. A file that exists and is
//! broken is an error, reported with its path: falling back to defaults would
//! mean a typo in a key silently lowered the approval mode.

use serde::Deserialize as _;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::error::{ConfigError, Problem};
use crate::types::{ApprovalMode, Config, ProviderConfig, ProviderKind, SandboxMode};

/// Name of the project-level file, looked for in the working directory.
pub const PROJECT_FILE: &str = "smoll.toml";

/// The environment variables this crate reads, as (name, the key it sets, what
/// it does). `smoll doctor` prints this so the list cannot drift from the code.
pub const ENV_VARS: [(&str, &str, &str); 10] = [
    ("SMOLL_CONFIG", "—", "path to the user configuration file"),
    (
        "SMOLL_APPROVAL_MODE",
        "agent.approval_mode",
        "one of the four approval modes",
    ),
    (
        "SMOLL_SANDBOX",
        "tools.*.sandbox",
        "set the sandbox mode for every tool",
    ),
    (
        "SMOLL_MAX_STEPS",
        "agent.max_steps",
        "cap on tool calls in one task",
    ),
    (
        "SMOLL_MAX_CONTEXT_TOKENS",
        "agent.max_context_tokens",
        "budget for the prompt",
    ),
    (
        "SMOLL_TIMEOUT_SECONDS",
        "agent.timeout_seconds",
        "wall-clock limit for one task",
    ),
    (
        "SMOLL_PROVIDER",
        "agent.provider",
        "which [providers.<name>] to use",
    ),
    (
        "SMOLL_MODEL",
        "providers.<name>.model",
        "override the model for one run",
    ),
    (
        "SMOLL_WORKSPACE_ONLY",
        "privacy.workspace_only",
        "refuse paths outside the repository",
    ),
    (
        "SMOLL_REDACT_SECRETS",
        "privacy.redact_secrets",
        "mask secrets in logs and output",
    ),
];

/// The configuration the agent will use, and where each layer came from.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    /// Files and defaults that contributed, in the order they were applied.
    pub sources: Vec<String>,
    /// Concerns that are real but not fatal, such as a provider whose URL is
    /// not this machine.
    pub warnings: Vec<String>,
}

/// Read from the files on disk and the process environment.
pub fn load(cwd: &Path) -> Result<Loaded, ConfigError> {
    load_with(cwd, &|key| std::env::var(key).ok())
}

/// Read with an injected environment, which is how the tests stay hermetic:
/// `HOME` and `XDG_CONFIG_HOME` are resolved through this closure too, never
/// against the real process environment.
pub fn load_with(cwd: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Loaded, ConfigError> {
    let mut merged = toml::Table::new();
    let mut sources = Vec::new();

    if let Some(path) = user_config_path(env) {
        if path.exists() {
            merge_file(&mut merged, &path)?;
            sources.push(path.display().to_string());
        } else if env("SMOLL_CONFIG").is_some() {
            // SMOLL_CONFIG names a file the operator asked for by hand. If it
            // is not there, saying nothing would let a whole configuration go
            // unread while the agent ran on defaults.
            return Err(ConfigError::Io {
                path,
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "named by SMOLL_CONFIG but not found",
                ),
            });
        }
    }

    let project = cwd.join(PROJECT_FILE);
    if project.exists() {
        merge_file(&mut merged, &project)?;
        sources.push(project.display().to_string());
    }

    let mut config = deserialize(merged, &sources)?;
    if sources.is_empty() {
        sources.push("built-in defaults".to_owned());
    }
    let mut problems = Vec::new();
    apply_env(&mut config, env, &mut problems);
    if !problems.is_empty() {
        return Err(ConfigError::invalid(problems));
    }

    config.ensure_a_provider();
    let warnings = config.validate()?;
    Ok(Loaded {
        config,
        sources,
        warnings,
    })
}

/// Read one configuration out of text a person wrote, with no files and no
/// environment in it.
///
/// `load` is the door a run comes through; this is the door a check comes
/// through — the starter template and the tools that a build registers are both
/// worth comparing against each other, and neither question needs a disk.
pub fn from_toml(text: &str) -> Result<Config, ConfigError> {
    let merged = toml::from_str::<toml::Table>(text).map_err(|source| ConfigError::Shape {
        message: source.to_string(),
        sources: "the text handed in".to_owned(),
    })?;
    let config =
        Config::deserialize(toml::Value::Table(merged)).map_err(|source| ConfigError::Shape {
            message: source.to_string(),
            sources: "the text handed in".to_owned(),
        })?;
    config.validate()?;
    Ok(config)
}

fn deserialize(merged: toml::Table, sources: &[String]) -> Result<Config, ConfigError> {
    let from = if sources.is_empty() {
        "the built-in defaults".to_owned()
    } else {
        sources.join(" + ")
    };
    // Deserialising the merged table directly, rather than re-parsing it as
    // text, keeps an error pointing at a key name instead of at a line number
    // in a document the user never wrote.
    Config::deserialize(toml::Value::Table(merged)).map_err(|source| ConfigError::Shape {
        message: source.to_string(),
        sources: from,
    })
}

/// Where the user's file is expected, if a location can be worked out at all.
pub fn user_config_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(explicit) = env("SMOLL_CONFIG") {
        return Some(PathBuf::from(explicit));
    }
    let config_home = env("XDG_CONFIG_HOME")
        .filter(|value| Path::new(value).is_absolute())
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config_home.join("smoll").join("config.toml"))
}

fn merge_file(merged: &mut toml::Table, path: &Path) -> Result<(), ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let value = toml::from_str::<toml::Value>(&text)
        .map_err(|source| ConfigError::syntax(path, source.to_string()))?;
    match value {
        toml::Value::Table(table) => {
            deep_merge(merged, table);
            Ok(())
        }
        other => Err(ConfigError::syntax(
            path,
            format!("expected a table of settings, found {}", kind_of(&other)),
        )),
    }
}

fn kind_of(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a date",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

/// Overlay `extra` onto `base`, recursing into tables so a project file that
/// sets one key keeps the rest of the user's section.
fn deep_merge(base: &mut toml::Table, extra: toml::Table) {
    for (key, value) in extra {
        match value {
            toml::Value::Table(overlay) => match base.get_mut(&key) {
                Some(toml::Value::Table(existing)) => deep_merge(existing, overlay),
                _ => {
                    base.insert(key, toml::Value::Table(overlay));
                }
            },
            other => {
                base.insert(key, other);
            }
        }
    }
}

fn apply_env(
    config: &mut Config,
    env: &dyn Fn(&str) -> Option<String>,
    problems: &mut Vec<Problem>,
) {
    if let Some(mode) = env_parsed::<ApprovalMode>(env, "SMOLL_APPROVAL_MODE", problems) {
        config.agent.approval_mode = mode;
    }
    if let Some(mode) = env_parsed::<SandboxMode>(env, "SMOLL_SANDBOX", problems) {
        // The whole run at once: the boundary a tool falls back to when its own
        // section says nothing. Setting it here rather than writing a mode into
        // every existing section means the variable still applies to a tool the
        // file never mentions — and to one registered next release.
        config.agent.sandbox = Some(mode);
    }
    if let Some(steps) = env_parsed::<usize>(env, "SMOLL_MAX_STEPS", problems) {
        config.agent.max_steps = steps;
    }
    if let Some(tokens) = env_parsed::<usize>(env, "SMOLL_MAX_CONTEXT_TOKENS", problems) {
        config.agent.max_context_tokens = tokens;
    }
    if let Some(seconds) = env_parsed::<u64>(env, "SMOLL_TIMEOUT_SECONDS", problems) {
        config.agent.timeout_seconds = seconds;
    }
    if let Some(name) = env("SMOLL_PROVIDER") {
        config.agent.provider = Some(name);
    }
    if let Some(model) = env("SMOLL_MODEL") {
        // A one-off run should not have to spell out a whole provider again, so
        // the variable sets the model on whichever provider is in use.
        let name = config
            .agent
            .provider
            .clone()
            .unwrap_or_else(|| "mock".to_owned());
        config
            .providers
            .entry(name)
            .or_insert_with(|| ProviderConfig::new(ProviderKind::Mock))
            .model = Some(model);
    }
    if let Some(flag) = env_bool(env, "SMOLL_WORKSPACE_ONLY", problems) {
        config.privacy.workspace_only = flag;
    }
    if let Some(flag) = env_bool(env, "SMOLL_REDACT_SECRETS", problems) {
        config.privacy.redact_secrets = flag;
    }
    if let Some(flag) = env_bool(env, "SMOLL_TELEMETRY", problems) {
        config.privacy.telemetry = flag;
    }
}

fn env_parsed<T>(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    problems: &mut Vec<Problem>,
) -> Option<T>
where
    T: FromStr,
    T::Err: Display,
{
    let raw = env(name)?;
    match raw.parse::<T>() {
        Ok(value) => Some(value),
        Err(source) => {
            problems.push(Problem::new(name, source.to_string()));
            None
        }
    }
}

fn env_bool(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    problems: &mut Vec<Problem>,
) -> Option<bool> {
    let raw = env(name)?;
    match parse_bool(&raw) {
        Some(value) => Some(value),
        None => {
            problems.push(Problem::new(
                name,
                format!("{raw:?} is not a boolean; expected true, false, 1, 0, yes, no, on or off"),
            ));
            None
        }
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}
